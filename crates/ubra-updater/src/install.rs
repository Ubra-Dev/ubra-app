//! Unpacking a verified update and swapping it in.
//!
//! A process cannot reliably delete the bundle it is executing out of, so the
//! swap runs from a detached `/bin/sh` helper that waits for ubra to exit
//! first. The helper unpacks the new bundle beside the old one while ubra is
//! still running, then swaps them by renaming, and puts the old one back if
//! anything fails, so an interrupted install leaves a working app rather than
//! a hole where one used to be.
//!
//! The swap must be renames only: when ubra exits, macOS reads the bundle at
//! its path to decide whether the processes ubra started may keep running. A
//! path that is missing or half unpacked at that moment gets the Engine, every
//! Holder, and every Agent terminated.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt as _;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::{Result, UpdateError};
use crate::net::MAX_ARCHIVE_BYTES;

/// Seconds the helper waits for ubra to exit before giving up untouched.
const EXIT_GRACE_SECONDS: u32 = 60;
const MAX_EXPANDED_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: u64 = 100_000;
const MAX_ZIP_COMMENT_BYTES: u64 = 65_535;

/// Expands the downloaded zip and returns the `.app` inside it.
pub fn unpack(archive: &Path, into: &Path) -> Result<PathBuf> {
    validate_zip_limits(archive)?;
    if into.exists() {
        fs::remove_dir_all(into)?;
    }
    fs::create_dir_all(into)?;
    // `ditto -x -k` is the counterpart of the `ditto -c -k` that built the
    // archive: unlike `unzip` it preserves the extended attributes and symlink
    // layout a signed bundle's seal depends on.
    let output = Command::new("/usr/bin/ditto")
        .arg("-x")
        .arg("-k")
        .arg(archive)
        .arg(into)
        .output()?;
    if !output.status.success() {
        let _ = fs::remove_dir_all(into);
        return Err(UpdateError::tool(
            "ditto",
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    if let Err(error) = validate_expanded_limits(into) {
        let _ = fs::remove_dir_all(into);
        return Err(error);
    }
    find_app(into)
}

fn validate_zip_limits(archive: &Path) -> Result<()> {
    const EOCD_SIGNATURE: [u8; 4] = *b"PK\x05\x06";
    const CENTRAL_SIGNATURE: [u8; 4] = *b"PK\x01\x02";
    const EOCD_BYTES: u64 = 22;
    const CENTRAL_HEADER_BYTES: usize = 46;

    let mut file = fs::File::open(archive)?;
    let archive_len = file.metadata()?.len();
    if archive_len == 0 || archive_len > MAX_ARCHIVE_BYTES {
        return Err(UpdateError::Integrity(format!(
            "update archive must be between 1 and {MAX_ARCHIVE_BYTES} bytes"
        )));
    }
    let tail_len = archive_len.min(EOCD_BYTES + MAX_ZIP_COMMENT_BYTES);
    file.seek(SeekFrom::End(-i64::try_from(tail_len).unwrap_or(i64::MAX)))?;
    let mut tail = vec![0_u8; usize::try_from(tail_len).unwrap_or(usize::MAX)];
    file.read_exact(&mut tail)?;
    let eocd_index = (0..=tail.len().saturating_sub(4))
        .rev()
        .find(|index| {
            tail[*index..].starts_with(&EOCD_SIGNATURE)
                && tail.len().saturating_sub(*index) >= usize::try_from(EOCD_BYTES).unwrap()
                && *index
                    + usize::try_from(EOCD_BYTES).unwrap()
                    + usize::from(le_u16(&tail[*index..], 20))
                    == tail.len()
        })
        .ok_or_else(|| UpdateError::Integrity("archive has no ZIP directory".to_owned()))?;
    if tail.len().saturating_sub(eocd_index) < usize::try_from(EOCD_BYTES).unwrap() {
        return Err(UpdateError::Integrity(
            "archive has a truncated ZIP directory".to_owned(),
        ));
    }
    let eocd = &tail[eocd_index..];
    let comment_len = usize::from(le_u16(eocd, 20));
    if eocd_index + usize::try_from(EOCD_BYTES).unwrap() + comment_len != tail.len() {
        return Err(UpdateError::Integrity(
            "archive has trailing data after its ZIP directory".to_owned(),
        ));
    }
    let disk = le_u16(eocd, 4);
    let directory_disk = le_u16(eocd, 6);
    let entries_on_disk = le_u16(eocd, 8);
    let entries = le_u16(eocd, 10);
    let directory_size = u64::from(le_u32(eocd, 12));
    let directory_offset = u64::from(le_u32(eocd, 16));
    if disk != 0
        || directory_disk != 0
        || entries_on_disk != entries
        || entries == u16::MAX
        || directory_size == u64::from(u32::MAX)
        || directory_offset == u64::from(u32::MAX)
    {
        return Err(UpdateError::Integrity(
            "multi-disk and ZIP64 update archives are not accepted".to_owned(),
        ));
    }
    let entries = u64::from(entries);
    if entries > MAX_ARCHIVE_ENTRIES {
        return Err(UpdateError::Integrity(
            "archive has too many entries".to_owned(),
        ));
    }
    let directory_end = directory_offset
        .checked_add(directory_size)
        .ok_or_else(|| UpdateError::Integrity("archive directory overflows".to_owned()))?;
    let eocd_offset = archive_len - tail_len + u64::try_from(eocd_index).unwrap_or(u64::MAX);
    if directory_end != eocd_offset || directory_end > archive_len {
        return Err(UpdateError::Integrity(
            "archive directory points outside the download".to_owned(),
        ));
    }

    file.seek(SeekFrom::Start(directory_offset))?;
    let mut expanded = 0_u64;
    for _ in 0..entries {
        let mut header = [0_u8; CENTRAL_HEADER_BYTES];
        file.read_exact(&mut header)?;
        if header[..4] != CENTRAL_SIGNATURE {
            return Err(UpdateError::Integrity(
                "archive contains a malformed directory entry".to_owned(),
            ));
        }
        let uncompressed = u64::from(le_u32(&header, 24));
        if uncompressed == u64::from(u32::MAX) {
            return Err(UpdateError::Integrity(
                "ZIP64 update entries are not accepted".to_owned(),
            ));
        }
        expanded = expanded
            .checked_add(uncompressed)
            .ok_or_else(|| UpdateError::Integrity("expanded archive size overflows".to_owned()))?;
        if expanded > MAX_EXPANDED_BYTES {
            return Err(UpdateError::Integrity(format!(
                "archive expands beyond {MAX_EXPANDED_BYTES} bytes"
            )));
        }
        if zip_entry_is_symlink(&header) {
            return Err(UpdateError::Integrity(
                "archive contains a symbolic link".to_owned(),
            ));
        }
        let name_len = usize::from(le_u16(&header, 28));
        let extra_len = u64::from(le_u16(&header, 30));
        let entry_comment_len = u64::from(le_u16(&header, 32));
        let mut name = vec![0_u8; name_len];
        file.read_exact(&mut name)?;
        validate_zip_entry_name(&name)?;
        file.seek(SeekFrom::Current(
            i64::try_from(extra_len + entry_comment_len).unwrap_or(i64::MAX),
        ))?;
        if file.stream_position()? > directory_end {
            return Err(UpdateError::Integrity(
                "archive directory entry exceeds its bounds".to_owned(),
            ));
        }
    }
    if file.stream_position()? != directory_end {
        return Err(UpdateError::Integrity(
            "archive directory contains unaccounted data".to_owned(),
        ));
    }
    Ok(())
}

fn validate_zip_entry_name(name: &[u8]) -> Result<()> {
    let name = std::str::from_utf8(name)
        .map_err(|_| UpdateError::Integrity("archive path is not UTF-8".to_owned()))?;
    if name.is_empty() || name.contains(['\\', '\0']) {
        return Err(UpdateError::Integrity("archive path is unsafe".to_owned()));
    }
    let path = Path::new(name);
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::RootDir))
    {
        return Err(UpdateError::Integrity(format!(
            "archive path escapes staging: {name:?}"
        )));
    }
    Ok(())
}

fn validate_expanded_limits(root: &Path) -> Result<()> {
    let mut pending = vec![root.to_owned()];
    let mut entries = 0_u64;
    let mut bytes = 0_u64;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            entries += 1;
            if entries > MAX_ARCHIVE_ENTRIES {
                return Err(UpdateError::Integrity(
                    "archive has too many entries".to_owned(),
                ));
            }
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_symlink() {
                return Err(UpdateError::Integrity(
                    "archive expanded a symbolic link".to_owned(),
                ));
            } else if file_type.is_file() {
                bytes = bytes.saturating_add(entry.metadata()?.len());
                if bytes > MAX_EXPANDED_BYTES {
                    return Err(UpdateError::Integrity(format!(
                        "archive expands beyond {MAX_EXPANDED_BYTES} bytes"
                    )));
                }
            }
        }
    }
    Ok(())
}

fn zip_entry_is_symlink(central_header: &[u8]) -> bool {
    const UNIX_FILE_TYPE_MASK: u32 = 0o170000;
    const UNIX_SYMLINK: u32 = 0o120000;
    let unix_mode = le_u32(central_header, 38) >> 16;
    unix_mode & UNIX_FILE_TYPE_MASK == UNIX_SYMLINK
}

fn le_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn le_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn find_app(directory: &Path) -> Result<PathBuf> {
    let mut apps = fs::read_dir(directory)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|extension| extension == "app"))
        .collect::<Vec<_>>();
    match apps.len() {
        1 => Ok(apps.remove(0)),
        0 => Err(UpdateError::Integrity(
            "the update archive contains no .app bundle".to_owned(),
        )),
        count => Err(UpdateError::Integrity(format!(
            "the update archive contains {count} .app bundles"
        ))),
    }
}

/// Starts the swap helper and returns once it is running.
///
/// The caller must quit immediately afterwards: the helper is already polling
/// for this process to disappear.
pub fn launch_installer(
    staged_app: &Path,
    target: &Path,
    script_dir: &Path,
    relaunch: bool,
) -> Result<()> {
    fs::create_dir_all(script_dir)?;
    let script_path = script_dir.join("install.sh");
    let log_path = script_dir.join("install.log");
    fs::write(
        &script_path,
        installer_script(std::process::id(), staged_app, target, relaunch),
    )?;

    let log = fs::File::create(&log_path)?;
    let errors = log.try_clone()?;
    Command::new("/bin/sh")
        .arg(&script_path)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(errors)
        // Its own process group, so the helper is not swept up in whatever
        // signals accompany ubra's exit.
        .process_group(0)
        .spawn()?;
    Ok(())
}

/// The helper, as text. Pure so the ordering guarantees can be asserted in
/// tests and read without running an install.
pub fn installer_script(pid: u32, staged_app: &Path, target: &Path, relaunch: bool) -> String {
    let staged = shell_quote(&staged_app.to_string_lossy());
    let target = shell_quote(&target.to_string_lossy());
    let attempts = EXIT_GRACE_SECONDS * 10;
    let after_success = if relaunch {
        "        exec /usr/bin/open \"$target\""
    } else {
        "        exit 0"
    };
    let after_restore = if relaunch {
        "/usr/bin/open \"$target\""
    } else {
        ":"
    };
    format!(
        r#"#!/bin/sh
# Generated by ubra's updater. Swaps the app bundle once ubra has exited.
set -u

pid={pid}
staged={staged}
target={target}
next="$target.ubra-next"
backup="$target.ubra-previous"

# Copy first, beside the live bundle, while ubra is still running. The moment
# ubra exits, macOS reads the bundle at its path to decide whether the
# processes ubra started may outlive it; a path that is missing or half
# unpacked then gets the Engine, every Holder and every Agent terminated. So
# after the exit, the path may only change by rename.
rm -rf "$next"
staged_ok=1
if ! /usr/bin/ditto "$staged" "$next"; then
    echo "could not unpack $staged; keeping the current app" >&2
    rm -rf "$next"
    staged_ok=0
fi

waited=0
while kill -0 "$pid" 2>/dev/null; do
    waited=$((waited + 1))
    if [ "$waited" -gt {attempts} ]; then
        echo "ubra (pid $pid) is still running after {EXIT_GRACE_SECONDS}s; not touching $target" >&2
        rm -rf "$next"
        exit 1
    fi
    sleep 0.1
done

if [ "$staged_ok" -eq 1 ]; then
    rm -rf "$backup"
    if [ -e "$target" ] && ! mv "$target" "$backup"; then
        echo "could not move $target aside; nothing was changed" >&2
        rm -rf "$next"
    elif mv "$next" "$target"; then
        rm -rf "$backup"
{after_success}
    else
        echo "install failed; restoring the previous bundle" >&2
        rm -rf "$target"
        if [ -e "$backup" ]; then
            mv "$backup" "$target"
        fi
        rm -rf "$next"
    fi
fi

{after_restore}
exit 1
"#
    )
}

/// Wraps a path in single quotes for `/bin/sh`, which is the only escaping a
/// POSIX shell needs and the only one that is safe for arbitrary bytes.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_paths_cannot_escape_staging() {
        assert!(validate_zip_entry_name(b"ubra.app/Contents/MacOS/ubra").is_ok());
        for name in [
            b"../Applications/ubra.app".as_slice(),
            b"/Applications/ubra.app".as_slice(),
            b"dir\\file".as_slice(),
            b"bad\0name".as_slice(),
        ] {
            assert!(validate_zip_entry_name(name).is_err(), "{name:?}");
        }
    }

    #[test]
    fn archive_symlinks_are_rejected_before_extraction() {
        let mut header = [0_u8; 46];
        let external_attributes = (0o120777_u32) << 16;
        header[38..42].copy_from_slice(&external_attributes.to_le_bytes());
        assert!(zip_entry_is_symlink(&header));
    }

    fn script() -> String {
        installer_script(
            4242,
            Path::new("/Users/giga/Library/Caches/ubra/updates/0.2.0/ubra.app"),
            Path::new("/Applications/ubra.app"),
            true,
        )
    }

    #[test]
    fn waits_for_the_running_app_before_touching_its_bundle() {
        let script = script();
        let wait = script.find("kill -0").expect("a wait loop");
        let mutate = script.find("mv \"$target\"").expect("the rename");
        assert!(wait < mutate, "the swap must come after the wait loop");
        assert!(script.contains("pid=4242"));
    }

    #[test]
    fn unpacks_beside_the_app_before_it_exits_and_swaps_by_rename() {
        let script = script();
        let unpack = script
            .find("/usr/bin/ditto \"$staged\" \"$next\"")
            .expect("ditto");
        let wait = script.find("kill -0").expect("a wait loop");
        let aside = script.find("mv \"$target\" \"$backup\"").expect("rename");
        let swap = script.find("mv \"$next\" \"$target\"").expect("swap");
        assert!(unpack < wait, "the slow copy happens while ubra still runs");
        assert!(wait < aside && aside < swap);
        assert_eq!(
            script.matches("/usr/bin/ditto").count(),
            1,
            "nothing is unpacked into the app's path"
        );
        // The old bundle is only discarded once the new one is in place.
        let discard = script
            .find("rm -rf \"$backup\"\n        exec")
            .expect("cleanup");
        assert!(swap < discard);
    }

    #[test]
    fn restores_the_previous_bundle_when_the_swap_fails() {
        let script = script();
        assert!(script.contains("mv \"$backup\" \"$target\""));
        assert!(script.contains("restoring the previous bundle"));
    }

    #[test]
    fn relaunches_the_app_on_both_paths() {
        assert_eq!(script().matches("/usr/bin/open \"$target\"").count(), 2);
    }

    #[test]
    fn quotes_paths_that_contain_spaces_and_quotes() {
        let script = installer_script(
            1,
            Path::new("/tmp/it's here/ubra.app"),
            Path::new("/Applications/My Apps/ubra.app"),
            true,
        );
        assert!(script.contains(r"staged='/tmp/it'\''s here/ubra.app'"));
        assert!(script.contains("target='/Applications/My Apps/ubra.app'"));
    }

    #[test]
    fn the_generated_script_is_valid_shell() {
        let output = Command::new("/bin/sh")
            .arg("-n")
            .arg("-c")
            .arg(script())
            .output()
            .expect("sh runs");
        assert!(
            output.status.success(),
            "sh -n rejected the script: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn a_normal_quit_installs_without_relaunching_the_app() {
        let script = installer_script(
            4242,
            Path::new("/tmp/staged/ubra.app"),
            Path::new("/Applications/ubra.app"),
            false,
        );
        assert!(!script.contains("/usr/bin/open"));
        assert!(script.contains("rm -rf \"$backup\"\n        exit 0"));
    }

    /// Runs the real helper against throwaway bundles.
    ///
    /// Only the relaunch is stubbed out — a test must not hand a bundle to
    /// LaunchServices — so the wait, rename, unpack, and restore paths are the
    /// shipped ones. `pid` is a process that has already exited, which makes
    /// the wait loop fall through immediately.
    #[cfg(target_os = "macos")]
    fn run_installer(staged: &Path, target: &Path, pid: u32) -> std::process::Output {
        let script =
            installer_script(pid, staged, target, true).replace("/usr/bin/open", "echo relaunched");
        Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .output()
            .expect("sh runs")
    }

    /// A pid that is guaranteed not to be running: spawn something trivial and
    /// reap it.
    #[cfg(target_os = "macos")]
    fn dead_pid() -> u32 {
        let mut child = Command::new("/usr/bin/true").spawn().expect("spawn");
        let pid = child.id();
        child.wait().expect("reap");
        pid
    }

    fn fake_bundle(root: &Path, name: &str, marker: &str) -> PathBuf {
        let bundle = root.join(name);
        fs::create_dir_all(bundle.join("Contents/MacOS")).expect("layout");
        fs::write(bundle.join("Contents/MacOS/ubra"), marker).expect("binary");
        bundle
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_helper_actually_replaces_the_bundle() {
        let directory = tempfile::tempdir().expect("temp dir");
        let staged = fake_bundle(directory.path(), "staged.app", "new");
        let target = fake_bundle(directory.path(), "ubra.app", "old");

        let output = run_installer(&staged, &target, dead_pid());
        assert!(
            output.status.success(),
            "installer failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read_to_string(target.join("Contents/MacOS/ubra")).expect("read"),
            "new"
        );
        assert!(
            !directory.path().join("ubra.app.ubra-previous").exists(),
            "the backup is discarded once the swap succeeds"
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("relaunched"));
    }

    /// The moment ubra exits, macOS asks Background Task Management whether
    /// the bundle at its path may keep running in the background. If nothing
    /// readable is there, the answer is an error and loginwindow terminates
    /// every process ubra started — the Engine, the Holders, and the Agents.
    /// So once the app is gone, its path may only ever be absent for the
    /// instant between two renames, never for the length of an unpack.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_bundle_path_is_never_missing_for_an_unpack_after_ubra_exits() {
        use std::time::{Duration, Instant};

        let directory = tempfile::tempdir().expect("temp dir");
        let staged = fake_bundle(directory.path(), "staged.app", "new");
        // Enough files that copying them takes far longer than a rename.
        let resources = staged.join("Contents/Resources");
        fs::create_dir_all(&resources).expect("resources");
        for index in 0..3000 {
            fs::write(resources.join(format!("r{index}")), [0_u8; 512]).expect("resource");
        }
        fs::write(staged.join("Contents/Info.plist"), "plist").expect("plist");
        let target = fake_bundle(directory.path(), "ubra.app", "old");
        fs::create_dir_all(target.join("Contents/Resources")).expect("resources");
        for index in [0, 1500, 2999] {
            fs::write(target.join(format!("Contents/Resources/r{index}")), "").expect("resource");
        }
        fs::write(target.join("Contents/Info.plist"), "plist").expect("plist");
        // What macOS reads: a bundle that is all there, not a directory that
        // `ditto` has created and is still filling.
        let complete = |bundle: &Path| {
            [
                "Contents/Info.plist",
                "Contents/MacOS/ubra",
                "Contents/Resources/r0",
                "Contents/Resources/r1500",
                "Contents/Resources/r2999",
            ]
            .iter()
            .all(|file| bundle.join(file).exists())
        };

        let mut app = Command::new("/bin/sleep").arg("30").spawn().expect("app");
        let script = installer_script(app.id(), &staged, &target, true)
            .replace("/usr/bin/open", "echo relaunched");
        let mut helper = Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("helper");

        // The expensive copy happens while ubra still runs, beside the live
        // bundle, which stays untouched.
        let next = directory.path().join("ubra.app.ubra-next");
        let staging = Instant::now() + Duration::from_secs(10);
        while !next.join("Contents/MacOS/ubra").exists() && Instant::now() < staging {
            std::thread::sleep(Duration::from_millis(5));
        }
        let staged_while_running = next.join("Contents/MacOS/ubra").exists();
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(
            fs::read_to_string(target.join("Contents/MacOS/ubra")).expect("read"),
            "old",
            "the running app's bundle is never touched before it exits"
        );

        app.kill().expect("quit the app");
        app.wait().expect("reap the app");
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut longest_gap = Duration::ZERO;
        let mut missing_since: Option<Instant> = None;
        let finished = loop {
            let now = Instant::now();
            match (complete(&target), missing_since) {
                (false, None) => missing_since = Some(now),
                (true, Some(since)) => {
                    longest_gap = longest_gap.max(now - since);
                    missing_since = None;
                }
                _ => {}
            }
            if let Some(status) = helper.try_wait().expect("poll helper") {
                break status;
            }
            assert!(now < deadline, "the helper did not finish");
        };
        if let Some(since) = missing_since {
            longest_gap = longest_gap.max(since.elapsed());
        }

        assert!(finished.success(), "the swap succeeds");
        assert!(
            longest_gap < Duration::from_millis(20),
            "ubra.app was missing for {longest_gap:?} after ubra exited"
        );
        assert!(
            staged_while_running,
            "the new bundle is copied beside the old one before ubra exits"
        );
        assert_eq!(
            fs::read_to_string(target.join("Contents/MacOS/ubra")).expect("read"),
            "new"
        );
        assert!(!next.exists(), "nothing is left beside the app");
        assert!(!directory.path().join("ubra.app.ubra-previous").exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_helper_restores_the_old_bundle_when_the_unpack_fails() {
        let directory = tempfile::tempdir().expect("temp dir");
        let target = fake_bundle(directory.path(), "ubra.app", "old");
        // A staged path that does not exist is exactly what a half-cleaned
        // cache directory looks like, and ditto fails on it.
        let missing = directory.path().join("staged.app");

        let output = run_installer(&missing, &target, dead_pid());
        assert!(
            !output.status.success(),
            "a failed unpack must report failure"
        );
        assert!(target.exists(), "the app must still be there");
        assert_eq!(
            fs::read_to_string(target.join("Contents/MacOS/ubra")).expect("read"),
            "old",
            "the original bundle must be restored byte for byte"
        );
        assert!(!directory.path().join("ubra.app.ubra-previous").exists());
        assert!(!directory.path().join("ubra.app.ubra-next").exists());
    }

    #[test]
    fn the_helper_leaves_the_app_alone_when_the_process_never_exits() {
        let directory = tempfile::tempdir().expect("temp dir");
        let staged = fake_bundle(directory.path(), "staged.app", "new");
        let target = fake_bundle(directory.path(), "ubra.app", "old");

        let mut hung = Command::new("/bin/sleep").arg("30").spawn().expect("spawn");
        // Shorten the wait loop rather than blocking the suite for 60s.
        let script = installer_script(hung.id(), &staged, &target, true)
            .replace("/usr/bin/open", "echo relaunched")
            .replace("-gt 600", "-gt 3");
        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .output()
            .expect("sh runs");
        let _ = hung.kill();
        let _ = hung.wait();

        assert!(!output.status.success());
        assert_eq!(
            fs::read_to_string(target.join("Contents/MacOS/ubra")).expect("read"),
            "old",
            "a still-running ubra must never have its bundle swapped"
        );
        assert!(!directory.path().join("ubra.app.ubra-next").exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn unpacking_finds_the_single_app_in_an_archive() {
        let directory = tempfile::tempdir().expect("temp dir");
        let source = directory.path().join("source");
        fs::create_dir_all(source.join("ubra.app/Contents/MacOS")).expect("bundle layout");
        fs::write(source.join("ubra.app/Contents/MacOS/ubra"), b"binary").expect("binary");

        let archive = directory.path().join("ubra.zip");
        let status = Command::new("/usr/bin/ditto")
            .arg("-c")
            .arg("-k")
            .arg("--keepParent")
            .arg(source.join("ubra.app"))
            .arg(&archive)
            .status()
            .expect("ditto runs");
        assert!(status.success());

        let unpacked = unpack(&archive, &directory.path().join("staged")).expect("unpack");
        assert_eq!(unpacked.file_name().unwrap(), "ubra.app");
        assert!(unpacked.join("Contents/MacOS/ubra").exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_archive_without_a_bundle_is_rejected() {
        let directory = tempfile::tempdir().expect("temp dir");
        let loose = directory.path().join("loose");
        fs::create_dir_all(&loose).expect("dir");
        fs::write(loose.join("README.txt"), b"not an app").expect("file");

        let archive = directory.path().join("loose.zip");
        let status = Command::new("/usr/bin/ditto")
            .arg("-c")
            .arg("-k")
            .arg(&loose)
            .arg(&archive)
            .status()
            .expect("ditto runs");
        assert!(status.success());

        let error = unpack(&archive, &directory.path().join("staged"))
            .expect_err("a bundle-less archive must be refused");
        assert!(matches!(error, UpdateError::Integrity(_)));
    }
}
