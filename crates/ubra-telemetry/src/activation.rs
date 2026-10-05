//! Activation milestones: the once-per-install steps of the new-user funnel.
//!
//! Each milestone is a marker file under `<state>/telemetry/activation/`,
//! published with an exclusive hard link, so exactly one process (app or
//! Engine) records it however many race for it, and never again after that,
//! across restarts and upgrades. `origin.json` is the install's activation
//! baseline: when this build first ran here, and whether Ubra had already
//! been used on this Mac before (`preexisting`), so an upgrade does not look
//! like a new user. See `ubra/TELEMETRY.md` (Activation).
//!
//! Markers hold only timestamps and the `preexisting` bit; the event fields
//! are the ones in the catalog.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

use crate::{Severity, Value};

const DIR: &str = "activation";
const ORIGIN_FILE: &str = "origin.json";

/// A funnel step, in funnel order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Milestone {
    /// The app ran on this install for the first time (with this build or
    /// later).
    FirstLaunch,
    /// A coding agent became launchable: installed from the welcome, already
    /// on the Mac at first launch, or installed some other way later.
    AgentReady,
    /// The first agent session started (terminals and notes do not count).
    FirstSession,
    /// The second agent session started.
    SecondSession,
    /// The first agent session started by another agent (it has a parent).
    FirstHelper,
    /// The app was launched on a later calendar day than its first launch.
    Returned,
}

impl Milestone {
    pub const ALL: [Milestone; 6] = [
        Milestone::FirstLaunch,
        Milestone::AgentReady,
        Milestone::FirstSession,
        Milestone::SecondSession,
        Milestone::FirstHelper,
        Milestone::Returned,
    ];

    /// The event kind recorded when the milestone is reached.
    #[must_use]
    pub const fn kind(self) -> &'static str {
        match self {
            Milestone::FirstLaunch => "activation.first_launch",
            Milestone::AgentReady => "activation.agent_ready",
            Milestone::FirstSession => "activation.first_session",
            Milestone::SecondSession => "activation.second_session",
            Milestone::FirstHelper => "activation.first_helper",
            Milestone::Returned => "activation.returned",
        }
    }

    /// The marker's file stem, also the step name `ubra-debug funnel` prints.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Milestone::FirstLaunch => "first_launch",
            Milestone::AgentReady => "agent_ready",
            Milestone::FirstSession => "first_session",
            Milestone::SecondSession => "second_session",
            Milestone::FirstHelper => "first_helper",
            Milestone::Returned => "returned",
        }
    }

    const fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

/// The install's activation baseline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    /// When a build with activation tracking first ran on this install.
    pub created_ms: u64,
    /// Ubra had been used on this Mac before that build: its settings,
    /// Engine state or preferences already existed. Every milestone event of
    /// such an install carries `preexisting: true`, so the funnel can leave
    /// upgrades out of the new-user cohort.
    pub preexisting: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Marker {
    t: u64,
}

/// What a successful claim knows, for the event's fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Claim {
    pub preexisting: bool,
    /// Seconds from the first launch (or, when the app never launched, from
    /// the baseline) to this milestone.
    pub since_first_launch_s: u64,
}

/// One install's milestone markers.
#[derive(Clone, Debug)]
pub struct Activation {
    dir: PathBuf,
}

impl Activation {
    #[must_use]
    pub fn new(state_dir: &Path) -> Self {
        Self {
            dir: crate::identity::telemetry_dir(state_dir).join(DIR),
        }
    }

    /// The baseline, if one was written.
    #[must_use]
    pub fn origin(&self) -> Option<Origin> {
        read_json(&self.dir.join(ORIGIN_FILE))
    }

    /// Loads the baseline, creating it on first use. `preexisting` is only
    /// evaluated when no baseline exists yet; call this before the process
    /// writes any of the files the evidence looks at.
    pub fn ensure_origin(
        &self,
        now_ms: u64,
        preexisting: impl FnOnce() -> bool,
    ) -> std::io::Result<Origin> {
        if let Some(origin) = self.origin() {
            return Ok(origin);
        }
        let origin = Origin {
            created_ms: now_ms,
            preexisting: preexisting(),
        };
        let path = self.dir.join(ORIGIN_FILE);
        match crate::identity::publish_exclusive(&path, &serde_json::to_vec(&origin)?) {
            Ok(()) => Ok(origin),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                self.origin().ok_or(error)
            }
            Err(error) => Err(error),
        }
    }

    /// When `milestone` was reached, if it was.
    #[must_use]
    pub fn reached(&self, milestone: Milestone) -> Option<u64> {
        read_json::<Marker>(&self.marker(milestone)).map(|marker| marker.t)
    }

    /// Marks `milestone` reached at `now_ms`. `Some` exactly once per
    /// install, in the process whose marker won; `None` when it was already
    /// reached. The baseline must exist (see [`Activation::ensure_origin`]).
    pub fn claim(&self, milestone: Milestone, now_ms: u64) -> std::io::Result<Option<Claim>> {
        let path = self.marker(milestone);
        if path.exists() {
            return Ok(None);
        }
        let origin = self
            .origin()
            .ok_or_else(|| std::io::Error::other("activation baseline missing"))?;
        match crate::identity::publish_exclusive(&path, &serde_json::to_vec(&Marker { t: now_ms })?)
        {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(None),
            Err(error) => return Err(error),
        }
        let start = self
            .reached(Milestone::FirstLaunch)
            .unwrap_or(origin.created_ms);
        Ok(Some(Claim {
            preexisting: origin.preexisting,
            since_first_launch_s: now_ms.saturating_sub(start) / 1000,
        }))
    }

    /// The first of `milestones` not reached yet, claimed. For "first, then
    /// second session": two racing spawns get one each.
    pub fn claim_next(
        &self,
        milestones: &[Milestone],
        now_ms: u64,
    ) -> std::io::Result<Option<(Milestone, Claim)>> {
        for &milestone in milestones {
            if let Some(claim) = self.claim(milestone, now_ms)? {
                return Ok(Some((milestone, claim)));
            }
        }
        Ok(None)
    }

    /// Calendar days (local time) from the first launch to `now_ms`; `None`
    /// before a first launch.
    #[must_use]
    pub fn days_since_first_launch(&self, now_ms: u64) -> Option<i64> {
        let first = self.reached(Milestone::FirstLaunch)?;
        Some(local_day(now_ms) - local_day(first))
    }

    fn marker(&self, milestone: Milestone) -> PathBuf {
        self.dir.join(format!("{}.json", milestone.name()))
    }
}

/// Whether Ubra was used on this Mac before activation tracking: the
/// diagnostics settings (written on the first run of any recording build),
/// the Engine's session table, or the app's preferences already exist.
#[must_use]
pub fn prior_use_evidence(state_dir: &Path, home: Option<&Path>) -> bool {
    crate::identity::telemetry_dir(state_dir)
        .join("config.json")
        .exists()
        || state_dir.join(ubra_proto::paths::STATE_FILE_NAME).exists()
        || home.is_some_and(|home| ubra_proto::paths::UbraPaths::prefs_file(home).exists())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Days since the epoch in this Mac's local time zone.
fn local_day(ms: u64) -> i64 {
    let secs = i64::try_from(ms / 1000).unwrap_or(i64::MAX);
    (secs + utc_offset_s(secs)).div_euclid(86_400)
}

#[cfg(unix)]
fn utc_offset_s(secs: i64) -> i64 {
    let time: libc::time_t = secs;
    // SAFETY: localtime_r writes only into `tm`, which outlives the call.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&raw const time, &raw mut tm).is_null() {
            return 0;
        }
        tm.tm_gmtoff
    }
}

#[cfg(not(unix))]
fn utc_offset_s(_secs: i64) -> i64 {
    0
}

/// Milestones this process already knows are reached, so a busy Engine
/// does not stat marker files on every spawn.
static KNOWN: AtomicU8 = AtomicU8::new(0);

/// Creates the baseline for the recording process, with evidence from
/// `home`. Call once at start, before the process writes its own state.
pub fn init_origin(home: Option<&Path>) -> Option<Origin> {
    let state_dir = crate::recording_state_dir()?;
    Activation::new(state_dir)
        .ensure_origin(crate::now_ms(), || prior_use_evidence(state_dir, home))
        .ok()
}

/// Whether `milestone` is reached. Also true while recording is off, so
/// callers skip the work of building its fields.
#[must_use]
pub fn is_reached(milestone: Milestone) -> bool {
    if KNOWN.load(Ordering::Relaxed) & milestone.bit() != 0 {
        return true;
    }
    let Some(state_dir) = crate::recording_state_dir() else {
        return true;
    };
    let reached = Activation::new(state_dir).reached(milestone).is_some();
    if reached {
        KNOWN.fetch_or(milestone.bit(), Ordering::Relaxed);
    }
    reached
}

/// Claims the first unreached of `milestones` and records its event with
/// `fields` plus `preexisting` and `since_first_launch_s`. Returns the
/// milestone recorded, if any. A no-op while recording is off.
pub fn reach_next(
    milestones: &[Milestone],
    fields: impl FnOnce(Milestone) -> Vec<(&'static str, Value)>,
) -> Option<Milestone> {
    let state_dir = crate::recording_state_dir()?;
    let pending: Vec<Milestone> = milestones
        .iter()
        .copied()
        .filter(|milestone| KNOWN.load(Ordering::Relaxed) & milestone.bit() == 0)
        .collect();
    if pending.is_empty() {
        return None;
    }
    let activation = Activation::new(state_dir);
    if activation.origin().is_none() {
        // A process that skipped init_origin (an Engine started by the CLI
        // before the app ever ran) still gets a baseline.
        activation
            .ensure_origin(crate::now_ms(), || prior_use_evidence(state_dir, None))
            .ok()?;
    }
    let now = crate::now_ms();
    for milestone in pending {
        match activation.claim(milestone, now) {
            Ok(Some(claim)) => {
                KNOWN.fetch_or(milestone.bit(), Ordering::Relaxed);
                let mut fields = fields(milestone);
                fields.push(("preexisting", Value::from(claim.preexisting)));
                fields.push((
                    "since_first_launch_s",
                    Value::from(claim.since_first_launch_s),
                ));
                crate::record(milestone.kind(), Severity::Info, fields);
                return Some(milestone);
            }
            Ok(None) => {
                KNOWN.fetch_or(milestone.bit(), Ordering::Relaxed);
            }
            Err(_) => return None,
        }
    }
    None
}

/// [`reach_next`] for one milestone.
pub fn reach(milestone: Milestone, fields: Vec<(&'static str, Value)>) -> bool {
    reach_next(&[milestone], |_| fields).is_some()
}

/// The app's launch-time milestones: `first_launch` once, then `returned`
/// on the first launch on a later local calendar day. True when this launch
/// was the install's first.
pub fn app_launched() -> bool {
    if reach(Milestone::FirstLaunch, Vec::new()) {
        return true;
    }
    if is_reached(Milestone::Returned) {
        return false;
    }
    let Some(state_dir) = crate::recording_state_dir() else {
        return false;
    };
    let days = Activation::new(state_dir).days_since_first_launch(crate::now_ms());
    if let Some(days) = days.filter(|days| *days >= 1) {
        reach(Milestone::Returned, vec![("days", Value::from(days))]);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_790_000_000_000;

    fn fresh() -> (tempfile::TempDir, Activation) {
        let dir = tempfile::tempdir().unwrap();
        let activation = Activation::new(dir.path());
        (dir, activation)
    }

    #[test]
    fn each_milestone_is_claimed_exactly_once() {
        let (_dir, activation) = fresh();
        activation.ensure_origin(T0, || false).unwrap();
        let first = activation
            .claim(Milestone::FirstLaunch, T0 + 1_000)
            .unwrap();
        assert_eq!(
            first,
            Some(Claim {
                preexisting: false,
                since_first_launch_s: 0
            })
        );
        assert_eq!(
            activation
                .claim(Milestone::FirstLaunch, T0 + 9_000)
                .unwrap(),
            None
        );
        assert_eq!(activation.reached(Milestone::FirstLaunch), Some(T0 + 1_000));
        let session = activation
            .claim(Milestone::FirstSession, T0 + 61_000)
            .unwrap()
            .unwrap();
        assert_eq!(session.since_first_launch_s, 60);
        // A new Activation over the same files (a restart) agrees.
        let again = Activation::new(_dir.path());
        assert_eq!(
            again.claim(Milestone::FirstSession, T0 + 99_000).unwrap(),
            None
        );
    }

    #[test]
    fn first_then_second_session_and_racing_claims_get_one_each() {
        let (dir, activation) = fresh();
        activation.ensure_origin(T0, || false).unwrap();
        let order = [Milestone::FirstSession, Milestone::SecondSession];
        let winners: Vec<Option<Milestone>> = (0..8)
            .map(|_| {
                let path = dir.path().to_path_buf();
                std::thread::spawn(move || {
                    Activation::new(&path)
                        .claim_next(&order, T0)
                        .unwrap()
                        .map(|(milestone, _)| milestone)
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        let count = |m| winners.iter().filter(|w| **w == Some(m)).count();
        assert_eq!(count(Milestone::FirstSession), 1, "{winners:?}");
        assert_eq!(count(Milestone::SecondSession), 1, "{winners:?}");
        assert_eq!(winners.iter().filter(|w| w.is_none()).count(), 6);
        assert_eq!(activation.claim_next(&order, T0).unwrap(), None);
    }

    #[test]
    fn a_fresh_install_is_new_and_the_baseline_is_decided_once() {
        let (dir, activation) = fresh();
        assert!(!prior_use_evidence(dir.path(), Some(dir.path())));
        let origin = activation
            .ensure_origin(T0, || prior_use_evidence(dir.path(), Some(dir.path())))
            .unwrap();
        assert!(!origin.preexisting);
        // State written after the baseline (the Engine's first spawn, the
        // privacy notice) never turns a new user into an old one.
        std::fs::write(dir.path().join(ubra_proto::paths::STATE_FILE_NAME), "{}").unwrap();
        let later = activation.ensure_origin(T0 + 5, || true).unwrap();
        assert_eq!(later, origin);
    }

    #[test]
    fn an_upgraded_install_is_preexisting_on_every_milestone() {
        for evidence in ["telemetry/config.json", "state.json", "prefs"] {
            let (dir, activation) = fresh();
            let home = dir.path().join("home");
            if evidence == "prefs" {
                let prefs = ubra_proto::paths::UbraPaths::prefs_file(&home);
                std::fs::create_dir_all(prefs.parent().unwrap()).unwrap();
                std::fs::write(prefs, "{}").unwrap();
            } else {
                let path = dir.path().join(evidence);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, "{}").unwrap();
            }
            let origin = activation
                .ensure_origin(T0, || prior_use_evidence(dir.path(), Some(&home)))
                .unwrap();
            assert!(origin.preexisting, "{evidence}");
            let claim = activation
                .claim(Milestone::FirstLaunch, T0)
                .unwrap()
                .unwrap();
            assert!(claim.preexisting, "{evidence}");
        }
    }

    #[test]
    fn claims_need_a_baseline() {
        let (_dir, activation) = fresh();
        assert!(activation.claim(Milestone::FirstSession, T0).is_err());
        assert_eq!(activation.reached(Milestone::FirstSession), None);
    }

    #[test]
    fn returning_is_a_later_local_calendar_day() {
        let (_dir, activation) = fresh();
        activation.ensure_origin(T0, || false).unwrap();
        assert_eq!(activation.days_since_first_launch(T0), None);
        activation.claim(Milestone::FirstLaunch, T0).unwrap();
        assert_eq!(activation.days_since_first_launch(T0 + 1_000), Some(0));
        assert_eq!(
            activation.days_since_first_launch(T0 + 3 * 86_400_000),
            Some(3)
        );
    }

    #[test]
    fn milestone_names_match_their_event_kinds() {
        for milestone in Milestone::ALL {
            assert_eq!(milestone.kind(), format!("activation.{}", milestone.name()));
        }
    }
}
