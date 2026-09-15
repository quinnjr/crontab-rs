//! Filesystem locations used by the daemon and the `crontab` utility.
//!
//! Defaults follow cronie on Linux.  Every path can be overridden with an
//! environment variable of the form `CRONTAB_RS_<NAME>` — but only when the
//! process is not running set-user-ID or set-group-ID, since a privileged
//! `crontab` binary must not trust its caller's environment.

use std::path::PathBuf;

use nix::unistd::{getegid, geteuid, getgid, getuid};

#[derive(Debug, Clone)]
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

impl Config {
    /// Defaults with `CRONTAB_RS_*` environment overrides applied when safe.
    pub fn from_env() -> Config {
        let mut cfg = Config::default();
        if is_privileged_binary() {
            return cfg;
        }
        let over = |name: &str, slot: &mut PathBuf| {
            if let Ok(v) = std::env::var(format!("CRONTAB_RS_{name}"))
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
        cfg
    }

    /// Path of a user's spool crontab.
    pub fn user_crontab(&self, user: &str) -> PathBuf {
        self.spool_dir.join(user)
    }
}
