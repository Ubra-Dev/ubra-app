//! Process resource sampling and the periodic `health` / `metrics` events.
//!
//! The sampler is for long-lived interactive processes (app, Engine). A
//! Holder must not start it: an idle Holder may not wake on a timer.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::value::Value;

pub type Gauge = Box<dyn Fn() -> Value + Send>;

static GAUGES: Mutex<Vec<(&'static str, Gauge)>> = Mutex::new(Vec::new());

/// Adds a process-specific value to every `health` event, e.g. live
/// sessions, open windows, attached clients. The closure runs on the sampler
/// thread; keep it cheap and lock-light.
pub fn register_gauge(name: &'static str, gauge: impl Fn() -> Value + Send + 'static) {
    if let Ok(mut gauges) = GAUGES.lock() {
        gauges.retain(|(existing, _)| *existing != name);
        gauges.push((name, Box::new(gauge)));
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ProcessStats {
    pub resident_bytes: u64,
    /// macOS `phys_footprint`: what Activity Monitor calls Memory. Equal to
    /// resident on other platforms.
    pub footprint_bytes: u64,
    pub cpu_ms: u64,
    pub threads: u64,
    pub open_fds: u64,
    /// The soft `RLIMIT_NOFILE`, so fd growth can be read against it.
    pub fd_limit: u64,
}

impl ProcessStats {
    #[must_use]
    pub fn current() -> Self {
        let mut stats = Self {
            cpu_ms: cpu_ms(),
            open_fds: open_fds(),
            fd_limit: fd_limit(),
            ..Self::default()
        };
        platform_memory(&mut stats);
        stats
    }
}

/// Starts the sampler thread: one `health` event (and one `metrics` event
/// when anything was counted) every `interval`.
pub fn start_health_sampler(interval: Duration) {
    if !crate::is_enabled() {
        return;
    }
    let started = Instant::now();
    let _ = std::thread::Builder::new()
        .name("ubra-telemetry-health".into())
        .spawn(move || {
            let mut last = (Instant::now(), cpu_ms());
            loop {
                std::thread::sleep(interval);
                last = sample(started, last);
            }
        });
}

fn sample(started: Instant, (last_at, last_cpu): (Instant, u64)) -> (Instant, u64) {
    const MIB: f64 = 1024.0 * 1024.0;
    let now = Instant::now();
    let stats = ProcessStats::current();
    let wall_ms = now.duration_since(last_at).as_secs_f64() * 1000.0;
    let cpu_pct = if wall_ms > 0.0 {
        ((stats.cpu_ms.saturating_sub(last_cpu)) as f64 / wall_ms * 1000.0).round() / 10.0
    } else {
        0.0
    };
    let round = |v: f64| (v * 10.0).round() / 10.0;
    let mut fields = vec![
        ("uptime_s", Value::from(started.elapsed().as_secs())),
        (
            "rss_mb",
            Value::from(round(stats.resident_bytes as f64 / MIB)),
        ),
        (
            "footprint_mb",
            Value::from(round(stats.footprint_bytes as f64 / MIB)),
        ),
        ("cpu_pct", Value::from(cpu_pct)),
        ("cpu_ms", Value::from(stats.cpu_ms)),
        ("threads", Value::from(stats.threads)),
        ("fds", Value::from(stats.open_fds)),
        ("fd_limit", Value::from(stats.fd_limit)),
    ];
    if let Ok(gauges) = GAUGES.lock() {
        for (name, gauge) in gauges.iter() {
            fields.push((name, gauge()));
        }
    }
    crate::record("health", crate::Severity::Info, fields);
    if let Some((counters, histograms)) = crate::metrics::drain() {
        crate::record(
            "metrics",
            crate::Severity::Info,
            vec![
                ("window_s", Value::from((wall_ms / 1000.0).round())),
                ("counters", Value::Obj(counters)),
                ("timings", Value::Obj(histograms)),
            ],
        );
    }
    (now, stats.cpu_ms)
}

fn cpu_ms() -> u64 {
    #[cfg(unix)]
    {
        // SAFETY: getrusage fills a caller-owned struct.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } == 0 {
            let ms = |tv: libc::timeval| tv.tv_sec as u64 * 1000 + tv.tv_usec as u64 / 1000;
            return ms(usage.ru_utime) + ms(usage.ru_stime);
        }
    }
    0
}

fn open_fds() -> u64 {
    let dir = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    // read_dir itself holds one descriptor while counting.
    std::fs::read_dir(dir).map_or(0, |entries| entries.count().saturating_sub(1) as u64)
}

fn fd_limit() -> u64 {
    #[cfg(unix)]
    {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: getrlimit fills a caller-owned struct.
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0 {
            #[allow(clippy::unnecessary_cast)] // rlim_t is not u64 everywhere.
            return limit.rlim_cur as u64;
        }
    }
    0
}

#[cfg(target_os = "macos")]
fn platform_memory(stats: &mut ProcessStats) {
    // SAFETY: proc_pidinfo writes at most `size` bytes into the struct.
    let pid = std::process::id() as libc::c_int;
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKINFO,
            0,
            std::ptr::from_mut(&mut info).cast(),
            size,
        )
    };
    if written == size {
        stats.resident_bytes = info.pti_resident_size;
        stats.threads = u64::try_from(info.pti_threadnum).unwrap_or(0);
    }
    stats.footprint_bytes = phys_footprint(pid).unwrap_or(stats.resident_bytes);
}

#[cfg(target_os = "macos")]
fn phys_footprint(pid: libc::c_int) -> Option<u64> {
    // rusage_info_v2 up to ri_phys_footprint; the kernel fills the whole v2
    // struct, so the buffer is the full layout.
    #[repr(C)]
    struct RusageInfoV2 {
        head: [u64; 9], // ri_uuid (16 bytes) .. ri_resident_size
        ri_phys_footprint: u64,
        tail: [u64; 10],
    }
    const RUSAGE_INFO_V2: libc::c_int = 2;
    unsafe extern "C" {
        fn proc_pid_rusage(
            pid: libc::c_int,
            flavor: libc::c_int,
            buffer: *mut libc::c_void,
        ) -> libc::c_int;
    }
    // SAFETY: the buffer is a writable struct of rusage_info_v2's size.
    let mut info: RusageInfoV2 = unsafe { std::mem::zeroed() };
    let rc = unsafe { proc_pid_rusage(pid, RUSAGE_INFO_V2, std::ptr::from_mut(&mut info).cast()) };
    (rc == 0).then_some(info.ri_phys_footprint)
}

#[cfg(target_os = "linux")]
fn platform_memory(stats: &mut ProcessStats) {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return;
    };
    for line in status.lines() {
        let mut parts = line.split_whitespace();
        match (
            parts.next(),
            parts.next().and_then(|v| v.parse::<u64>().ok()),
        ) {
            (Some("VmRSS:"), Some(kib)) => stats.resident_bytes = kib * 1024,
            (Some("Threads:"), Some(n)) => stats.threads = n,
            _ => {}
        }
    }
    stats.footprint_bytes = stats.resident_bytes;
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn platform_memory(_stats: &mut ProcessStats) {}

#[cfg(test)]
mod tests {
    use super::ProcessStats;

    #[test]
    fn samples_this_process() {
        let stats = ProcessStats::current();
        assert!(stats.resident_bytes > 0);
        assert!(stats.footprint_bytes > 0);
        assert!(stats.threads >= 1);
        assert!(stats.open_fds >= 3);
        assert!(stats.fd_limit >= stats.open_fds);
    }
}
