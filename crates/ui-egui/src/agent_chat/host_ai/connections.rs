//! Original preference identity, frozen invocation settings and per-attempt keys.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use super::super::agents::{self, AgentConfig, Protocol};
use super::super::backend::Secrets;
use super::AiError;

/// No network/keychain access. Deleted entries are dropped, not retained forever.
#[derive(Default)]
pub struct ConnectionCatalog {
    source: Arc<()>,
    state: Mutex<CatalogState>,
}

#[derive(Default)]
struct CatalogState {
    // A compact tombstone high-water mark shared by all IDs in this catalog.
    // Never reset it when the last entry is removed.
    generation: u64,
    entries: HashMap<String, Entry>,
}

struct Entry {
    generation: u64,
    configuration: Result<AgentConfig, AiError>,
}

/// A turn's selected model and invocation configuration, never its credentials.
#[derive(Clone)]
pub struct ConnectionBinding {
    source: Arc<()>,
    generation: u64,
    configuration: AgentConfig,
    protocol: Protocol,
}

impl fmt::Debug for ConnectionBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Do not dump endpoint, display text or any future credential-bearing field.
        f.debug_struct("ConnectionBinding")
            .field("connection_id", &self.connection_id())
            .field("model_id", &self.model_id())
            .field("protocol", &self.protocol)
            .finish_non_exhaustive()
    }
}

impl ConnectionBinding {
    pub fn connection_id(&self) -> &str {
        &self.configuration.id
    }

    pub fn model_id(&self) -> &str {
        &self.configuration.model
    }

    pub fn protocol(&self) -> Protocol {
        self.protocol
    }

    pub(crate) fn configuration(&self) -> &AgentConfig {
        &self.configuration
    }
}

/// One chosen live configuration, cloned with no catalog lock held over external work.
pub(crate) struct ResolvedConnection {
    configuration: AgentConfig,
}

impl ResolvedConnection {
    pub(super) fn configuration(&self) -> &AgentConfig {
        &self.configuration
    }
}

impl ConnectionCatalog {
    /// Publish one complete preference view atomically, retaining bad entries as refusals.
    pub fn replace(&self, configs: &[AgentConfig]) -> Result<(), AiError> {
        let mut prepared = HashMap::new();
        for config in configs {
            let value = if prepared.contains_key(&config.id) { Err(AiError::invalid("connection_id")) } else { normalize(config) };
            prepared.insert(config.id.clone(), value);
        }
        let mut state = self.state.lock().map_err(|_| AiError::internal("connections"))?;
        let mut generation = state.generation;
        let mut entries = HashMap::new();
        for (id, configuration) in prepared {
            let old = state.entries.get(&id);
            let unchanged = old.is_some_and(|old| match (&old.configuration, &configuration) {
                (Ok(a), Ok(b)) => same_invocation(a, b),
                (Err(a), Err(b)) => a == b,
                _ => false,
            });
            let entry_generation = if let Some(old) = old.filter(|_| unchanged) {
                old.generation
            } else {
                generation = generation.checked_add(1).ok_or_else(|| AiError::internal("connection_generation"))?;
                generation
            };
            entries.insert(id, Entry { generation: entry_generation, configuration });
        }
        // No mutation before every fallible check above has succeeded.
        *state = CatalogState { generation, entries };
        Ok(())
    }

    pub fn bind(&self, connection_id: &str, model_id: &str) -> Result<ConnectionBinding, AiError> {
        if !valid_id(connection_id) {
            return Err(AiError::invalid("connection_id"));
        }
        if !super::super::model_id_valid(model_id) {
            return Err(AiError::invalid("model_id"));
        }
        let state = self.state.lock().map_err(|_| AiError::internal("connections"))?;
        let entry = state.entries.get(connection_id).ok_or_else(AiError::connection_changed)?;
        let mut configuration = entry.configuration.clone()?;
        let protocol = configuration.kind.protocol().ok_or_else(|| AiError::unsupported("modelGenerate"))?;
        configuration.model = model_id.into();
        // Only resolve's live configuration determines whether a key is required.
        configuration.key = false;
        Ok(ConnectionBinding { source: Arc::clone(&self.source), generation: entry.generation, configuration, protocol })
    }

    pub(crate) fn resolve(&self, binding: &ConnectionBinding) -> Result<ResolvedConnection, AiError> {
        if !Arc::ptr_eq(&self.source, &binding.source) {
            return Err(AiError::connection_changed());
        }
        let state = self.state.lock().map_err(|_| AiError::internal("connections"))?;
        let entry = state.entries.get(binding.connection_id()).ok_or_else(AiError::connection_changed)?;
        let configuration = entry.configuration.as_ref().map_err(|_| AiError::connection_changed())?;
        if entry.generation != binding.generation || !same_invocation(configuration, &binding.configuration) {
            return Err(AiError::connection_changed());
        }
        let mut configuration = configuration.clone();
        configuration.model = binding.model_id().into();
        Ok(ResolvedConnection { configuration })
    }
}

fn same_invocation(a: &AgentConfig, b: &AgentConfig) -> bool {
    a.kind == b.kind && a.base_url == b.base_url && a.request_timeout_s == b.request_timeout_s && a.reasoning_effort == b.reasoning_effort
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

fn normalize(config: &AgentConfig) -> Result<AgentConfig, AiError> {
    if !valid_id(&config.id) {
        return Err(AiError::invalid("connection_id"));
    }
    if !config.kind.is_api() {
        return Err(AiError::unsupported("modelGenerate"));
    }
    if !(agents::MIN_REQUEST_TIMEOUT_S..=agents::MAX_REQUEST_TIMEOUT_S).contains(&config.request_timeout_s) {
        return Err(AiError::invalid("request_timeout_s"));
    }
    let effort = config.reasoning_effort.trim();
    if effort.len() > agents::MAX_REASONING_EFFORT_LEN || effort.chars().any(char::is_control) {
        return Err(AiError::invalid("reasoning_effort"));
    }
    let base_url = agents::normalize_url(&config.base_url).map_err(|_| AiError::invalid("base_url"))?;
    if !valid_endpoint(&base_url) {
        return Err(AiError::invalid("base_url"));
    }
    Ok(AgentConfig { base_url, reasoning_effort: effort.into(), ..config.clone() })
}

fn valid_endpoint(url: &str) -> bool {
    // normalize_url checks the scheme/whitespace, but deliberately is not a URL parser.
    // Refuse ambiguous or missing authorities instead of letting a client reinterpret them.
    if url.contains('\\') {
        return false;
    }
    let Some((_, rest)) = url.split_once("://") else { return false };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return false;
    }
    let (host, port) = if let Some(ip) = authority.strip_prefix('[') {
        let Some((ip, port)) = ip.split_once(']') else { return false };
        if ip.parse::<std::net::Ipv6Addr>().is_err() {
            return false;
        }
        (ip, port)
    } else {
        let end = authority.find(':').unwrap_or(authority.len());
        let (host, port) = authority.split_at(end);
        if !host.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.')) {
            return false;
        }
        (host, port)
    };
    !host.is_empty()
        && (port.is_empty()
            || port.strip_prefix(':').is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) && p.parse::<u16>().is_ok()))
}

/// Exists for one physical attempt only. No Debug, serde or catalog/binding storage.
pub(super) struct SecretValue(String);

impl SecretValue {
    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Worker-only: the future provider must call this inside its physical worker,
/// then recheck generation/control before dispatch. Never call from an egui frame.
pub(super) fn resolve_secret(connection: &ResolvedConnection, secrets: Option<&dyn Secrets>) -> Result<Option<SecretValue>, AiError> {
    let configuration = connection.configuration();
    if !configuration.key {
        return Ok(None);
    }
    if !agents::key_safe_for(&configuration.base_url) {
        return Err(AiError::invalid("base_url"));
    }
    let secrets = secrets.ok_or_else(AiError::auth_required)?;
    let key = secrets
        .get(&configuration.id)
        .map_err(|_| AiError::internal("read_secret"))?
        .filter(|key| !key.trim().is_empty())
        .ok_or_else(AiError::auth_required)?;
    Ok(Some(SecretValue(key)))
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::agent_chat::host_ai::{AiShared, call::HttpPermit};

    pub(crate) fn read_key(
        catalog: &ConnectionCatalog,
        binding: &ConnectionBinding,
        secrets: Option<&dyn Secrets>,
    ) -> Result<Option<String>, AiError> {
        let connection = catalog.resolve(binding)?;
        Ok(resolve_secret(&connection, secrets)?.map(|key| key.as_str().into()))
    }

    pub(crate) fn retained_entries(catalog: &ConnectionCatalog) -> usize {
        catalog.state.lock().unwrap().entries.len()
    }

    pub(crate) fn exhaust_generations(catalog: &ConnectionCatalog) {
        catalog.state.lock().unwrap().generation = u64::MAX;
    }

    pub(crate) fn admit(shared: &AiShared, id: &str) -> Result<HttpPermit, AiError> {
        shared.http.try_acquire(id, 2, 8)
    }
}
