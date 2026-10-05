//! "Resume all": bring back every session a restart ended — the computer's
//! or Ubra's — in one action instead of one click per tab.
//!
//! A reboot can end two dozen agents at once. Resuming them all in the same
//! instant would start two dozen CLIs together, each loading its whole
//! conversation, so the batch keeps a few in flight and starts the next only
//! once an earlier one has come up (or given it a fair while). It runs off
//! the effect loop and records one summary for the status bar.

use std::future::Future;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::broadcast;
use ubra_client::DaemonClient;
use ubra_proto::{SessionId, SessionStatus};

use super::{SessionStore, StoreEffect};

/// Resumes in flight at once.
pub(super) const MAX_IN_FLIGHT: usize = 3;
/// How long one resumed session may stay starting before the next is let in
/// anyway: a slow agent should not stall the rest of the batch.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(20);
const SETTLE_POLL: Duration = Duration::from_millis(250);

/// How far a running batch has got, for the card that started it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResumeAllProgress {
    pub total: usize,
    pub finished: usize,
}

/// How a finished batch turned out, held on the store until the status bar
/// has shown it for its hold and cleared it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResumeAllSummary {
    pub resumed: usize,
    pub total: usize,
    pub first_failure: Option<String>,
}

impl SessionStore {
    /// Every session a restart ended that can pick up where it left off,
    /// the selected one first, then the most recently active.
    pub fn restart_ended_resumable(&self) -> Vec<SessionId> {
        let mut candidates: Vec<_> = self
            .sessions
            .values()
            .filter(|session| {
                // One the selection is already bringing back is not resumed twice.
                !self.auto_resuming.contains(&session.id)
                    && !session.is_archived()
                    && session.can_resume()
                    && matches!(&session.status, SessionStatus::Exited(info) if info.ended_by_restart())
            })
            .map(|session| {
                let selected = self.selected_session_id.as_ref() == Some(&session.id);
                (!selected, -session.updated_at.0, session.id.clone())
            })
            .collect();
        candidates.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then(a.1.total_cmp(&b.1))
                .then_with(|| a.2.0.cmp(&b.2.0))
        });
        candidates.into_iter().map(|(_, _, id)| id).collect()
    }

    /// How many sessions "Resume all" would bring back, when offering it is
    /// worth it: more than one, and no batch already running.
    pub fn resume_all_offer(&self) -> Option<usize> {
        if self.resume_all.is_some() {
            return None;
        }
        let count = self.restart_ended_resumable().len();
        (count > 1).then_some(count)
    }

    pub fn resume_all_progress(&self) -> Option<ResumeAllProgress> {
        self.resume_all
    }

    /// Starts resuming every restart-ended session. Returns how many it
    /// queued; none while a batch is already running.
    pub fn resume_all(&mut self) -> usize {
        if self.resume_all.is_some() {
            return 0;
        }
        let ids = self.restart_ended_resumable();
        if ids.is_empty() {
            return 0;
        }
        for id in &ids {
            // The same once-per-run claim selection uses, so selecting a
            // queued tab does not resume it a second time; `auto_resuming`
            // shows "Resuming conversation…" on its card until it is back.
            self.auto_resume_attempted.insert(id.clone());
            self.auto_resuming.insert(id.clone());
        }
        self.resume_all = Some(ResumeAllProgress {
            total: ids.len(),
            finished: 0,
        });
        let count = ids.len();
        self.emit(StoreEffect::ResumeAll(ids));
        count
    }

    pub(super) fn finish_resume_all_one(&mut self, id: &SessionId) {
        self.auto_resuming.remove(id);
        if let Some(progress) = self.resume_all.as_mut() {
            progress.finished += 1;
        }
    }

    /// Ends the batch and records how it turned out for the status bar.
    pub(crate) fn finish_resume_all(
        &mut self,
        resumed: usize,
        total: usize,
        first_failure: Option<String>,
    ) {
        self.resume_all = None;
        self.resume_all_summary = Some(ResumeAllSummary {
            resumed,
            total,
            first_failure,
        });
    }

    pub fn resume_all_summary(&self) -> Option<&ResumeAllSummary> {
        self.resume_all_summary.as_ref()
    }

    pub fn clear_resume_all_summary(&mut self) {
        self.resume_all_summary = None;
    }

    /// Whether a resumed session has come up, so the next may start: it is
    /// no longer starting, nor still showing the exit it was resumed from.
    pub(super) fn resume_settled(&self, id: &SessionId) -> bool {
        self.sessions
            .get(id)
            .is_none_or(|session| match &session.status {
                SessionStatus::Starting => false,
                SessionStatus::Exited(info) => !info.ended_by_restart(),
                _ => true,
            })
    }
}

/// Resumes `ids` in order with at most [`MAX_IN_FLIGHT`] outstanding. A
/// slot frees when its resume fails, or when `settled` says the session came
/// up (or `timeout` passes). `finished` hears each outcome as it lands.
pub(super) async fn drive<R, F, S>(
    ids: Vec<SessionId>,
    resume: R,
    settled: S,
    timeout: Duration,
    mut finished: impl FnMut(SessionId, Result<(), String>),
) where
    R: Fn(SessionId) -> F,
    F: Future<Output = Result<(), String>> + Send + 'static,
    S: Fn(&SessionId) -> bool + Clone + Send + Sync + 'static,
{
    let mut queue = ids.into_iter();
    let mut in_flight = tokio::task::JoinSet::new();
    loop {
        while in_flight.len() < MAX_IN_FLIGHT {
            let Some(id) = queue.next() else { break };
            let resumed = resume(id.clone());
            let settled = settled.clone();
            in_flight.spawn(async move {
                let result = resumed.await;
                if result.is_ok() {
                    let deadline = tokio::time::Instant::now() + timeout;
                    while !settled(&id) && tokio::time::Instant::now() < deadline {
                        tokio::time::sleep(SETTLE_POLL).await;
                    }
                }
                (id, result)
            });
        }
        match in_flight.join_next().await {
            Some(Ok((id, result))) => finished(id, result),
            Some(Err(_)) => {}
            None => break,
        }
    }
}

pub(super) async fn run(
    ids: Vec<SessionId>,
    client: Arc<DaemonClient>,
    store: Arc<RwLock<SessionStore>>,
    change_tx: broadcast::Sender<()>,
) {
    let total = ids.len();
    let mut resumed = 0;
    let mut first_failure: Option<String> = None;
    let settled_store = Arc::clone(&store);
    drive(
        ids,
        |id| {
            let client = Arc::clone(&client);
            async move {
                client
                    .resume(&id)
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            }
        },
        move |id| {
            settled_store
                .read()
                .expect("session store lock poisoned")
                .resume_settled(id)
        },
        SETTLE_TIMEOUT,
        |id, result| {
            match result {
                Ok(()) => resumed += 1,
                Err(error) => {
                    first_failure.get_or_insert(error);
                }
            }
            store
                .write()
                .expect("session store lock poisoned")
                .finish_resume_all_one(&id);
            let _ = change_tx.send(());
        },
    )
    .await;
    store
        .write()
        .expect("session store lock poisoned")
        .finish_resume_all(resumed, total, first_failure);
    let _ = change_tx.send(());
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn ids(count: usize) -> Vec<SessionId> {
        (0..count)
            .map(|index| SessionId(format!("s{index}")))
            .collect()
    }

    /// Two dozen restart-ended agents never start together: at most
    /// `MAX_IN_FLIGHT` resumes are outstanding, every one still runs, in
    /// order, and a failure neither stops the batch nor holds its slot.
    #[tokio::test]
    async fn a_batch_keeps_a_bounded_number_of_resumes_in_flight() {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Mutex::new(Vec::new()));
        let mut outcomes = Vec::new();
        drive(
            ids(24),
            |id| {
                let active = Arc::clone(&active);
                let peak = Arc::clone(&peak);
                started.lock().unwrap().push(id.clone());
                async move {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    if id.0 == "s3" {
                        Err("no conversation".to_owned())
                    } else {
                        Ok(())
                    }
                }
            },
            |_| true,
            Duration::from_secs(1),
            |id, result| outcomes.push((id, result.is_ok())),
        )
        .await;
        assert_eq!(peak.load(Ordering::SeqCst), MAX_IN_FLIGHT);
        assert_eq!(*started.lock().unwrap(), ids(24), "started in order");
        assert_eq!(outcomes.len(), 24);
        assert_eq!(outcomes.iter().filter(|(_, ok)| !ok).count(), 1);
    }

    /// A resumed session holds its slot until it has come up, so the next
    /// waits for it rather than piling on; one that never comes up is let
    /// go after the timeout.
    #[tokio::test(start_paused = true)]
    async fn the_next_resume_waits_for_the_last_to_come_up() {
        let started = Arc::new(Mutex::new(Vec::new()));
        let finished = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&finished);
        let runner = tokio::spawn({
            let started = Arc::clone(&started);
            async move {
                drive(
                    ids(MAX_IN_FLIGHT + 1),
                    |id| {
                        started.lock().unwrap().push(id);
                        async { Ok(()) }
                    },
                    |_| false,
                    Duration::from_secs(20),
                    move |id, _| recorded.lock().unwrap().push(id),
                )
                .await;
            }
        });
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert_eq!(started.lock().unwrap().len(), MAX_IN_FLIGHT);
        assert!(finished.lock().unwrap().is_empty());
        tokio::time::sleep(Duration::from_secs(40)).await;
        runner.await.unwrap();
        assert_eq!(started.lock().unwrap().len(), MAX_IN_FLIGHT + 1);
        assert_eq!(finished.lock().unwrap().len(), MAX_IN_FLIGHT + 1);
    }
}
