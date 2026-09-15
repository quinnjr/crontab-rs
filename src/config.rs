//! Filesystem locations used by the daemon and the `crontab` utility.
//!
//! Defaults follow cronie on Linux.  Every path can be overridden with an
//! environment variable of the form `CRONTAB_RS_<NAME>` — but only when the
//! process is not running set-user-ID or set-group-ID, since a privileged
//! `crontab` binary must not trust its caller's environment.

use std::path::PathBuf;

use nix::unistd::{getegid, geteuid, getgid, getuid};

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Directory holding one crontab per user, named after the user.
    pub spool_dir: PathBuf,
    /// The system crontab (with user field).
    pub system_crontab: PathBuf,
    /// Directory of drop-in system crontabs.
    pub cron_d_dir: PathBuf,
    pub allow_file: PathBuf,
    pub deny_file: PathBuf,
    pub pid_file: PathBuf,
    /// Touched after `@reboot` jobs run so a daemon restart does not repeat
    /// them.
    pub reboot_file: PathBuf,
    /// Default `PATH` for jobs.
    pub default_path: String,
    /// Default `SHELL` for jobs.
    pub default_shell: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            spool_dir: PathBuf::from("/var/spool/cron"),
            system_crontab: PathBuf::from("/etc/crontab"),
            cron_d_dir: PathBuf::from("/etc/cron.d"),
            allow_file: PathBuf::from("/etc/cron.allow"),
            deny_file: PathBuf::from("/etc/cron.deny"),
            pid_file: PathBuf::from("/run/crond.pid"),
            reboot_file: PathBuf::from("/run/crond.reboot"),
            default_path: "/usr/bin:/bin".to_string(),
            default_shell: "/bin/sh".to_string(),
        }
    }
}

/// True when the process runs with privileges it did not inherit from its
/// invoker (set-user-ID or set-group-ID binary).
pub fn is_privileged_binary() -> bool {
    getuid() != geteuid() || getgid() != getegid()
}

/// Apply `CRONTAB_RS_*` overrides to `cfg` using `lookup` to read each
/// variable.  Split out from [`Config::from_env_with`] so tests can drive it
/// with a fake environment.
fn apply_env_overrides(cfg: &mut Config, lookup: impl Fn(&str) -> Option<String>) {
    let over = |name: &str, slot: &mut PathBuf| {
        if let Some(v) = lookup(&format!("CRONTAB_RS_{name}"))
            && !v.is_empty()
        {
            *slot = PathBuf::from(v);
        }
    };
    over("SPOOL_DIR", &mut cfg.spool_dir);
    over("SYSTEM_CRONTAB", &mut cfg.system_crontab);
    over("CRON_D", &mut cfg.cron_d_dir);
    over("ALLOW", &mut cfg.allow_file);
    over("DENY", &mut cfg.deny_file);
    over("PID_FILE", &mut cfg.pid_file);
    over("REBOOT_FILE", &mut cfg.reboot_file);
}

impl Config {
    /// Defaults with `CRONTAB_RS_*` environment overrides applied when safe.
    pub fn from_env() -> Config {
        Config::from_env_with(is_privileged_binary(), |k| std::env::var(k).ok())
    }

    /// Testable version of [`Config::from_env`]: `privileged` stands in for
    /// [`is_privileged_binary`] and `lookup` stands in for reading the real
    /// process environment.
    pub fn from_env_with(privileged: bool, lookup: impl Fn(&str) -> Option<String>) -> Config {
        let mut cfg = Config::default();
        if privileged {
            return cfg;
        }
        apply_env_overrides(&mut cfg, lookup);
        cfg
    }

    /// Path of a user's spool crontab.
    pub fn user_crontab(&self, user: &str) -> PathBuf {
        self.spool_dir.join(user)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup_from(map: HashMap<&'static str, &'static str>) -> impl Fn(&str) -> Option<String> {
        move |k: &str| map.get(k).map(|v| v.to_string())
    }

    #[test]
    fn overrides_applied_when_unprivileged() {
        let map = HashMap::from([
            ("CRONTAB_RS_SPOOL_DIR", "/tmp/spool"),
            ("CRONTAB_RS_ALLOW", "/tmp/cron.allow"),
        ]);
        let cfg = Config::from_env_with(false, lookup_from(map));
        assert_eq!(cfg.spool_dir, PathBuf::from("/tmp/spool"));
        assert_eq!(cfg.allow_file, PathBuf::from("/tmp/cron.allow"));
        // Untouched fields keep their defaults.
        assert_eq!(cfg.system_crontab, Config::default().system_crontab);
    }

    #[test]
    fn overrides_skipped_when_privileged() {
        let map = HashMap::from([("CRONTAB_RS_SPOOL_DIR", "/tmp/spool")]);
        let cfg = Config::from_env_with(true, lookup_from(map));
        assert_eq!(cfg, Config::default());
    }
}
