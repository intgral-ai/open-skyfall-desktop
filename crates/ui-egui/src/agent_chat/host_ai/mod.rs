//! Native model-call resources, connection identity and immutable-lifetime controls.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, Weak},
    time::Duration,
};

use super::backend::{HttpFn, Secrets};

mod call;
mod connections;
pub(super) mod continuation;
pub(super) mod go;
pub(super) mod messages;
pub(super) mod provider;
mod types;

pub use call::{CallControl, HttpAdmission, ModelCallBudget, TurnControl};
pub use connections::{ConnectionBinding, ConnectionCatalog};
pub use types::{
    AiCapabilities, AiError, AiErrorCode, AiErrorDetails, AiLimits, ArtifactReadFn, ArtifactRef, CallContext, CallScope, ClockFn,
    ConnectionDescription, ConnectionState, ContentPart, ContinuationId, Delivery, FinishReason, GenerateOptions, GenerateRequest, IdFn, MessageRole,
    ModelMessage, ModelReply, SessionKey, ToolCall, ToolDefinition, Usage,
};

/// Each window has its own catalog but can share the process's physical limiter.
pub struct AiShared {
    connections: ConnectionCatalog,
    http: Arc<HttpAdmission>,
}

impl Default for AiShared {
    fn default() -> Self {
        Self::new(Arc::new(HttpAdmission::default()))
    }
}

impl AiShared {
    pub fn new(http: Arc<HttpAdmission>) -> Self {
        Self { connections: ConnectionCatalog::default(), http }
    }

    pub fn connections(&self) -> &ConnectionCatalog {
        &self.connections
    }
}

/// All external work is injected. Construction and catalog publication do no I/O.
pub struct HostAiDependencies {
    pub shared: Arc<AiShared>,
    pub http: HttpFn,
    pub secrets: Option<Arc<dyn Secrets>>,
    pub artifacts: ArtifactReadFn,
    pub clock: ClockFn,
    pub ids: IdFn,
}

/// One-step normalized facade, sharing resources with the native direct-chat adapter.
pub struct HostAi {
    deps: HostAiDependencies,
    state: Mutex<HostState>,
}

#[derive(Default)]
struct HostState {
    continuations: continuation::ContinuationStore,
    sessions: HashMap<SessionKey, LiveSession>,
    calls: Vec<RegisteredCall>,
}

struct LiveSession {
    token: Arc<()>,
    limits: AiLimits,
    pins: Vec<ScopePin>,
}

struct RegisteredCall {
    lifetime: Weak<()>,
    control: Weak<CallControl>,
    session: Weak<()>,
}

struct ScopePin {
    scope: CallScope,
    budget: Weak<ModelCallBudget>,
    binding: ConnectionBinding,
}

impl HostAi {
    pub fn new(deps: HostAiDependencies) -> Self {
        Self { deps, state: Mutex::default() }
    }

    /// Direct chat already owns native prompt attachments; it cannot resolve runtime artifacts.
    pub(crate) fn direct(shared: Arc<AiShared>, http: Option<HttpFn>, secrets: Option<Arc<dyn Secrets>>) -> Result<Arc<Self>, AiError> {
        let http = http.ok_or_else(|| AiError::internal("http_unavailable"))?;
        let origin = std::time::Instant::now();
        Ok(Arc::new(Self::new(HostAiDependencies {
            shared,
            http,
            secrets,
            artifacts: Arc::new(|_, _| Err(AiError::unsupported("artifacts"))),
            clock: Arc::new(move || origin.elapsed()),
            ids: Arc::new(continuation::random_id),
        })))
    }

    pub(crate) fn bind(&self, connection_id: &str, model_id: &str) -> Result<ConnectionBinding, AiError> {
        let binding = self.deps.shared.connections.bind(connection_id, model_id)?;
        let current = self.deps.shared.connections.resolve(&binding)?;
        if current.configuration().key && !super::agents::key_safe_for(&current.configuration().base_url) {
            return Err(AiError::invalid("base_url"));
        }
        Ok(binding)
    }

    pub(crate) fn check_binding(&self, binding: &ConnectionBinding) -> Result<(), AiError> {
        self.deps.shared.connections.resolve(binding).map(|_| ())
    }

    /// Explicit saved-connection Test action; never a hidden describe/generate probe.
    pub(crate) fn test_connection(&self, binding: &ConnectionBinding) -> Result<String, AiError> {
        if binding.protocol() == super::agents::Protocol::OpenAi {
            let models = self.list_models(binding)?;
            let found = models.iter().any(|model| model.id == binding.model_id());
            return Ok(crate::i18n::format(
                if found { "ui.ai_test_ok" } else { "ui.ai_test_no_model" },
                &[("count", &models.len().to_string()), ("model", binding.model_id())],
            ));
        }
        let control = TurnControl::new().new_call()?;
        let timeout = binding.configuration().request_timeout_s.min(60);
        self.invoke_once(
            binding,
            provider::ProviderCall {
                wire: provider::WireProtocol::Messages,
                system: String::new(),
                history: vec![serde_json::json!({"role":"user","content":"Hello"})],
                tools: vec![],
                max_output_tokens: Some(16),
                temperature: None,
                reasoning_effort: None,
                timeout_seconds: timeout,
                http_per_connection: 2,
                http_per_host: 8,
                model_budget: None,
                private: provider::ProviderState::default(),
            },
            &control,
            self.deadline_after(Duration::from_secs(timeout)),
        )?;
        control.publish(|| {
            self.check_binding(binding)?;
            Ok(crate::i18n::format("ui.ai_test_answered", &[("model", binding.model_id())]))
        })
    }

    pub fn open_session(&self, session: SessionKey, limits: AiLimits) -> Result<(), AiError> {
        let result = (|| {
            let mut state = self.lock_state()?;
            state.continuations.open_session(session.clone(), limits.clone())?;
            state.sessions.insert(session, LiveSession { token: Arc::new(()), limits, pins: Vec::new() });
            Ok(())
        })();
        self.fail_closed_if_poisoned();
        result
    }

    /// Worker only. Current credential availability is not ACP/runtime authentication,
    /// and neither capability inspection nor this key lookup probes a model endpoint.
    pub fn describe(&self, binding: &ConnectionBinding) -> Result<ConnectionDescription, AiError> {
        let connection = self.deps.shared.connections.resolve(binding)?;
        let key = connections::resolve_secret(&connection, self.deps.secrets.as_deref());
        self.deps.shared.connections.resolve(binding)?;
        let state = match key {
            Ok(_) => ConnectionState::Available,
            Err(error) if error.code() == AiErrorCode::AuthRequired => ConnectionState::AuthRequired,
            Err(error) => return Err(error),
        };
        Ok(ConnectionDescription { state, capabilities: provider::capabilities(binding) })
    }

    /// Worker only. A domain proposal is not #5's authoritative wire/turn completion.
    pub fn generate(&self, binding: &ConnectionBinding, context: &CallContext, request: GenerateRequest) -> Result<ModelReply, AiError> {
        let lifetime = Arc::new(());
        let token = match self.claim_call(context, &lifetime) {
            Ok(token) => token,
            Err(error) => {
                // No claim, no clock/check/cancel of somebody else's control.
                // Poison remains a host-wide fail-closed event, not a duplicate.
                self.fail_closed_if_poisoned();
                return Err(error);
            }
        };
        let result = self.generate_inner(binding, context, request, &token);
        // No store/publication guard is alive here, including poison/error paths.
        // A failed lifetime cannot later be reused after reset under the same key.
        if result.is_err() {
            context.control.cancel();
        }
        // Keep the unique claim through terminal publication/failure cleanup.
        // A later claimant can only encounter an irrevocably terminal control.
        drop(lifetime);
        self.fail_closed_if_poisoned();
        if let Ok(mut state) = self.lock_state() {
            state.calls.retain(|call| call.lifetime.strong_count() > 0);
            if let Some(session) = state.sessions.get_mut(&context.scope.session) {
                session.pins.retain(|pin| pin.budget.strong_count() > 0);
            }
        }
        result
    }

    /// One store-lock transition both tests uniqueness and installs ownership.
    /// Never inspect/mutate a control or invoke external code while holding it.
    fn claim_call(&self, context: &CallContext, lifetime: &Arc<()>) -> Result<Arc<()>, AiError> {
        let mut state = self.lock_state()?;
        state.calls.retain(|call| call.lifetime.strong_count() > 0);
        if state.calls.iter().any(|call| std::ptr::eq(call.control.as_ptr(), Arc::as_ptr(&context.control))) {
            return Err(AiError::invalid("call_control"));
        }
        context.limits.validate()?;
        let session = state.sessions.get(&context.scope.session).ok_or_else(|| AiError::invalid("session"))?;
        let per_session = session.limits.http_per_host.min(context.limits.http_per_host);
        let active = state.calls.iter().filter(|call| std::ptr::eq(call.session.as_ptr(), Arc::as_ptr(&session.token))).count();
        if active >= per_session as usize {
            return Err(AiError::exhausted("active_calls", per_session));
        }
        // Bound the same weak registry even when retired owners are paused outside
        // locks. Retirement cannot erase a still-live claim before cancelling it.
        let per_host = AiLimits::contract_defaults().http_per_host;
        if state.calls.len() >= per_host as usize {
            return Err(AiError::exhausted("active_calls", per_host));
        }
        let token = Arc::clone(&session.token);
        state.calls.push(RegisteredCall {
            lifetime: Arc::downgrade(lifetime),
            control: Arc::downgrade(&context.control),
            session: Arc::downgrade(&token),
        });
        Ok(token)
    }

    fn generate_inner(
        &self,
        binding: &ConnectionBinding,
        context: &CallContext,
        request: GenerateRequest,
        token: &Arc<()>,
    ) -> Result<ModelReply, AiError> {
        let now = (self.deps.clock)();
        context.control.check(now, context.deadline)?;
        context.limits.validate()?;
        if !messages::valid_id(&context.scope.turn_id) || !messages::valid_id(&context.scope.workflow_run_id) {
            return Err(AiError::invalid("scope"));
        }
        self.deps.shared.connections.resolve(binding)?;
        let (request, lease, limits) = {
            let mut state = self.lock_state()?;
            let session = state.sessions.get(&context.scope.session).ok_or_else(|| AiError::invalid("session"))?;
            if !Arc::ptr_eq(&session.token, token) {
                return Err(AiError::cancelled());
            }
            let limits = session.limits.intersection(&context.limits);
            // Storage negotiation belongs to open_session. Do not silently promise a
            // stricter per-call storage quota while charging a different generation.
            if limits.continuation_bytes != session.limits.continuation_bytes
                || limits.continuation_handles != session.limits.continuation_handles
                || limits.continuation_ttl_seconds != session.limits.continuation_ttl_seconds
            {
                return Err(AiError::invalid("limits"));
            }
            self.pin_scope(&mut state, binding, context)?;
            let request = state.continuations.own_request(&context.scope.session, binding, request, &limits)?;
            let lease = state.continuations.begin(&context.scope.session, binding, request.request(), now)?;
            (request, lease, limits)
        };
        // Artifact/secret/HTTP/ID/clock callbacks never hold the store or control gate.
        let mut call =
            messages::prepare_messages(&context.scope.session, binding, request.request(), &limits, &self.deps.artifacts, lease.private())?;
        let now = (self.deps.clock)();
        context.control.check(now, context.deadline)?;
        let seconds = u64::from(request.request().options.timeout_seconds)
            .min(binding.configuration().request_timeout_s)
            .min(u64::from(limits.call_max_seconds))
            .min(600)
            .min(context.deadline.saturating_sub(now).as_secs());
        if seconds == 0 {
            return Err(AiError::timeout());
        }
        let deadline = context.deadline.min(now.saturating_add(Duration::from_secs(seconds)));
        call.timeout_seconds = seconds;
        call.model_budget = Some(Arc::clone(&context.model_budget));
        let proposal = provider::project_strict_reply(self.invoke_once(binding, call, &context.control, deadline)?)?;
        Self::validate_reply_limits(&proposal, &limits)?;
        let prepared = continuation::prepare_continuation(lease, request.request(), &proposal, &self.deps.ids);
        // The store's final shrink is valid only after these transient owners die.
        drop(proposal);
        drop(request);
        let now = (self.deps.clock)();
        context.control.check(now, deadline)?;
        let prepared = prepared?;
        context.control.publish(|| {
            // No injected callback in this closure. The control gate owns cancellation;
            // this very store lock owns retirement, generation and continuation commit.
            self.deps.shared.connections.resolve(binding)?;
            let mut state = self.lock_state()?;
            let session = state.sessions.get(&context.scope.session).ok_or_else(AiError::cancelled)?;
            if !Arc::ptr_eq(&session.token, token) {
                return Err(AiError::cancelled());
            }
            if now >= deadline {
                return Err(AiError::timeout());
            }
            state.continuations.commit(prepared, now).map(|(reply, _)| reply)
        })
    }

    fn pin_scope(&self, state: &mut HostState, binding: &ConnectionBinding, context: &CallContext) -> Result<(), AiError> {
        let mut present = false;
        for session in state.sessions.values_mut() {
            session.pins.retain(|pin| pin.budget.strong_count() > 0);
            for pin in &session.pins {
                let same_budget = std::ptr::eq(pin.budget.as_ptr(), Arc::as_ptr(&context.model_budget));
                let same_turn = pin.scope.session == context.scope.session && pin.scope.turn_id == context.scope.turn_id;
                if same_budget || same_turn {
                    // Rebinding the same ID/model must not replace this turn's
                    // immutable invocation generation. Credentials remain live.
                    self.deps.shared.connections.resolve(&pin.binding)?;
                    if !same_budget
                        || pin.scope != context.scope
                        || pin.binding.connection_id() != binding.connection_id()
                        || pin.binding.model_id() != binding.model_id()
                    {
                        return Err(AiError::invalid("scope"));
                    }
                    present = true;
                }
            }
        }
        if !present {
            let session = state.sessions.get_mut(&context.scope.session).ok_or_else(|| AiError::invalid("session"))?;
            if session.pins.len() >= session.limits.model_messages as usize {
                return Err(AiError::exhausted("scope_pins", session.limits.model_messages));
            }
            session.pins.push(ScopePin { scope: context.scope.clone(), budget: Arc::downgrade(&context.model_budget), binding: binding.clone() });
        }
        Ok(())
    }

    fn validate_reply_limits(reply: &provider::ProviderReply, limits: &AiLimits) -> Result<(), AiError> {
        if reply.calls.len() > limits.tool_calls as usize {
            return Err(AiError::exhausted("tool_calls", limits.tool_calls));
        }
        if reply.text.iter().any(|(_, text)| text.len() > limits.text_bytes as usize) {
            return Err(AiError::exhausted("text_bytes", limits.text_bytes));
        }
        // Allocation-free serialization, not a second charging workspace/quota.
        // The provider already bounded depth/shape; Task6 charges owned candidates.
        struct BoundedBytes(usize);
        impl std::io::Write for BoundedBytes {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 = self.0.checked_sub(bytes.len()).ok_or_else(|| std::io::Error::other("limit"))?;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        for call in &reply.calls {
            let arguments = call.arguments.as_ref().map_err(|_| AiError::malformed())?;
            serde_json::to_writer(BoundedBytes(limits.tool_json_bytes as usize), arguments)
                .map_err(|_| AiError::exhausted("tool_json_bytes", limits.tool_json_bytes))?;
        }
        Ok(())
    }

    pub fn retire_session(&self, session: &SessionKey) -> Result<(), AiError> {
        let result = (|| {
            let calls = {
                let mut state = self.lock_state()?;
                state.continuations.retire(session);
                state
                    .sessions
                    .remove(session)
                    .map(|session| {
                        state
                            .calls
                            .iter()
                            .filter(|call| std::ptr::eq(call.session.as_ptr(), Arc::as_ptr(&session.token)))
                            .filter_map(|call| call.control.upgrade())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            };
            for call in calls {
                call.cancel();
            }
            Ok(())
        })();
        self.fail_closed_if_poisoned();
        result
    }

    fn lock_state(&self) -> Result<MutexGuard<'_, HostState>, AiError> {
        self.state.lock().map_err(|poison| {
            // PoisonError owns a guard: drop it BEFORE any fail-closed re-entry.
            drop(poison.into_inner());
            AiError::internal("ai_sessions")
        })
    }

    fn fail_closed_if_poisoned(&self) {
        if !self.state.is_poisoned() {
            return;
        }
        let calls = {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(poison) => poison.into_inner(),
            };
            let sessions = std::mem::take(&mut state.sessions);
            for key in sessions.keys() {
                state.continuations.retire(key);
            }
            state.calls.iter().filter_map(|call| call.control.upgrade()).collect::<Vec<_>>()
        };
        for call in calls {
            call.cancel();
        }
    }

    pub(crate) fn deadline_after(&self, timeout: Duration) -> Duration {
        (self.deps.clock)().saturating_add(timeout)
    }

    pub(crate) fn invoke_once(
        &self,
        binding: &ConnectionBinding,
        call: provider::ProviderCall,
        control: &Arc<CallControl>,
        deadline: Duration,
    ) -> Result<provider::ProviderReply, AiError> {
        provider::invoke_once(&self.deps, binding, call, control, deadline)
    }

    /// Explicit worker-only picker operation. Reuse the existing GET codec/transport;
    /// no model budget, retry, probe, or separate background HTTP runner is introduced.
    pub(crate) fn list_models(&self, binding: &ConnectionBinding) -> Result<Vec<super::acp::ModelInfo>, AiError> {
        let connection = self.deps.shared.connections.resolve(binding)?;
        let _permit = self.deps.shared.http.try_acquire(binding.connection_id(), 2, 8)?;
        let key = connections::resolve_secret(&connection, self.deps.secrets.as_deref())?;
        let current = self.deps.shared.connections.resolve(binding)?;
        let key = if current.configuration().key { key } else { None };
        if current.configuration().key && key.is_none() {
            return Err(AiError::auth_required());
        }
        let key_text = key.as_ref().map(|key| key.as_str());
        let models = match binding.protocol() {
            super::agents::Protocol::OpenAi => super::api_client::model_list(&self.deps.http, &current.configuration().base_url, key_text),
            super::agents::Protocol::Anthropic => {
                super::api_client::anthropic_model_list(&self.deps.http, &current.configuration().base_url, key_text)
            }
        }
        .map_err(|error| {
            // These codecs already sanitize errors; retain their bounded HTTP status without
            // retaining the string, raw provider body or arbitrary transport diagnostics.
            match error.strip_prefix("HTTP ").and_then(|s| s.split_once(':')).and_then(|(s, _)| s.parse::<u16>().ok()) {
                Some(status) if (100..=599).contains(&status) => AiError::provider_status(binding.connection_id(), status, status == 401),
                _ => AiError::model_failed(),
            }
        })?;
        if key_text
            .is_some_and(|key| models.iter().any(|model| model.id.contains(key) || model.name.contains(key) || model.description.contains(key)))
        {
            return Err(AiError::malformed());
        }
        self.deps.shared.connections.resolve(binding)?;
        Ok(models)
    }
}

#[cfg(test)]
pub(crate) use connections::test_support;
