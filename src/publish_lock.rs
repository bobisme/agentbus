//! Single-publisher ownership for one event-log path.
//!
//! Multiple O_APPEND writers can coexist while a log only grows, but rotation
//! gives the active pathname exactly one owner. The kernel lock, not the
//! continued existence or contents of the lock file, is the authority.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub struct PublishLock {
    _file: File,
}

fn lock_path(log: &Path) -> PathBuf {
    let mut name = log.as_os_str().to_os_string();
    name.push(".lock");
    PathBuf::from(name)
}

impl PublishLock {
    pub fn acquire(log: &Path) -> Result<Self, String> {
        let path = lock_path(log);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
        file.try_lock().map_err(|error| {
            format!(
                "another publisher owns {} (lock {}: {error})",
                log.display(),
                path.display()
            )
        })?;
        // Diagnostic only. The held OS lock is the sole ownership signal.
        let _ = file.set_len(0);
        let _ = file.seek(SeekFrom::Start(0));
        let _ = writeln!(file, "{}", std::process::id());
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_log_has_one_owner_but_another_log_is_independent() {
        let dir = std::env::temp_dir().join(format!("agentbus-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.jsonl");
        let b = dir.join("b.jsonl");
        let _first = PublishLock::acquire(&a).unwrap();
        assert!(PublishLock::acquire(&a).is_err());
        assert!(PublishLock::acquire(&b).is_ok());
    }

    #[test]
    fn stale_lock_file_does_not_block_a_new_owner() {
        let dir = std::env::temp_dir().join(format!("agentbus-stale-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("events.jsonl");
        std::fs::write(lock_path(&log), "999999\n").unwrap();
        assert!(PublishLock::acquire(&log).is_ok());
    }
}
