//! Native desktop close confirmation, without an in-window fallback.
//!
//! X11 IDs come from GPUI's `HasWindowHandle` implementation. `--attach` works
//! with GTK3 zenity; zenity 4 accepts it but no longer parents the dialog.
//! Wayland parenting requires an exported xdg-foreign handle, which
//! GPUI does not expose: a raw wl_surface pointer must never cross processes.
//! Those cases use a standalone native dialog, not a fabricated modal parent.

use super::CloseResponse;
use async_process::{Command, Stdio};
use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;

const SUPPRESS_TAG: &str = "suppress";
const SUPPRESS_LABEL: &str = "Don't ask again";

fn dialog_arguments(title: &str, message: &str, x11_parent: Option<u64>) -> Vec<String> {
    let mut args = vec![
        format!("--title={title}"),
        "--ok-label=Close".into(),
        "--cancel-label=Cancel".into(),
        "--list".into(),
        "--checklist".into(),
        "--modal".into(),
        format!("--text={message}"),
        "--column=".into(),
        "--column=".into(),
        "--column=choice".into(),
        "--hide-header".into(),
        "--hide-column=3".into(),
        "--print-column=3".into(),
        "FALSE".into(),
        SUPPRESS_LABEL.into(),
        SUPPRESS_TAG.into(),
    ];
    if let Some(parent) = x11_parent {
        args.push(format!("--attach={parent}"));
    }
    args
}

fn find_zenity(path: Option<&OsStr>) -> Result<PathBuf, String> {
    if let Some(path) = path {
        for directory in std::env::split_paths(path) {
            let executable = directory.join("zenity");
            if std::fs::metadata(&executable).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            }) {
                return Ok(executable);
            }
        }
    }
    Err("Native close confirmation requires zenity (GTK) on PATH. Install zenity and try again; the window has not been closed.".into())
}

fn response_from_output(output: Output) -> CloseResponse {
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Zenity uses 1 for Cancel. A startup/argument error can also return 1,
    // but emits diagnostics: do not disguise that failure as user cancellation.
    if output.status.code() == Some(1) && output.stdout.is_empty() && stderr.trim().is_empty() {
        return CloseResponse::Cancel;
    }
    if !output.status.success() {
        return CloseResponse::Unavailable(format!(
            "zenity close confirmation failed ({}): {}",
            output.status,
            if stderr.trim().is_empty() {
                "no diagnostic output"
            } else {
                stderr.trim()
            },
        ));
    }
    // Require the exact one-row protocol; unexpected/malformed output must
    // never grant permission to close or persist the suppression preference.
    match output.stdout.as_slice() {
        b"" | b"\n" => CloseResponse::Close { suppress: false },
        b"suppress" | b"suppress\n" => CloseResponse::Close { suppress: true },
        _ => CloseResponse::Unavailable(
            "zenity returned unexpected close confirmation output".into(),
        ),
    }
}

async fn run_dialog(
    executable: &Path,
    title: &str,
    message: &str,
    x11_parent: Option<u64>,
) -> CloseResponse {
    // The output future owns its child. Dropping the caller's task therefore
    // kills and reaps the native dialog instead of leaving an orphaned prompt.
    let output = Command::new(executable)
        .args(dialog_arguments(title, message, x11_parent))
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .reap_on_drop(true)
        .output()
        .await;
    match output {
        Ok(output) => response_from_output(output),
        Err(error) => {
            CloseResponse::Unavailable(format!("Unable to run zenity close confirmation: {error}",))
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn close_prompt(
    window: &mut gpui::Window,
    title: &str,
    message: &str,
    cx: &mut gpui::App,
) -> gpui::Task<CloseResponse> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let x11_parent = HasWindowHandle::window_handle(window)
        .ok()
        .and_then(|handle| match handle.as_raw() {
            RawWindowHandle::Xlib(handle) => Some(handle.window as u64),
            RawWindowHandle::Xcb(handle) => Some(u64::from(handle.window.get())),
            _ => None,
        });
    let path = std::env::var_os("PATH");
    let title = title.to_owned();
    let message = message.to_owned();
    // Return this task directly, not a detached nested background task. Its
    // owner must retain it only for the lifetime of the pending close request.
    cx.background_executor().spawn(async move {
        match find_zenity(path.as_deref()) {
            Ok(executable) => run_dialog(&executable, &title, &message, x11_parent).await,
            Err(error) => CloseResponse::Unavailable(error),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn output(code: i32, stdout: &[u8], stderr: &[u8]) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    fn fake_executable(directory: &Path, name: &str, script: &str) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[test]
    fn executable_discovery_and_missing_toolkit_are_deterministic() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = Some(directory.path().as_os_str());
        assert!(find_zenity(path).is_err());
        let executable = fake_executable(directory.path(), "zenity", "exit 0");
        assert_eq!(find_zenity(path).unwrap(), executable);
        assert!(find_zenity(None).is_err());
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(find_zenity(path).is_err());
    }

    #[test]
    fn arguments_keep_title_message_and_unchecked_choice_structured() {
        let title = "Close 'window'; $(false)";
        let message = "Sessions & jobs\nremain running";
        let args = dialog_arguments(title, message, Some(42));
        assert!(args.contains(&format!("--title={title}")));
        assert!(args.contains(&"--ok-label=Close".to_owned()));
        assert!(args.contains(&"--cancel-label=Cancel".to_owned()));
        assert!(args.contains(&"--attach=42".to_owned()));
        assert!(args.contains(&SUPPRESS_LABEL.to_owned()));
        assert!(args.contains(&SUPPRESS_TAG.to_owned()));
        assert!(args.contains(&format!("--text={message}")));
        assert!(args.contains(&"FALSE".to_owned()));
        assert!(args.contains(&"--print-column=3".to_owned()));
        assert!(
            !dialog_arguments(title, message, None)
                .iter()
                .any(|arg| arg.starts_with("--attach"))
        );
    }

    #[test]
    fn malformed_output_and_process_errors_never_close() {
        for stdout in [
            b"unexpected\n".as_slice(),
            b"suppress\nsuppress\n",
            b"\xff",
            b" suppress\n",
        ] {
            assert!(matches!(
                response_from_output(output(0, stdout, b"")),
                CloseResponse::Unavailable(_)
            ));
        }
        for result in [
            output(1, b"", b"no display"),
            output(2, b"suppress\n", b"error"),
            output(1, b"suppress\n", b""),
        ] {
            assert!(matches!(
                response_from_output(result),
                CloseResponse::Unavailable(_)
            ));
        }
        let signalled = Output {
            status: std::process::ExitStatus::from_raw(libc::SIGTERM),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        assert!(matches!(
            response_from_output(signalled),
            CloseResponse::Unavailable(_)
        ));
    }

    #[tokio::test]
    async fn fake_toolkit_close_cancel_checkbox_and_errors() {
        let directory = tempfile::TempDir::new().unwrap();
        let executable = fake_executable(directory.path(), "zenity", "exit 0");
        assert!(matches!(
            run_dialog(&executable, "title", "message", None).await,
            CloseResponse::Close { suppress: false }
        ));
        fake_executable(directory.path(), "zenity", "printf 'suppress\\n'; exit 0");
        assert!(matches!(
            run_dialog(&executable, "title", "message", None).await,
            CloseResponse::Close { suppress: true }
        ));
        fake_executable(directory.path(), "zenity", "exit 1");
        assert!(matches!(
            run_dialog(&executable, "title", "message", None).await,
            CloseResponse::Cancel
        ));
        fake_executable(directory.path(), "zenity", "printf 'invalid\\n'; exit 0");
        assert!(matches!(
            run_dialog(&executable, "title", "message", None).await,
            CloseResponse::Unavailable(_)
        ));
        fake_executable(
            directory.path(),
            "zenity",
            "printf 'toolkit failed\\n' >&2; exit 1",
        );
        assert!(matches!(
            run_dialog(&executable, "title", "message", None).await,
            CloseResponse::Unavailable(_)
        ));
        assert!(matches!(
            run_dialog(&directory.path().join("missing"), "title", "message", None).await,
            CloseResponse::Unavailable(_)
        ));
    }

    #[tokio::test]
    async fn dropping_pending_dialog_future_kills_and_reaps_child() {
        let directory = tempfile::TempDir::new().unwrap();
        // Only a fake test fixture uses a shell. exec keeps one PID, so no
        // descendant is left behind when the real adapter's child is dropped.
        let executable = fake_executable(
            directory.path(),
            "zenity",
            "printf '%s' \"$$\" > \"$0.pid\"; exec /bin/sleep 30",
        );
        let pid_file = executable.with_extension("pid");
        let task =
            tokio::spawn(async move { run_dialog(&executable, "title", "message", None).await });
        let pid: libc::pid_t = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(pid) = std::fs::read_to_string(&pid_file)
                    && let Ok(pid) = pid.parse()
                {
                    break pid;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fake native dialog did not start");
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                // SAFETY: signal 0 only checks existence of our fixture PID.
                if unsafe { libc::kill(pid, 0) } == -1 {
                    assert_eq!(
                        std::io::Error::last_os_error().raw_os_error(),
                        Some(libc::ESRCH)
                    );
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("dropped native dialog child was not killed and reaped");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "opens a real Linux toolkit dialog; requires a desktop and interactive input"]
    async fn native_toolkit_close_smoke() {
        let path = std::env::var_os("PATH");
        let executable = find_zenity(path.as_deref()).unwrap();
        let response = run_dialog(
            &executable,
            "Ubra native close smoke",
            "Choose Close or Cancel; optionally check Don't ask again.",
            None,
        )
        .await;
        match response {
            CloseResponse::Unavailable(error) => panic!("{error}"),
            response => eprintln!("Native toolkit response: {response:?}"),
        }
    }
}
