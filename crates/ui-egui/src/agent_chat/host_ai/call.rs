//! Physical admission is independent of logical completion and cancellation.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use super::types::AiError;

/// One shared atomic allowance per admitted turn, not per physical call.
pub struct ModelCallBudget {
    remaining: AtomicU32,
    limit: u32,
}

impl ModelCallBudget {
    pub fn new(limit: u32) -> Result<Self, AiError> {
        if !(1..=20).contains(&limit) {
            return Err(AiError::invalid("model_call_budget"));
        }
        Ok(Self { remaining: AtomicU32::new(limit), limit })
    }

    pub(super) fn debit(&self) -> Result<(), AiError> {
        self.remaining
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| remaining.checked_sub(1))
            .map(|_| ())
            .map_err(|_| AiError::exhausted("model_calls", self.limit))
    }
}

/// Shared by every physical caller in a host, including separate windows.
pub struct HttpAdmission {
    per_connection: usize,
    per_host: usize,
    counts: Arc<Mutex<HttpCounts>>,
}

#[derive(Default)]
struct HttpCounts {
    connections: HashMap<String, usize>,
    host: usize,
}

impl Default for HttpAdmission {
    fn default() -> Self {
        Self { per_connection: 2, per_host: 8, counts: Arc::default() }
    }
}

impl HttpAdmission {
    pub fn new(per_connection: usize, per_host: usize) -> Result<Self, AiError> {
        validate_http_limits(per_connection, per_host)?;
        Ok(Self { per_connection, per_host, counts: Arc::default() })
    }

    /// No waiting queue. Move the returned permit into the physical worker,
    /// never into its waiter or cancellation control.
    pub(crate) fn try_acquire(&self, connection_id: &str, per_connection: usize, per_host: usize) -> Result<HttpPermit, AiError> {
        validate_http_limits(per_connection, per_host)?;
        let per_connection = per_connection.min(self.per_connection);
        let per_host = per_host.min(self.per_host);
        let mut counts = self.counts.lock().map_err(|_| AiError::internal("http_admission"))?;
        let connection = counts.connections.get(connection_id).copied().unwrap_or(0);
        if connection >= per_connection {
            return Err(AiError::exhausted("http_per_connection", per_connection as u32));
        }
        if counts.host >= per_host {
            return Err(AiError::exhausted("http_per_host", per_host as u32));
        }
        let connection = connection.checked_add(1).ok_or_else(|| AiError::internal("http_admission"))?;
        let host = counts.host.checked_add(1).ok_or_else(|| AiError::internal("http_admission"))?;
        counts.connections.insert(connection_id.into(), connection);
        counts.host = host;
        Ok(HttpPermit { counts: Arc::clone(&self.counts), connection_id: connection_id.into() })
    }
}

fn validate_http_limits(per_connection: usize, per_host: usize) -> Result<(), AiError> {
    if !(1..=2).contains(&per_connection) {
        return Err(AiError::invalid("http_per_connection"));
    }
    if !(1..=8).contains(&per_host) {
        return Err(AiError::invalid("http_per_host"));
    }
    Ok(())
}

/// Not cloneable: only the physical worker's exit returns its capacity.
pub(crate) struct HttpPermit {
    counts: Arc<Mutex<HttpCounts>>,
    connection_id: String,
}

impl HttpPermit {
    fn release(&self) -> Result<(), AiError> {
        let mut counts = self.counts.lock().map_err(|_| AiError::internal("http_release"))?;
        let connection =
            counts.connections.get(&self.connection_id).and_then(|count| count.checked_sub(1)).ok_or_else(|| AiError::internal("http_release"))?;
        let host = counts.host.checked_sub(1).ok_or_else(|| AiError::internal("http_release"))?;
        if connection == 0 {
            counts.connections.remove(&self.connection_id);
        } else if let Some(count) = counts.connections.get_mut(&self.connection_id) {
            *count = connection;
        }
        counts.host = host;
        Ok(())
    }
}

impl Drop for HttpPermit {
    fn drop(&mut self) {
        // Drop cannot return an error. A poisoned limiter remains closed to
        // all future admission, which reports InternalError rather than panicking.
        let _ = self.release();
    }
}

enum CallState {
    Active,
    Published,
    Cancelled,
    TimedOut,
}

impl CallState {
    fn check(&self) -> Result<(), AiError> {
        match self {
            Self::Active => Ok(()),
            Self::Published => Err(AiError::invalid("call_control")),
            Self::Cancelled => Err(AiError::cancelled()),
            Self::TimedOut => Err(AiError::timeout()),
        }
    }
}

pub struct CallControl {
    state: Mutex<CallState>,
    turn: Option<Arc<Mutex<TurnState>>>,
}

impl CallControl {
    pub(crate) fn new() -> Self {
        Self { state: Mutex::new(CallState::Active), turn: None }
    }

    pub fn cancel(&self) {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poison) => poison.into_inner(),
        };
        if matches!(*state, CallState::Active) {
            *state = CallState::Cancelled;
            let _ = self.retire();
        }
    }

    pub(crate) fn check(&self, now: Duration, deadline: Duration) -> Result<(), AiError> {
        self.while_active(now, deadline, || Ok(()))
    }

    /// Dispatch debit and proposal construction linearize against cancellation.
    /// Only short, internal work is permitted: never external keychain/HTTP work.
    pub(super) fn while_active<T>(&self, now: Duration, deadline: Duration, f: impl FnOnce() -> Result<T, AiError>) -> Result<T, AiError> {
        let mut state = self.state.lock().map_err(|_| AiError::internal("call_check"))?;
        let mut turn = self.active(&mut state)?;
        if now >= deadline {
            *state = CallState::TimedOut;
            if let Some(turn) = turn.as_mut() {
                self.unregister(turn);
            }
            return Err(AiError::timeout());
        }
        f()
    }

    /// Lock order: call control, short parent admission gate, then session store.
    /// Release the parent gate before running the closure; a publication admitted
    /// before turn cancellation may finish afterwards. Never cancel controls while
    /// holding a turn/session lock, or re-enter this control from its closure.
    pub(crate) fn publish<T>(&self, f: impl FnOnce() -> Result<T, AiError>) -> Result<T, AiError> {
        let mut state = self.state.lock().map_err(|_| AiError::internal("call_publish"))?;
        let mut turn = self.active(&mut state)?;
        // Even an unsuccessful publication attempt cannot reuse this lifetime.
        *state = CallState::Published;
        if let Some(turn) = turn.as_mut() {
            self.unregister(turn);
        }
        drop(turn);
        f()
    }

    /// Check the same trusted parent gate that turn cancellation closes. The
    /// caller holds the call lock and keeps this guard through its state change.
    fn active(&self, state: &mut CallState) -> Result<Option<MutexGuard<'_, TurnState>>, AiError> {
        state.check()?;
        let mut turn = self.turn.as_ref().map(|turn| turn.lock().map_err(|_| AiError::internal("call_admission"))).transpose()?;
        if let Some(turn) = turn.as_mut()
            && turn.cancelled
        {
            *state = CallState::Cancelled;
            self.unregister(turn);
            return Err(AiError::cancelled());
        }
        Ok(turn)
    }

    fn unregister(&self, turn: &mut TurnState) {
        turn.children.retain(|child| !std::ptr::eq(child.as_ptr(), self));
    }

    fn retire(&self) -> Result<(), AiError> {
        if let Some(turn) = &self.turn {
            let mut turn = turn.lock().map_err(|_| AiError::internal("call_retire"))?;
            self.unregister(&mut turn);
        }
        Ok(())
    }
}

impl Drop for CallControl {
    fn drop(&mut self) {
        // No strong child is owned by the registry. Remove its weak registration
        // even when a caller drops an active child without explicitly cancelling.
        let _ = self.retire();
    }
}

#[derive(Default)]
struct TurnState {
    cancelled: bool,
    children: Vec<Weak<CallControl>>,
}

#[derive(Default)]
pub struct TurnControl {
    state: Arc<Mutex<TurnState>>,
}

impl TurnControl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn new_call(&self) -> Result<Arc<CallControl>, AiError> {
        let mut state = self.state.lock().map_err(|_| AiError::internal("turn_admission"))?;
        if state.cancelled {
            return Err(AiError::cancelled());
        }
        state.children.retain(|child| child.strong_count() > 0);
        let mut call = CallControl::new();
        call.turn = Some(Arc::clone(&self.state));
        let call = Arc::new(call);
        state.children.push(Arc::downgrade(&call));
        Ok(call)
    }

    pub fn cancel(&self) {
        let children = {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(poison) => poison.into_inner(),
            };
            state.cancelled = true;
            std::mem::take(&mut state.children)
        };
        // No turn/session lock is held while waiting for a child's publication.
        for child in children {
            if let Some(child) = child.upgrade() {
                child.cancel();
            }
        }
    }

    pub fn check(&self) -> Result<(), AiError> {
        let state = self.state.lock().map_err(|_| AiError::internal("turn_check"))?;
        if state.cancelled { Err(AiError::cancelled()) } else { Ok(()) }
    }
}
