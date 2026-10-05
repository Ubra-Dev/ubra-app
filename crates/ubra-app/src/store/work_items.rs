//! Agents started from a note's to-do.
//!
//! Starting work from a to-do is an ordinary spawn whose parent is the note
//! Session, with two differences from ⌘T: the new session is not selected,
//! because the person stays in the note to watch it, and the new session's
//! id comes back to the note so the to-do can link it. The executor writes
//! that link into the note file itself (so it lands even if the note was
//! closed meanwhile) and records the outcome here for the open editor.

use std::collections::HashMap;
use std::path::PathBuf;

use ubra_proto::{SessionId, SessionSpawnParams};

use super::{SessionStore, StoreEffect};

/// Where the new session's chip goes once the Engine names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkLink {
    pub notes_dir: PathBuf,
    pub note_id: String,
    /// The to-do's text when the person pressed Start, to find it in the
    /// file if the editor is gone.
    pub todo_text: String,
    pub label: String,
}

#[derive(Default)]
pub(crate) struct WorkItems {
    next_ticket: u64,
    outcomes: HashMap<u64, Result<SessionId, String>>,
    /// A session whose to-do a note should scroll to: (note session, child).
    reveal: Option<(SessionId, SessionId)>,
}

impl SessionStore {
    /// Spawns an agent for a to-do without leaving the note. Returns the
    /// ticket its outcome is filed under.
    pub fn start_work_item(&mut self, params: SessionSpawnParams, link: WorkLink) -> u64 {
        self.work_items.next_ticket += 1;
        let ticket = self.work_items.next_ticket;
        self.record_agent_used(&params.kind);
        self.emit(StoreEffect::StartWorkItem {
            ticket,
            params,
            link,
        });
        ticket
    }

    pub(crate) fn finish_work_item(&mut self, ticket: u64, outcome: Result<SessionId, String>) {
        self.work_items.outcomes.insert(ticket, outcome);
        self.emit(StoreEffect::UiChanged);
    }

    /// The spawn result for `ticket`, once, when it has arrived.
    pub fn take_work_outcome(&mut self, ticket: u64) -> Option<Result<SessionId, String>> {
        self.work_items.outcomes.remove(&ticket)
    }

    /// Asks the note `note` to scroll to the to-do that links `child`, once
    /// it is shown: the way back from a session to the work item it serves.
    /// Storing the target is all this does; the caller routes the note itself
    /// (the Notes surface detail), since notes never take the selection.
    pub fn reveal_in_note(&mut self, note: SessionId, child: SessionId) {
        self.work_items.reveal = Some((note, child));
    }

    /// The session whose to-do `note` should scroll to, once.
    pub fn take_note_reveal(&mut self, note: &SessionId) -> Option<SessionId> {
        match &self.work_items.reveal {
            Some((target, _)) if target == note => self.work_items.reveal.take().map(|(_, c)| c),
            _ => None,
        }
    }
}

/// Runs a `StartWorkItem` effect: spawn, link the to-do in the file, file
/// the outcome for the editor. Failures stay with the to-do instead of the
/// app-wide error banner.
pub(super) async fn run(
    client: std::sync::Arc<ubra_client::DaemonClient>,
    store: std::sync::Arc<std::sync::RwLock<SessionStore>>,
    ticket: u64,
    params: SessionSpawnParams,
    link: WorkLink,
) {
    let outcome = match client.spawn(params).await {
        Ok(id) => {
            let session = id.clone();
            // The file write takes the notes store lock; keep it off the
            // async executor.
            let _ = tokio::task::spawn_blocking(move || {
                crate::notes::work_item::link_in_file(&link, &session)
            })
            .await;
            Ok(id)
        }
        Err(error) => Err(error.to_string()),
    };
    store
        .write()
        .expect("session store lock poisoned")
        .finish_work_item(ticket, outcome);
}
