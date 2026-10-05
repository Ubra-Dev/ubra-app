//! Restarts a wrapped agent that exited only to be started again.
//!
//! Codex's startup update chooser runs `npm install -g @openai/codex` (or the
//! Homebrew/bun equivalent), prints "Please restart Codex." and exits 0. The
//! `returnToLoginShell` wrapper then leaves a bare login shell, and a `codex`
//! typed there runs without the `-c` overrides Ubra injects, so the tab loses
//! the ubra MCP server and its notify hook. The session pump spots the
//! manifest's `relaunchNotice` as the wrapper reports the exit; this relaunches
//! the tab through the same spec builders `session.resume` uses.
use super::*;

impl ControlServer {
    /// Serves relaunch requests published by the Registry watcher.
    pub fn spawn_agent_relaunch(self: &Arc<Self>) {
        let stream = self.events.subscribe(
            None,
            crate::events::Filter::new(
                None,
                Some(vec![crate::events::RELAUNCH_REQUESTED.to_owned()]),
            ),
        );
        let server = Arc::downgrade(self);
        if let Err(error) = std::thread::Builder::new()
            .name("ubra-agent-relaunch".into())
            .spawn(move || {
                loop {
                    let Some(event) = stream.recv(Duration::from_secs(3600)) else {
                        if server.strong_count() == 0 {
                            return;
                        }
                        continue;
                    };
                    let Some(server) = server.upgrade() else {
                        return;
                    };
                    if event.name != crate::events::RELAUNCH_REQUESTED {
                        continue;
                    }
                    let Some(id) = event.session_id else {
                        continue;
                    };
                    if let Err(error) = server.relaunch_agent(&id) {
                        ubra_telemetry::warn_event!(
                            "session.agent_relaunch_failed",
                            session = ubra_telemetry::id(&id),
                            code = ubra_telemetry::id(&error.code),
                        );
                    } else {
                        ubra_telemetry::event!(
                            "session.agent_relaunched",
                            session = ubra_telemetry::id(&id),
                        );
                    }
                }
            })
        {
            eprintln!("ubra-engine: could not start agent relaunch: {error}");
        }
    }

    /// Replaces the tab's login shell with a fresh launch of its agent.
    ///
    /// The notice is printed before the agent has a conversation (Codex's
    /// update chooser runs ahead of its TUI), so a tab that knows no
    /// conversation starts fresh and one launched to resume resumes the same
    /// id again; never `resume --last`, which could pick another tab's thread.
    pub(super) fn relaunch_agent(&self, id: &str) -> Result<(), ControlError> {
        let _operation = session_operation::SessionOperation::for_session(self, id)?;
        let (_record, spec) = {
            let registry = self.registry.lock().map_err(poisoned)?;
            let record = registry
                .record(id)
                .ok_or_else(|| ControlError::not_found(id.to_owned()))?;
            if record.host.is_some()
                || record.is_archived()
                || registry.get(id).is_none()
                || matches!(record.status, ubra_proto::SessionStatus::Exited(_))
            {
                return Err(ControlError::bad_request(
                    "Only a live local tab is relaunched",
                ));
            }
            let spec = match record.agent_session_id.as_deref() {
                Some(conversation) => self.resume_spec(
                    &registry,
                    id,
                    record.kind.id(),
                    &record.cwd,
                    Some(conversation),
                )?,
                None => self.fresh_spec(&registry, id, record.kind.id(), &record.cwd, None)?,
            };
            (record, spec)
        };
        self.terminate_session_unlocked(id, Duration::from_millis(500))?;
        let mut registry = self.registry.lock().map_err(poisoned)?;
        registry.respawn(spec).map_err(io_control_error)?;
        registry.persist_now().map_err(io_control_error)?;
        self.publish_updated(&registry, id);
        Ok(())
    }
}
