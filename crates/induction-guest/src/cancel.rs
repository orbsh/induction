//! The guest's half of stage two (aura ADR-0047, gravity ADR-0008).
//!
//! A stop has two stages, and the second one is NOT storage either: the parent
//! sends an out-of-band `interrupt` frame naming a call it no longer waits for,
//! and the guest's reader loop turns it into a [`CancelToken`] the running
//! handler can see.
//!
//! It is a token and not a delivery into the handler because a handler cannot be
//! delivered to: it is a plain function on a worker thread. What the token gives
//! it is the only thing that can work — a flag (and a wait) that the handler
//! checks in the loop it already runs: between chunks, around a blocking read,
//! once per tick of whatever it is doing.
//!
//! **Cooperative by construction.** A handler that never looks cannot be
//! stopped from inside; the host's clock — the roadmap's next step — is what
//! bounds it from outside. That is why the token has no `abort`: pretending to
//! interrupt a running function would be the lie this pair of crates exists to
//! avoid.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// One call's cancel flag: cheap to clone, shared between the reader loop (which
/// raises it) and the handler (which reads it).
#[derive(Clone, Default)]
pub struct CancelToken {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    cancelled: Mutex<bool>,
    // Wakes `wait`; the handler is the only waiter by contract, but a condvar
    // costs nothing and makes several waiters correct rather than lucky.
    wake: Condvar,
}

impl std::fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CancelToken")
            .field(&self.is_cancelled())
            .finish()
    }
}

impl CancelToken {
    /// A token that is already raised — what the reader loop leaves for a call that
    /// is still queued when its interrupt arrives, so "too early" is not "lost".
    pub fn cancelled() -> Self {
        let token = Self::default();
        token.cancel();
        token
    }

    /// Raise it. Idempotent; the loop may raise a token that a worker has and
    /// one it has not (a call still queued behind others).
    pub fn cancel(&self) {
        let mut flag = self
            .inner
            .cancelled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *flag = true;
        drop(flag);
        self.inner.wake.notify_all();
    }

    /// Raised? The cheap read a handler makes between chunks.
    pub fn is_cancelled(&self) -> bool {
        *self
            .inner
            .cancelled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Block until raised or `timeout` passes; returns "cancelled?". This is the
    /// shape a blocking loop wants: `if cancel.wait(Duration::from_millis(50)) {
    /// break }` instead of a busy spin, and it keeps the flag and the wake in one
    /// place rather than in every guest's own condvar.
    pub fn wait(&self, timeout: Duration) -> bool {
        let flag = self
            .inner
            .cancelled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *flag {
            return true;
        }
        let (flag, _) = self
            .inner
            .wake
            .wait_timeout(flag, timeout)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *flag
    }
}