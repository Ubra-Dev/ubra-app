//! Whether a terminal's foreground job is stopped at a question.
//!
//! A job that asks `Proceed? [y/N]`, `Password:` or a script's `read` sits
//! blocked in a `read` of the terminal. So does nothing else a quiet job does:
//! a build waits on its children, a download on a socket, `sleep` on a timer.
//! Telling those apart is the whole job of this module, and it only answers
//! "yes" when the kernel's own bookkeeping says the wait is the terminal's.
//!
//! Linux says so directly: `/proc/<pid>/task/<tid>/syscall` names the call a
//! blocked thread is in and its descriptor, which is then compared with the
//! terminal. macOS reports no wait channel to an unprivileged caller, so it is
//! read by elimination, from facts measured on real commands:
//!
//! - A thread blocked in a plain `read` (or `poll`) keeps its kernel stack,
//!   while timers, `wait4`, `select`, `kevent` and condition variables park on
//!   a continuation and give it up, which `TH_FLAGS_SWAPPED` reports. `sleep`,
//!   `make` waiting on a compiler, Go and Node event loops all fall out here.
//! - A process with a child in the group is waiting on that child: a shell's
//!   `$(…)`, Python's `subprocess.run`.
//! - A pipe being read marks itself (`PIPE_WANTR`), which covers `a | b`.
//! - Any socket is taken to be what the process waits on. A download or a
//!   server cannot be told from a question asked beside an idle connection,
//!   and missing that question is the cheaper mistake.
//!
//! A stack-holding wait left over by all of that is the terminal read.
//!
//! Every failure answers `false`: a job that cannot be inspected is not known
//! to be waiting, and a wrong "needs you" is worse than a missing one.

/// What inspecting a foreground group found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupRead {
    /// A member is blocked reading the terminal.
    Reading,
    /// Every member was inspected and none is.
    NotReading,
    /// None of the members that could be inspected is, but some could not
    /// be: another user's process, such as a setuid `sudo`.
    Uninspectable,
}

/// Whether a process in foreground group `pgid` is blocked reading the
/// terminal. The caller has already checked that the line discipline is in
/// canonical mode; raw-mode readers are editors and TUIs, not questions.
#[must_use]
pub fn group_reads_terminal(pgid: i32) -> GroupRead {
    if pgid <= 1 {
        return GroupRead::NotReading;
    }
    platform::group_reads_terminal(pgid)
}

/// No more members are inspected than this: a foreground group this large
/// is a build, not a question, and inspecting it would cost more than the
/// answer is worth.
const MAX_GROUP_MEMBERS: usize = 64;

#[cfg(target_os = "macos")]
mod platform {
    use super::{GroupRead, MAX_GROUP_MEMBERS};

    const PROC_PGRP_ONLY: u32 = 2;
    const PROC_PIDLISTTHREADS: i32 = 6;
    const PROC_PIDFDPIPEINFO: i32 = 6;

    /// `struct pipe_fdinfo` is a `proc_fileinfo` (24 bytes), then
    /// `pipe_info`: a `vinfo_stat` (136), two handles, and `pipe_status`.
    const PIPE_FDINFO_SIZE: usize = 184;
    const PIPE_STATUS_OFFSET: usize = 176;
    /// `PIPE_WANTR` from `<sys/pipe.h>`: a reader is asleep on this end.
    const PIPE_WANTR: u32 = 0x008;

    /// Descriptors past this are not inspected, and the wait counts as
    /// explained: a process with this many files open is not at a prompt.
    const MAX_DESCRIPTORS: usize = 512;
    const MAX_THREADS: usize = 256;

    pub(super) fn group_reads_terminal(pgid: i32) -> GroupRead {
        let mut pids = [0i32; MAX_GROUP_MEMBERS + 1];
        // SAFETY: the buffer is writable for exactly the size passed.
        let filled = unsafe {
            libc::proc_listpids(
                PROC_PGRP_ONLY,
                pgid as u32,
                pids.as_mut_ptr().cast(),
                std::mem::size_of_val(&pids) as i32,
            )
        };
        if filled <= 0 {
            return GroupRead::NotReading;
        }
        let count = filled as usize / std::mem::size_of::<i32>();
        if count > MAX_GROUP_MEMBERS {
            return GroupRead::NotReading;
        }
        let members: Vec<i32> = pids[..count]
            .iter()
            .copied()
            .filter(|pid| *pid > 0)
            .collect();
        let parents: Vec<Option<i32>> = members.iter().map(|pid| parent_of(*pid)).collect();
        let mut found = GroupRead::NotReading;
        for pid in &members {
            if parents.contains(&Some(*pid)) {
                continue;
            }
            match blocked_in_plain_read(*pid) {
                Some(true) if !wait_explained(*pid) => return GroupRead::Reading,
                Some(_) => {}
                None => found = GroupRead::Uninspectable,
            }
        }
        found
    }

    fn parent_of(pid: i32) -> Option<i32> {
        // SAFETY: zero is a valid `proc_bsdinfo`; the kernel fills it.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of_val(&info) as i32;
        // SAFETY: `info` is writable for `size` bytes.
        let filled = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size)
        };
        (filled == size && info.pbi_pid == pid as u32).then_some(info.pbi_ppid as i32)
    }

    /// Whether a thread is asleep while still holding its kernel stack;
    /// `None` for a process that cannot be inspected.
    fn blocked_in_plain_read(pid: i32) -> Option<bool> {
        let mut threads = vec![0u64; MAX_THREADS];
        // SAFETY: the buffer is writable for exactly the size passed.
        let filled = unsafe {
            libc::proc_pidinfo(
                pid,
                PROC_PIDLISTTHREADS,
                0,
                threads.as_mut_ptr().cast(),
                (threads.len() * std::mem::size_of::<u64>()) as i32,
            )
        };
        if filled <= 0 {
            return None;
        }
        let count = (filled as usize / std::mem::size_of::<u64>()).min(MAX_THREADS);
        Some(threads[..count].iter().any(|thread| {
            // SAFETY: zero is a valid `proc_threadinfo`; the kernel fills it.
            let mut info: libc::proc_threadinfo = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of_val(&info) as i32;
            // SAFETY: `info` is writable for `size` bytes.
            let filled = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDTHREADINFO,
                    *thread,
                    (&raw mut info).cast(),
                    size,
                )
            };
            filled == size
                && info.pth_run_state == libc::TH_STATE_WAITING
                && info.pth_flags & libc::TH_FLAGS_SWAPPED == 0
        }))
    }

    /// Whether a pipe or socket of `pid` accounts for its blocked thread, or
    /// the process could not be read well enough to rule that out.
    fn wait_explained(pid: i32) -> bool {
        let mut fds: Vec<libc::proc_fdinfo> = Vec::with_capacity(MAX_DESCRIPTORS + 1);
        let bytes = (MAX_DESCRIPTORS + 1) * std::mem::size_of::<libc::proc_fdinfo>();
        // SAFETY: the vector's spare capacity is writable for `bytes`; only
        // the prefix the kernel reports filled is exposed below.
        let filled = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                fds.as_mut_ptr().cast(),
                bytes as i32,
            )
        };
        if filled <= 0 {
            return true;
        }
        let count = filled as usize / std::mem::size_of::<libc::proc_fdinfo>();
        if count > MAX_DESCRIPTORS {
            return true;
        }
        // SAFETY: the kernel initialized `count` entries.
        unsafe { fds.set_len(count) };
        fds.iter().any(|fd| match fd.proc_fdtype as i32 {
            libc::PROX_FDTYPE_PIPE => pipe_being_read(pid, fd.proc_fd),
            libc::PROX_FDTYPE_SOCKET => true,
            _ => false,
        })
    }

    fn pipe_being_read(pid: i32, fd: i32) -> bool {
        let mut info = [0u8; PIPE_FDINFO_SIZE];
        // SAFETY: the buffer is writable for exactly its size.
        let filled = unsafe {
            libc::proc_pidfdinfo(
                pid,
                fd,
                PROC_PIDFDPIPEINFO,
                info.as_mut_ptr().cast(),
                PIPE_FDINFO_SIZE as i32,
            )
        };
        // A pipe that closed in between cannot be what the thread waits on.
        filled as usize == PIPE_FDINFO_SIZE && {
            let status = u32::from_ne_bytes(
                info[PIPE_STATUS_OFFSET..PIPE_STATUS_OFFSET + 4]
                    .try_into()
                    .expect("four bytes"),
            );
            status & PIPE_WANTR != 0
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::{GroupRead, MAX_GROUP_MEMBERS};
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    /// `read`, `readv` and `pread64`, which is how line prompts read.
    #[cfg(target_arch = "x86_64")]
    const READ_SYSCALLS: &[i64] = &[0, 19, 17];
    #[cfg(target_arch = "aarch64")]
    const READ_SYSCALLS: &[i64] = &[63, 65, 67];
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    const READ_SYSCALLS: &[i64] = &[];

    /// `/dev/tty`, which `sudo`, `ssh` and `git` open to ask their questions.
    const DEV_TTY: (u64, u64) = (5, 0);

    pub(super) fn group_reads_terminal(pgid: i32) -> GroupRead {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return GroupRead::NotReading;
        };
        let mut members = 0;
        let mut found = GroupRead::NotReading;
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            let Some((group, tty)) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .as_deref()
                .and_then(super::group_and_tty_from_stat)
            else {
                continue;
            };
            if group != pgid {
                continue;
            }
            members += 1;
            if members > MAX_GROUP_MEMBERS {
                return GroupRead::NotReading;
            }
            match reads_terminal(pid, tty) {
                Some(true) => return GroupRead::Reading,
                Some(false) => {}
                None => found = GroupRead::Uninspectable,
            }
        }
        found
    }

    /// `None` when a thread's syscall cannot be read: `/proc/<pid>/syscall`
    /// needs ptrace access, which another user's process (`sudo`) denies.
    fn reads_terminal(pid: u32, tty: (u64, u64)) -> Option<bool> {
        let tasks = std::fs::read_dir(format!("/proc/{pid}/task")).ok()?;
        let mut reading = Some(false);
        for task in tasks.flatten() {
            let Ok(line) = std::fs::read_to_string(task.path().join("syscall")) else {
                reading = None;
                continue;
            };
            let Some(fd) = super::read_fd_from_syscall(&line, READ_SYSCALLS) else {
                continue;
            };
            let terminal = std::fs::metadata(format!("/proc/{pid}/fd/{fd}")).is_ok_and(|meta| {
                let device = (
                    libc::major(meta.rdev()) as u64,
                    libc::minor(meta.rdev()) as u64,
                );
                meta.file_type().is_char_device() && (device == tty || device == DEV_TTY)
            });
            if terminal {
                return Some(true);
            }
        }
        reading
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    pub(super) fn group_reads_terminal(_: i32) -> super::GroupRead {
        super::GroupRead::NotReading
    }
}

/// The process group and controlling terminal (major, minor) from a
/// `/proc/<pid>/stat` line. The command name may hold spaces and parentheses,
/// so fields are counted from the last `)`.
#[cfg(any(target_os = "linux", test))]
fn group_and_tty_from_stat(stat: &str) -> Option<(i32, (u64, u64))> {
    let mut fields = stat.get(stat.rfind(')')? + 2..)?.split_whitespace();
    // state, ppid, pgrp, session, tty_nr
    let pgrp: i32 = fields.nth(2)?.parse().ok()?;
    let tty: u64 = fields.nth(1)?.parse::<i64>().ok()?.try_into().ok()?;
    if tty == 0 {
        return None;
    }
    // The kernel's `new_encode_dev`: minor's low byte, major, minor's rest.
    let major = (tty >> 8) & 0xfff;
    let minor = (tty & 0xff) | ((tty >> 12) & 0xfff00);
    Some((pgrp, (major, minor)))
}

/// The descriptor a blocked thread is reading, from its `syscall` file:
/// the call number, then its arguments in hex. A running thread reads
/// `running`, and one outside a syscall `-1 …`.
#[cfg(any(target_os = "linux", test))]
fn read_fd_from_syscall(line: &str, reads: &[i64]) -> Option<i32> {
    let mut fields = line.split_whitespace();
    let number: i64 = fields.next()?.parse().ok()?;
    if !reads.contains(&number) {
        return None;
    }
    let fd = fields.next()?.strip_prefix("0x")?;
    i32::from_str_radix(fd, 16).ok().filter(|fd| *fd >= 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_stat_yields_the_group_and_the_terminal() {
        // pts/3 is major 136, minor 3; pts/300 spills into the high bits.
        let stat = "4242 (my (odd) prog) S 4200 4242 4200 34819 4242 4194560 0 0";
        assert_eq!(group_and_tty_from_stat(stat), Some((4242, (136, 3))));
        let high = (300u64 & 0xff) | (136 << 8) | ((300u64 & !0xff) << 12);
        let stat = format!("7 (read) S 1 7 1 {high} 7 0");
        assert_eq!(group_and_tty_from_stat(&stat), Some((7, (136, 300))));
        assert_eq!(group_and_tty_from_stat("7 (daemon) S 1 7 7 0 -1 0"), None);
    }

    #[test]
    fn syscall_lines_name_the_descriptor_only_for_reads() {
        let reads = [0, 19, 17];
        assert_eq!(
            read_fd_from_syscall("0 0x0 0x7ffd 0x1 0x0 0x0 0x0 0x7ffd 0x7f00", &reads),
            Some(0)
        );
        assert_eq!(
            read_fd_from_syscall("0 0x3 0x7ffd 0x1000 0x0 0x0 0x0 0x7ffd 0x7f00", &reads),
            Some(3)
        );
        // wait4, nanosleep, a running thread, one outside any call.
        assert_eq!(
            read_fd_from_syscall("61 0xffffffffffffffff 0x0", &reads),
            None
        );
        assert_eq!(read_fd_from_syscall("35 0x7ffd 0x0", &reads), None);
        assert_eq!(read_fd_from_syscall("running", &reads), None);
        assert_eq!(read_fd_from_syscall("-1 0x7ffd 0x7f00", &reads), None);
    }

    #[test]
    fn a_group_that_does_not_exist_is_not_waiting() {
        assert_eq!(group_reads_terminal(0), GroupRead::NotReading);
        assert_eq!(group_reads_terminal(i32::MAX - 7), GroupRead::NotReading);
    }
}
