//! Deterministic Engine/power integration: actual helper wire and registry,
//! explicit wall times, no runner waits, PTYs or operating-system assertions.
use super::*;
use crate::wake::{PowerConfig, Request, Response};
use std::io::Write;
use std::os::unix::net::UnixListener;
use std::sync::mpsc::{self, Receiver, Sender};

struct FakeHelper {
    requests: Receiver<(Request, Sender<Response>)>,
    _temp: tempfile::TempDir,
    power: PowerConfig,
}

impl FakeHelper {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("wake.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (tx, requests) = mpsc::channel();
        // A finite accept deadline keeps failed tests from leaking workers.
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                };
                let line = crate::wake::read_frame(&mut stream).unwrap();
                let request = serde_json::from_str(&line).unwrap();
                let (reply, response) = mpsc::channel();
                if tx.send((request, reply)).is_err() {
                    break;
                }
                let response = response.recv_timeout(Duration::from_secs(5)).unwrap();
                writeln!(stream, "{}", serde_json::to_string(&response).unwrap()).unwrap();
            }
        });
        Self {
            requests,
            power: PowerConfig {
                socket_path: path,
                caffeinate: None,
            },
            _temp: temp,
        }
    }

    fn next(&self) -> (Request, Sender<Response>) {
        self.requests
            .recv_timeout(Duration::from_secs(5))
            .expect("helper request")
    }

    fn accept(&self, expected: Request) {
        let (request, reply) = self.next();
        assert_eq!(request, expected);
        reply
            .send(Response {
                ok: true,
                ..Response::default()
            })
            .unwrap();
    }
}

fn scheduler(helper: &FakeHelper) -> Arc<Scheduler> {
    Arc::new(Scheduler {
        power: helper.power.clone(),
        ..Scheduler::default()
    })
}

fn arm(scheduler: &Scheduler, alarm: i64) {
    scheduler.state.lock().unwrap().pending.insert(
        "one".into(),
        Pending {
            next_due_ms: alarm + crate::wake::WAKE_LEAD_MS,
            keep_awake: true,
            wake_mac: true,
        },
    );
}

fn settled(scheduler: &Scheduler) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while scheduler.wakes.lock().unwrap().busy {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
}

#[test]
fn wake_sync_serializes_delayed_replies_and_clears_a_deleted_schedule() {
    let helper = FakeHelper::new();
    let scheduler = scheduler(&helper);
    let now = now_ms().div_euclid(1000) * 1000;
    arm(&scheduler, now + 300_000);
    scheduler.sync_wakes(now);
    let (first, reply) = helper.next();
    assert_eq!(
        first,
        Request::SetWakes {
            times_ms: vec![now + 300_000]
        }
    );
    arm(&scheduler, now + 600_000);
    scheduler.sync_wakes(now);
    scheduler.state.lock().unwrap().pending.clear();
    scheduler.sync_wakes(now);
    assert!(
        helper.requests.try_recv().is_err(),
        "only one in-flight request"
    );
    reply
        .send(Response {
            ok: true,
            ..Response::default()
        })
        .unwrap();
    helper.accept(Request::SetWakes { times_ms: vec![] });
    settled(&scheduler);
    assert_eq!(scheduler.wakes.lock().unwrap().sent, Some(vec![]));
    assert!(
        scheduler.wakes.lock().unwrap().accepted.is_empty(),
        "canceled alarms cannot grant sleep"
    );
}

#[test]
fn failed_sync_still_clears_and_retries_empty_desired_state() {
    let helper = FakeHelper::new();
    let scheduler = scheduler(&helper);
    let now = now_ms().div_euclid(1000) * 1000;
    arm(&scheduler, now + 300_000);
    scheduler.sync_wakes(now);
    let (_, reply) = helper.next();
    reply
        .send(Response::failure("lost acknowledgement"))
        .unwrap();
    settled(&scheduler);
    scheduler.state.lock().unwrap().pending.clear();
    scheduler.sync_wakes(now);
    let (request, reply) = helper.next();
    assert_eq!(request, Request::SetWakes { times_ms: vec![] });
    reply.send(Response::failure("try again")).unwrap();
    settled(&scheduler);
    assert!(scheduler.wake_retry_needed());
    scheduler.sync_wakes(now);
    helper.accept(Request::SetWakes { times_ms: vec![] });
    settled(&scheduler);
    assert!(!scheduler.wake_retry_needed());
}

#[test]
fn imminent_accepted_alarm_is_not_canceled_and_far_future_does_not_poison_batch() {
    let helper = FakeHelper::new();
    let scheduler = scheduler(&helper);
    let now = now_ms().div_euclid(1000) * 1000;
    let alarm = now + 300_000;
    arm(&scheduler, alarm);
    scheduler.sync_wakes(now);
    helper.accept(Request::SetWakes {
        times_ms: vec![alarm],
    });
    settled(&scheduler);
    scheduler.sync_wakes(alarm - 20_000);
    assert!(!scheduler.wakes.lock().unwrap().busy);
    scheduler.state.lock().unwrap().pending.insert(
        "far".into(),
        Pending {
            next_due_ms: now + 40 * 86_400_000,
            keep_awake: true,
            wake_mac: true,
        },
    );
    scheduler.sync_wakes(now);
    assert_eq!(scheduler.wakes.lock().unwrap().desired, vec![alarm]);
    assert!(!scheduler.wakes.lock().unwrap().busy);
}

#[test]
fn wake_attribution_requires_an_accepted_alarm_and_allows_a_late_runner_tick() {
    let scheduler = Scheduler::default();
    scheduler.wakes.lock().unwrap().accepted = vec![500_000];
    *scheduler.last_sleep.lock().unwrap() = Some((100_000, 530_000));
    assert_eq!(scheduler.wake_started(620_000), Some(500_000));
    assert_eq!(scheduler.wake_started(529_999), None, "clock reversal");
    *scheduler.last_sleep.lock().unwrap() = Some((100_000, 1_100_000));
    assert_eq!(
        scheduler.wake_started(1_120_000),
        None,
        "unrelated late catch-up"
    );
}

fn server(helper: &FakeHelper) -> Arc<super::super::ControlServer> {
    let (engine, _) =
        crate::detect::ManifestEngine::load_dir(&crate::detect::bundled_manifest_dir()).unwrap();
    let registry = Arc::new(Mutex::new(crate::registry::Registry::new(
        Arc::new(engine),
        helper._temp.path().join("state.json"),
    )));
    Arc::new(
        super::super::ControlServer::new(registry, helper._temp.path().join("engine.sock"))
            .with_schedule_power(helper.power.clone()),
    )
}

fn run(server: &super::super::ControlServer, id: &str, woke: Option<i64>, status: SessionStatus) {
    let mut record = super::super::new_record(id, "shell", "/tmp");
    record.status = status;
    server.registry.lock().unwrap().insert_record(record);
    server.scheduler.active.lock().unwrap().push(ActiveRun {
        session_id: id.into(),
        fired_ms: 120_000,
        woke,
        idle_since_ms: None,
    });
}

#[test]
fn sleeps_once_after_all_runs_finish_and_counts_input_since_wake() {
    let helper = FakeHelper::new();
    let server = server(&helper);
    run(&server, "woken", Some(0), SessionStatus::Idle);
    run(&server, "other", None, SessionStatus::Working);
    assert!(server.keep_awake_wanted(250_000));
    assert!(server.keep_awake_wanted(370_000));
    assert!(helper.requests.try_recv().is_err());
    server
        .registry
        .lock()
        .unwrap()
        .update_record("other", |r| r.status = SessionStatus::Idle);
    assert!(server.keep_awake_wanted(400_000));
    let worker = {
        let server = Arc::clone(&server);
        std::thread::spawn(move || server.keep_awake_wanted(520_001))
    };
    helper.accept(Request::SleepIfIdle {
        min_idle_secs: 521,
        idle_since_ms: Some(0),
    });
    assert!(!worker.join().unwrap());
    assert!(!server.keep_awake_wanted(600_000));
    assert!(helper.requests.try_recv().is_err());
}

#[test]
fn nonwoken_runs_caps_and_pending_spawns_never_authorize_sleep() {
    let helper = FakeHelper::new();
    let server = server(&helper);
    run(&server, "awake", None, SessionStatus::Idle);
    assert!(server.keep_awake_wanted(250_000));
    assert!(!server.keep_awake_wanted(370_000));
    run(&server, "capped", Some(0), SessionStatus::Working);
    assert!(!server.keep_awake_wanted(KEEP_AWAKE_RUN_CAP_MS + 120_000));
    assert!(server.scheduler.sleep_due.lock().unwrap().is_none());
    server.scheduler.spawning.store(1, Ordering::Release);
    *server.scheduler.sleep_due.lock().unwrap() = Some(0);
    assert!(!server.keep_awake_wanted(KEEP_AWAKE_RUN_CAP_MS + 150_000));
    assert!(helper.requests.try_recv().is_err());
}

#[test]
fn status_flapping_requires_continuous_idle_and_permission_waits_are_busy() {
    let mut run = ActiveRun {
        session_id: "x".into(),
        fired_ms: 0,
        woke: Some(0),
        idle_since_ms: None,
    };
    assert!(run.keep(Some(SessionStatus::Idle), 150_000));
    assert!(run.keep(Some(SessionStatus::Working), 180_000));
    assert!(run.keep(Some(SessionStatus::Idle), 200_000));
    assert!(run.keep(Some(SessionStatus::Idle), 319_999));
    assert!(!run.keep(Some(SessionStatus::Idle), 320_000));
    assert!(run.keep(Some(SessionStatus::Unknown), 330_000));
}

#[test]
fn restart_restores_assertions_without_reusing_sleep_permission() {
    let helper = FakeHelper::new();
    let server = server(&helper);
    let mut record = super::super::new_record("restored", "shell", "/tmp");
    record.status = SessionStatus::Working;
    record.scheduled_run = Some(ubra_proto::schedules::ScheduledRunInfo {
        schedule_id: "deleted".into(),
        title: "run".into(),
        due_at: DateMillis(0.0),
        wake_mac: true,
        woke_mac: true,
    });
    server.registry.lock().unwrap().insert_record(record);
    server.recover_scheduled_assertions();
    assert!(server.keep_awake_wanted(now_ms()));
    assert_eq!(server.scheduler.active.lock().unwrap()[0].woke, None);
    assert!(helper.requests.try_recv().is_err());
}

#[test]
fn matched_alarm_still_requires_kernel_timer_wake_evidence() {
    let helper = FakeHelper::new();
    let scheduler = scheduler(&helper);
    scheduler.wakes.lock().unwrap().accepted = vec![500_000];
    *scheduler.last_sleep.lock().unwrap() = Some((100_000, 530_000));
    for timer_wake in [false, true] {
        let worker = {
            let scheduler = Arc::clone(&scheduler);
            std::thread::spawn(move || scheduler.attributed_wake(620_000))
        };
        let (request, reply) = helper.next();
        assert_eq!(request, Request::Status);
        reply
            .send(Response {
                ok: true,
                timer_wake,
                ..Response::default()
            })
            .unwrap();
        assert_eq!(worker.join().unwrap(), timer_wake.then_some(500_000));
    }
}

#[test]
fn dark_wake_is_promoted_on_alarm_timing_without_asking_the_helper() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let command = temp.path().join("fake-caffeinate");
    std::fs::write(&command, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.args\"\n").unwrap();
    std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();
    // No helper listens here: an unrecognised or unreadable kernel wake
    // reason must not stop the run from holding the Mac awake.
    let scheduler = Scheduler {
        power: PowerConfig {
            socket_path: temp.path().join("absent.sock"),
            caffeinate: Some(command.clone()),
        },
        ..Scheduler::default()
    };
    let output = command.with_file_name("fake-caffeinate.args");

    // A wake nowhere near an armed alarm is not ours: no nudge.
    scheduler.wakes.lock().unwrap().accepted = vec![500_000];
    *scheduler.last_sleep.lock().unwrap() = Some((100_000, 900_000));
    scheduler.promote_observed_wake(900_000);
    std::thread::sleep(Duration::from_millis(200));
    assert!(!output.exists(), "promoted a wake ubra did not arm");

    *scheduler.last_sleep.lock().unwrap() = Some((100_000, 530_000));
    scheduler.promote_observed_wake(530_000);
    let deadline = Instant::now() + Duration::from_secs(2);
    while std::fs::read_to_string(&output).ok().as_deref() != Some("-u\n-t\n5\n") {
        assert!(
            Instant::now() < deadline,
            "fake promotion command did not run"
        );
        std::thread::yield_now();
    }
}
