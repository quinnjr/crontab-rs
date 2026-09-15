//! `cron.allow` / `cron.deny` access control, as in cronie:
//!
//! * root is always allowed;
//! * if `cron.allow` exists, only users listed in it may use crontab;
//! * otherwise, if `cron.deny` exists, users listed in it may not;
//! * otherwise everyone may.

use std::path::Path;

use crate::config::Config;

pub fn user_allowed(cfg: &Config, user: &str, uid: u32) -> bool {
    if uid == 0 {
        return true;
    }
    if cfg.allow_file.exists() {
        return list_contains(&cfg.allow_file, user);
    }
    if cfg.deny_file.exists() {
        return !list_contains(&cfg.deny_file, user);
    }
    true
}

fn list_contains(path: &Path, user: &str) -> bool {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .lines()
            .map(str::trim)
            .any(|l| !l.is_empty() && !l.starts_with('#') && l == user),
        Err(_) => false,
    }
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
}
