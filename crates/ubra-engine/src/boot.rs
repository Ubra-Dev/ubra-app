//! When this machine last booted, so a session whose holder is gone can be
//! told apart: ended by the computer restarting, or by Ubra.

use ubra_proto::DateMillis;

/// The wall-clock time the running kernel booted, or `None` when the
/// platform will not say.
pub fn boot_time() -> Option<DateMillis> {
    platform_boot_time()
}

#[cfg(target_os = "macos")]
fn platform_boot_time() -> Option<DateMillis> {
    let mut boot = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut length = std::mem::size_of::<libc::timeval>();
    // SAFETY: sysctlbyname writes at most `length` bytes into a properly
    // sized timeval; the name is a NUL-terminated literal.
    let result = unsafe {
        libc::sysctlbyname(
            c"kern.boottime".as_ptr(),
            (&raw mut boot).cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 || length != std::mem::size_of::<libc::timeval>() || boot.tv_sec <= 0 {
        return None;
    }
    Some(DateMillis(
        boot.tv_sec as f64 * 1000.0 + f64::from(boot.tv_usec) / 1000.0,
    ))
}

#[cfg(target_os = "linux")]
fn platform_boot_time() -> Option<DateMillis> {
    proc_stat_btime(&std::fs::read_to_string("/proc/stat").ok()?)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn platform_boot_time() -> Option<DateMillis> {
    None
}

/// `btime` from `/proc/stat`: whole seconds since the epoch.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn proc_stat_btime(stat: &str) -> Option<DateMillis> {
    let seconds: u64 = stat
        .lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()?;
    (seconds > 0).then_some(DateMillis(seconds as f64 * 1000.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_btime_from_proc_stat() {
        let stat = "cpu  1 2 3 4\nintr 9\nctxt 12\nbtime 1790859000\nprocesses 7\n";
        assert_eq!(proc_stat_btime(stat), Some(DateMillis(1_790_859_000_000.0)));
        assert_eq!(proc_stat_btime("cpu 1\n"), None);
        assert_eq!(proc_stat_btime("btime nonsense\n"), None);
        assert_eq!(proc_stat_btime("btime 0\n"), None);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn this_machine_booted_in_the_past() {
        let boot = boot_time().expect("boot time");
        let now = DateMillis::from(std::time::SystemTime::now());
        assert!(boot.0 > 0.0 && boot.0 < now.0, "{boot:?} vs {now:?}");
    }
}
