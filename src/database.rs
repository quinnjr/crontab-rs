//! The set of crontabs currently loaded from disk, and how to refresh it.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use nix::unistd::User;

use crate::config::Config;
use crate::crontab::{Crontab, Format};

/// One crontab file that has been read successfully.
#[derive(Debug, Clone)]
pub struct LoadedTab {
    pub path: PathBuf,
    pub mtime: SystemTime,
    pub format: Format,
    /// For spool crontabs, the owning user (who runs every entry).
    pub owner: Option<String>,
    pub crontab: Crontab,
    /// Delay in minutes applied to every job in this file (derived from
    /// `RANDOM_DELAY` and the daemon's random scale).
    pub delay: u32,
    /// Name used in log messages (user name or file path).
    pub label: String,
}

/// All loaded crontabs keyed by path.
#[derive(Debug)]
pub struct Database {
    cfg: Config,
    permissive: bool,
    /// Uniform random factor in `[0, 1]` chosen once per daemon run.
    random_scale: f64,
    /// Files that failed to load, with their mtime, so errors are logged once.
    bad: BTreeMap<PathBuf, SystemTime>,
    pub tabs: BTreeMap<PathBuf, LoadedTab>,
}

impl Database {
    pub fn new(cfg: Config, permissive: bool, random_scale: f64) -> Database {
        Database {
            cfg,
            permissive,
            random_scale: random_scale.clamp(0.0, 1.0),
            bad: BTreeMap::new(),
            tabs: BTreeMap::new(),
        }
    }

    /// Drop all cached state so the next [`refresh`](Self::refresh)
    /// reloads every file.
    pub fn forget_all(&mut self) {
        self.tabs.clear();
        self.bad.clear();
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Rescan all crontab locations.  Returns `true` when anything was
    /// added, removed or reloaded.
    pub fn refresh(&mut self) -> bool {
        let mut seen: Vec<PathBuf> = Vec::new();
        let mut changed = false;

        // /etc/crontab
        let sys = self.cfg.system_crontab.clone();
        if sys.is_file() {
            seen.push(sys.clone());
            changed |= self.consider(&sys, Format::System, None, "*system*".to_string());
        }

        // /etc/cron.d/*
        if let Ok(rd) = fs::read_dir(&self.cfg.cron_d_dir) {
            let mut names: Vec<_> = rd.flatten().collect();
            names.sort_by_key(|e| e.file_name());
            for de in names {
                let name = de.file_name().to_string_lossy().into_owned();
                if !cron_d_name_ok(&name) {
                    continue;
                }
                let path = de.path();
                if !path.is_file() {
                    continue;
                }
                seen.push(path.clone());
                changed |= self.consider(&path, Format::System, None, format!("*system*{name}"));
            }
        }

        // /var/spool/cron/<user>
        if let Ok(rd) = fs::read_dir(&self.cfg.spool_dir) {
            let mut names: Vec<_> = rd.flatten().collect();
            names.sort_by_key(|e| e.file_name());
            for de in names {
                let name = de.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || name.ends_with('~') {
                    continue;
                }
                let path = de.path();
                if !path.is_file() {
                    continue;
                }
                seen.push(path.clone());
                changed |= self.consider(&path, Format::User, Some(name.clone()), name);
            }
        }

        // Drop anything that vanished.
        let gone: Vec<PathBuf> = self
            .tabs
            .keys()
            .filter(|p| !seen.contains(p))
            .cloned()
            .collect();
        for p in gone {
            if let Some(tab) = self.tabs.remove(&p) {
                log::info!("({}) UNLOAD ({})", tab.label, p.display());
                changed = true;
            }
        }
        self.bad.retain(|p, _| seen.contains(p));
        changed
    }

    /// Load or reload one file if its mtime changed.  Returns `true` on a
    /// change to the loaded set.
    fn consider(
        &mut self,
        path: &Path,
        format: Format,
        owner: Option<String>,
        label: String,
    ) -> bool {
        let meta = match fs::metadata(path) {
            Ok(m) => m,
            Err(e) => {
                log::warn!("({label}) CAN'T STAT ({}): {e}", path.display());
                return self.tabs.remove(path).is_some();
            }
        };
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if let Some(existing) = self.tabs.get(path)
            && existing.mtime == mtime
        {
            return false;
        }
        if let Some(bad_mtime) = self.bad.get(path)
            && *bad_mtime == mtime
        {
            return false;
        }

        if !self.permissive
            && let Err(reason) = check_file_security(&meta, format, owner.as_deref())
        {
            log::error!("({label}) {reason} ({})", path.display());
            self.bad.insert(path.to_path_buf(), mtime);
            return self.tabs.remove(path).is_some();
        }
        if let Some(user) = &owner {
            match User::from_name(user) {
                Ok(Some(_)) => {}
                _ => {
                    log::error!("({label}) ORPHAN (no passwd entry)");
                    self.bad.insert(path.to_path_buf(), mtime);
                    return self.tabs.remove(path).is_some();
                }
            }
        }

        let text = match fs::read(path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) => {
                log::error!("({label}) CAN'T OPEN ({}): {e}", path.display());
                self.bad.insert(path.to_path_buf(), mtime);
                return self.tabs.remove(path).is_some();
            }
        };
        match Crontab::parse(&text, format) {
            Ok(crontab) => {
                let delay = crontab
                    .random_delay
                    .map(|max| (max as f64 * self.random_scale).round() as u32)
                    .unwrap_or(0);
                let verb = if self.tabs.contains_key(path) {
                    "RELOAD"
                } else {
                    "LOAD"
                };
                log::info!("({label}) {verb} ({})", path.display());
                self.bad.remove(path);
                self.tabs.insert(
                    path.to_path_buf(),
                    LoadedTab {
                        path: path.to_path_buf(),
                        mtime,
                        format,
                        owner,
                        crontab,
                        delay,
                        label,
                    },
                );
                true
            }
            Err(errors) => {
                for e in &errors {
                    log::error!("({label}) BAD CRONTAB ({}): {e}", path.display());
                }
                self.bad.insert(path.to_path_buf(), mtime);
                self.tabs.remove(path).is_some()
            }
        }
    }
}

/// cronie skips editor backups and package-manager leftovers in `cron.d`.
pub fn cron_d_name_ok(name: &str) -> bool {
    if name.starts_with('.') || name.ends_with('~') {
        return false;
    }
    const BAD_SUFFIXES: &[&str] = &[
        ".rpmsave",
        ".rpmorig",
        ".rpmnew",
        ".dpkg-dist",
        ".dpkg-old",
        ".dpkg-new",
        ".pacsave",
        ".pacnew",
        ".orig",
        ".bak",
    ];
    !BAD_SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// Ownership and mode checks in the spirit of cronie's `process_crontab`.
fn check_file_security(
    meta: &fs::Metadata,
    format: Format,
    owner: Option<&str>,
) -> Result<(), &'static str> {
    if !meta.is_file() {
        return Err("NOT REGULAR");
    }
    let mode = meta.mode() & 0o7777;
    match format {
        Format::System => {
            if meta.uid() != 0 {
                return Err("WRONG FILE OWNER");
            }
            if mode & 0o022 != 0 {
                return Err("INSECURE MODE (group/world writable)");
            }
        }
        Format::User => {
            let owner = owner.unwrap_or("");
            let owner_uid = User::from_name(owner)
                .ok()
                .flatten()
                .map(|u| u.uid.as_raw());
            if meta.uid() != 0 && Some(meta.uid()) != owner_uid {
                return Err("WRONG FILE OWNER");
            }
            if meta.nlink() != 1 {
                return Err("WRONG FILE LINK COUNT");
            }
            if mode & 0o077 != 0 {
                return Err("INSECURE MODE (group/world accessible)");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(dir: &Path) -> Config {
        Config {
            spool_dir: dir.join("spool"),
            system_crontab: dir.join("crontab"),
            cron_d_dir: dir.join("cron.d"),
            ..Config::default()
        }
    }

    fn me() -> String {
        User::from_uid(nix::unistd::getuid()).unwrap().unwrap().name
    }

    #[test]
    fn loads_and_tracks_changes() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        fs::create_dir_all(&cfg.spool_dir).unwrap();
        fs::create_dir_all(&cfg.cron_d_dir).unwrap();
        fs::write(&cfg.system_crontab, "* * * * * root echo sys\n").unwrap();
        fs::write(cfg.cron_d_dir.join("job"), "* * * * * root echo d\n").unwrap();
        fs::write(cfg.cron_d_dir.join("job.rpmsave"), "* * * * * root nope\n").unwrap();
        fs::write(cfg.cron_d_dir.join(".hidden"), "garbage\n").unwrap();
        let spool = cfg.user_crontab(&me());
        fs::write(&spool, "* * * * * echo user\n").unwrap();
        fs::write(cfg.user_crontab("no-such-user-xyz"), "* * * * * echo x\n").unwrap();

        let mut db = Database::new(cfg.clone(), true, 0.5);
        assert!(db.refresh());
        assert_eq!(db.tabs.len(), 3, "{:?}", db.tabs.keys());
        assert!(!db.refresh());

        // Modify: mtime granularity may be coarse, so force a distinct mtime.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&spool, "RANDOM_DELAY=10\n0 1 * * * echo user2\n").unwrap();
        let t = filetime_bump(&spool);
        let _ = t;
        assert!(db.refresh());
        let tab = &db.tabs[&spool];
        assert_eq!(tab.crontab.entries[0].command, "echo user2");
        assert_eq!(tab.delay, 5);
        assert_eq!(tab.owner.as_deref(), Some(me().as_str()));

        // Break it: entry is unloaded, error logged once.
        fs::write(&spool, "99 * * * * echo bad\n").unwrap();
        filetime_bump(&spool);
        assert!(db.refresh());
        assert!(!db.tabs.contains_key(&spool));
        assert!(!db.refresh());

        fs::remove_file(cfg.cron_d_dir.join("job")).unwrap();
        assert!(db.refresh());
        assert_eq!(db.tabs.len(), 1);
    }

    /// Ensure the file's mtime differs from any earlier observation.
    fn filetime_bump(path: &Path) -> SystemTime {
        let now = SystemTime::now() + std::time::Duration::from_secs(2);
        let f = fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(now).unwrap();
        now
    }

    #[test]
    fn security_checks() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        fs::create_dir_all(&cfg.spool_dir).unwrap();
        let spool = cfg.user_crontab(&me());
        fs::write(&spool, "* * * * * echo user\n").unwrap();
        fs::set_permissions(&spool, fs::Permissions::from_mode(0o644)).unwrap();
        let mut db = Database::new(cfg.clone(), false, 0.0);
        db.refresh();
        assert!(
            db.tabs.is_empty(),
            "world-readable spool file must be rejected"
        );
        fs::set_permissions(&spool, fs::Permissions::from_mode(0o600)).unwrap();
        filetime_bump(&spool);
        db.refresh();
        assert_eq!(db.tabs.len(), 1);
        assert!(cron_d_name_ok("0hourly"));
        assert!(!cron_d_name_ok("job~"));
    }
}
