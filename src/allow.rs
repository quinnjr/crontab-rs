//! `cron.allow` / `cron.deny` access control, as in cronie:
//!
//! * root is always allowed;
//! * if `cron.allow` exists, only users listed in it may use crontab;
//! * otherwise, if `cron.deny` exists, users listed in it may not;
//! * otherwise everyone may.

use std::io;
use std::path::Path;

use crate::config::Config;

pub fn user_allowed(cfg: &Config, user: &str, uid: u32) -> bool {
    if uid == 0 {
        return true;
    }
    match list_contains(&cfg.allow_file, user) {
        Ok(found) => return found,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return false,
    }
    match list_contains(&cfg.deny_file, user) {
        Ok(found) => !found,
        Err(e) if e.kind() == io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

/// Whether `user` appears in the access list at `path`.  A read error other
/// than the file simply not existing is passed up so the caller can decide
/// how to fail (deny, in `user_allowed`'s case).
fn list_contains(path: &Path, user: &str) -> io::Result<bool> {
    let text = std::fs::read_to_string(path)?;
    Ok(text
        .lines()
        .map(str::trim)
        .any(|l| !l.is_empty() && !l.starts_with('#') && l == user))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(dir: &Path) -> Config {
        Config {
            allow_file: dir.join("cron.allow"),
            deny_file: dir.join("cron.deny"),
            ..Config::default()
        }
    }

    #[test]
    fn neither_file() {
        let d = tempfile::tempdir().unwrap();
        assert!(user_allowed(&cfg(d.path()), "bob", 1000));
    }

    #[test]
    fn allow_file_wins() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("cron.allow"), "# people\nalice\n").unwrap();
        std::fs::write(d.path().join("cron.deny"), "alice\n").unwrap();
        let c = cfg(d.path());
        assert!(user_allowed(&c, "alice", 1000));
        assert!(!user_allowed(&c, "bob", 1001));
        assert!(user_allowed(&c, "root", 0));
    }

    #[test]
    fn deny_file() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("cron.deny"), "bob\n").unwrap();
        let c = cfg(d.path());
        assert!(user_allowed(&c, "alice", 1000));
        assert!(!user_allowed(&c, "bob", 1001));
    }

    /// An unreadable `cron.deny` (here, a directory where a file is
    /// expected, so the read fails with `EISDIR`) must deny everyone, not
    /// silently allow.
    #[test]
    fn unreadable_deny_file_denies() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("cron.deny")).unwrap();
        let c = cfg(d.path());
        assert!(!user_allowed(&c, "alice", 1000));
        // root is always allowed regardless.
        assert!(user_allowed(&c, "root", 0));
    }

    /// Same, but for `cron.allow`: if it exists but can't be read, nobody
    /// (but root) should be let in.
    #[test]
    fn unreadable_allow_file_denies() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("cron.allow")).unwrap();
        std::fs::write(d.path().join("cron.deny"), "").unwrap();
        let c = cfg(d.path());
        assert!(!user_allowed(&c, "alice", 1000));
        assert!(user_allowed(&c, "root", 0));
    }
}
