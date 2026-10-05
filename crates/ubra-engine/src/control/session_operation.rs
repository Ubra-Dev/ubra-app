//! Per-session lifecycle serialization shared by dispatch and restore.
//!
//! Lifecycle operations reserve one session without holding the Registry
//! during SSH: a second operation on the same session fails fast instead of
//! interleaving with the first.
use super::*;

pub(super) struct SessionOperation<'a> {
    server: &'a ControlServer,
    id: Option<String>,
}

impl<'a> SessionOperation<'a> {
    pub(super) fn acquire(
        server: &'a ControlServer,
        method: &str,
        params: Option<&Value>,
    ) -> Result<Self, ControlError> {
        let guarded = matches!(
            method,
            Method::SESSION_WAKE
                | Method::SESSION_HIBERNATE
                | Method::SESSION_RESUME
                | Method::SESSION_RECONNECT
                | Method::SESSION_FORK
                | Method::SESSION_KILL
                | Method::SESSION_REMOVE
                | Method::SESSION_MIGRATE
                | Method::SESSION_ARCHIVE
                | Method::SESSION_UNARCHIVE
                | Method::SESSION_REPARENT_WORKTREE
        );
        let id = guarded
            .then(|| params?.get("sessionID")?.as_str().map(str::to_owned))
            .flatten();
        Self::reserve(server, id)
    }

    pub(super) fn for_session(server: &'a ControlServer, id: &str) -> Result<Self, ControlError> {
        Self::reserve(server, Some(id.to_owned()))
    }

    fn reserve(server: &'a ControlServer, id: Option<String>) -> Result<Self, ControlError> {
        if let Some(id) = &id
            && !server
                .session_operations
                .lock()
                .map_err(poisoned)?
                .insert(id.clone())
        {
            return Err(ControlError::bad_request(
                "Another operation is already changing this session. Wait for it to finish.",
            ));
        }
        Ok(Self { server, id })
    }
}

impl Drop for SessionOperation<'_> {
    fn drop(&mut self) {
        if let Some(id) = &self.id
            && let Ok(mut operations) = self.server.session_operations.lock()
        {
            operations.remove(id);
        }
    }
}
