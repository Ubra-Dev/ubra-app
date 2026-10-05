//! The user-owned `.ubra-include` file that Quick Open and MCP share.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Active gitignore-style lines: comments and blanks are dropped.
pub fn pattern_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

pub fn load(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

pub fn store(path: &Path, text: &str) -> io::Result<()> {
    let _lock = lock(path)?;
    store_locked(path, text)
}

// Lock a stable sibling, not the include file: writes replace its inode and
// clearing removes it. Keep the lock file so every writer locks the same inode.
fn lock(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut name = path.as_os_str().to_os_string();
    name.push(".lock");
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(Path::new(&name))?;
    file.lock()?;
    Ok(file)
}

fn store_locked(path: &Path, text: &str) -> io::Result<()> {
    if text.trim().is_empty() {
        return match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
    }
    // Readers need no lock: rename publishes the complete file at once.
    // create_new prevents stale files or symlinks from being overwritten.
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let (temporary, mut file) = loop {
        let mut name = path.as_os_str().to_os_string();
        name.push(format!(
            ".tmp-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let temporary = PathBuf::from(name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temporary) {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    let result = (|| {
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IncludeMutation {
    pub path: PathBuf,
    pub text: String,
    pub patterns: Vec<String>,
    pub added: Vec<String>,
}

/// Append unique patterns, preserving comments and existing order.
pub fn add_patterns(path: &Path, patterns: &[String]) -> io::Result<IncludeMutation> {
    let _lock = lock(path)?;
    let mut text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    let mut existing: HashSet<String> = pattern_lines(&text)
        .iter()
        .map(|pattern| pattern_key(pattern))
        .collect();
    let mut added = Vec::new();
    for pattern in patterns {
        let trimmed = pattern.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let key = pattern_key(trimmed);
        if !existing.insert(key) {
            continue;
        }
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(trimmed);
        text.push('\n');
        added.push(trimmed.to_owned());
    }
    if !added.is_empty() {
        store_locked(path, &text)?;
    }
    let patterns = pattern_lines(&text);
    Ok(IncludeMutation {
        path: path.to_path_buf(),
        text,
        patterns,
        added,
    })
}

fn pattern_key(pattern: &str) -> String {
    pattern.trim_end_matches('/').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn add_patterns_appends_unique_gitignore_lines() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(".ubra-include");
        fs::write(&path, "# keep\n*/.worktrees/\n").unwrap();

        let first = add_patterns(
            &path,
            &[
                "  **/.worktrees/  ".into(),
                "*/.worktrees".into(),
                "# ignored".into(),
                String::new(),
            ],
        )
        .unwrap();
        assert_eq!(first.added, ["**/.worktrees/"]);
        assert_eq!(first.patterns, ["*/.worktrees/", "**/.worktrees/"]);
        assert_eq!(load(&path), "# keep\n*/.worktrees/\n**/.worktrees/\n");

        let again = add_patterns(&path, &["**/.worktrees".into()]).unwrap();
        assert!(again.added.is_empty());
        assert_eq!(load(&path), "# keep\n*/.worktrees/\n**/.worktrees/\n");
    }

    #[test]
    fn concurrent_additions_preserve_every_pattern_and_comment() {
        use std::sync::{Arc, Barrier};

        let temp = tempdir().unwrap();
        let path = temp.path().join(".ubra-include");
        store(&path, "# keep this comment\n").unwrap();
        let gate = Arc::new(Barrier::new(16));
        std::thread::scope(|scope| {
            for index in 0..16 {
                let gate = Arc::clone(&gate);
                let path = &path;
                scope.spawn(move || {
                    gate.wait();
                    add_patterns(path, &[format!("**/.worktree-{index}/")]).unwrap();
                });
            }
        });
        let text = load(&path);
        assert!(text.starts_with("# keep this comment\n"));
        let patterns: HashSet<_> = pattern_lines(&text).into_iter().collect();
        let expected: HashSet<_> = (0..16).map(|i| format!("**/.worktree-{i}/")).collect();
        assert_eq!(patterns, expected);
    }

    #[test]
    fn readers_only_see_complete_replacements() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let temp = tempdir().unwrap();
        let path = temp.path().join(".ubra-include");
        let first = "**/.first/\n".repeat(4096);
        let second = "**/.second/\n".repeat(4096);
        store(&path, &first).unwrap();
        let finished = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..20 {
                    store(&path, &second).unwrap();
                    store(&path, &first).unwrap();
                }
                finished.store(true, Ordering::Release);
            });
            while !finished.load(Ordering::Acquire) {
                let contents = fs::read_to_string(&path).unwrap();
                assert!(contents == first || contents == second);
            }
        });
    }

    #[test]
    fn failed_reads_and_replacements_preserve_existing_data() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(".ubra-include");
        fs::write(&path, [0xff]).unwrap();
        assert!(add_patterns(&path, &["**/.worktrees/".into()]).is_err());
        assert_eq!(fs::read(&path).unwrap(), [0xff]);

        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        fs::write(path.join("keep"), "unchanged").unwrap();
        assert!(store(&path, "**/.worktrees/\n").is_err());
        assert_eq!(fs::read_to_string(path.join("keep")).unwrap(), "unchanged");
        assert!(fs::read_dir(temp.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")
        }));
    }

    #[test]
    fn empty_store_deletes_the_file() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(".ubra-include");
        store(&path, "**/.hidden/\n").unwrap();
        assert!(path.exists());
        store(&path, " \n").unwrap();
        assert!(!path.exists());
        assert_eq!(load(&path), "");
    }
}
