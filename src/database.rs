//! The set of crontabs currently loaded from disk, and how to refresh it.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, Read as _};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use nix::unistd::User;

use crate::config::Config;
use crate::crontab::{Crontab, Format};

/// `(mtime, ctime, ctime_nsec)` — identifies a specific version of a file's
/// metadata, so a `chmod`/`chown` that leaves `mtime` alone (but bumps
/// `ctime`) is still noticed on the next refresh.
type MetaKey = (SystemTime, i64, i64);

fn key_of(meta: &fs::Metadata) -> MetaKey {
    (
        meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        meta.ctime(),
        meta.ctime_nsec(),
    )
}

/// One crontab file that has been read successfully.
#[derive(Debug, Clone)]
pub struct LoadedTab {
    pub path: PathBuf,
    pub mtime: SystemTime,
    /// Change time, for detecting metadata-only changes (chmod/chown) that
    /// leave `mtime` untouched.
    pub ctime: i64,
    pub ctime_nsec: i64,
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
    /// Files that failed to load, with the metadata key they failed at, so
    /// errors are logged once per version of the file.
    bad: BTreeMap<PathBuf, MetaKey>,
    /// Spool files with no matching passwd entry, with the metadata key the
    /// ORPHAN message was last logged for.  Unlike `bad`, an orphan is
    /// retried every refresh (so adding the passwd entry takes effect on
    /// the next pass) — this map exists only to keep the log from repeating.
    orphan_logged: BTreeMap<PathBuf, MetaKey>,
    pub tabs: BTreeMap<PathBuf, LoadedTab>,
}

/// Ownership/mode/regularity problems found by [`check_file_security`].
#[derive(Debug)]
enum SecurityError {
    NotRegular,
    WrongOwner,
    InsecureMode(&'static str),
    WrongLinkCount,
    /// The passwd lookup needed to validate ownership failed (e.g. an NSS
    /// backend is unreachable) — distinct from the user simply not
    /// existing, and from the other variants: callers must not cache this
    /// outcome, so a transient failure is retried on the next refresh.
    LookupFailed(nix::errno::Errno),
}

impl fmt::Display for SecurityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecurityError::NotRegular => f.write_str("NOT REGULAR"),
            SecurityError::WrongOwner => f.write_str("WRONG FILE OWNER"),
            SecurityError::InsecureMode(detail) => write!(f, "INSECURE MODE {detail}"),
            SecurityError::WrongLinkCount => f.write_str("WRONG FILE LINK COUNT"),
            SecurityError::LookupFailed(e) => write!(f, "CAN'T LOOKUP USER ({e})"),
        }
    }
}

impl Database {
    pub fn new(cfg: Config, permissive: bool, random_scale: f64) -> Database {
        Database {
            cfg,
            permissive,
            random_scale: random_scale.clamp(0.0, 1.0),
            bad: BTreeMap::new(),
            orphan_logged: BTreeMap::new(),
            tabs: BTreeMap::new(),
        }
    }

    /// Drop all cached state so the next [`refresh`](Self::refresh)
    /// reloads every file.
    pub fn forget_all(&mut self) {
        self.tabs.clear();
        self.bad.clear();
        self.orphan_logged.clear();
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
        match fs::symlink_metadata(&sys) {
            Ok(_) => {
                seen.push(sys.clone());
                changed |= self.consider(&sys, Format::System, None, "*system*".to_string());
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                log::error!("(CRON) ERROR (can't stat {}: {e})", sys.display());
            }
        }

        // /etc/cron.d/*
        let cron_d_dir = self.cfg.cron_d_dir.clone();
        for de in self.scan_dir(&cron_d_dir, &mut seen) {
            let name = de.file_name().to_string_lossy().into_owned();
            if !cron_d_name_ok(&name) {
                continue;
            }
            let path = de.path();
            seen.push(path.clone());
            changed |= self.consider(&path, Format::System, None, format!("*system*{name}"));
        }

        // /var/spool/cron/<user>
        let spool_dir = self.cfg.spool_dir.clone();
        if self.spool_dir_secure(&spool_dir) {
            for de in self.scan_dir(&spool_dir, &mut seen) {
                let name = de.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || name.ends_with('~') {
                    continue;
                }
                let path = de.path();
                seen.push(path.clone());
                changed |= self.consider(&path, Format::User, Some(name.clone()), name);
            }
        } else {
            // Keep whatever's already loaded from the spool rather than
            // mass-unloading it over a directory permission problem.
            for p in self.tabs.keys() {
                if p.starts_with(&spool_dir) {
                    seen.push(p.clone());
                }
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
        self.orphan_logged.retain(|p, _| seen.contains(p));
        changed
    }

    /// List the entries of `dir`, logging errors instead of silently
    /// dropping them.  A missing directory is treated as empty.  On any
    /// other read error, every currently loaded tab under `dir` is marked
    /// `seen` so it survives this refresh instead of being unloaded.
    fn scan_dir(&self, dir: &Path, seen: &mut Vec<PathBuf>) -> Vec<fs::DirEntry> {
        let rd = match fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => {
                log::error!("(CRON) ERROR (can't read directory {}: {e})", dir.display());
                for p in self.tabs.keys() {
                    if p.starts_with(dir) {
                        seen.push(p.clone());
                    }
                }
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        for entry in rd {
            match entry {
                Ok(e) => out.push(e),
                Err(e) => log::error!("(CRON) ERROR (can't read entry in {}: {e})", dir.display()),
            }
        }
        out.sort_by_key(|e| e.file_name());
        out
    }

    /// True when it's safe to read spool crontabs from `dir`: the directory
    /// must not be group/world-writable unless the sticky bit is set.  In
    /// permissive mode (used by tests) this check is skipped entirely.
    fn spool_dir_secure(&self, dir: &Path) -> bool {
        if self.permissive {
            return true;
        }
        match fs::metadata(dir) {
            Ok(m) => {
                let mode = m.mode() & 0o7777;
                let writable_by_others = mode & 0o022 != 0;
                let sticky = mode & 0o1000 != 0;
                if writable_by_others && !sticky {
                    log::error!("(CRON) ERROR (INSECURE MODE on {})", dir.display());
                    false
                } else {
                    true
                }
            }
            // Missing/unreadable spool dir: scan_dir will report it.
            Err(_) => true,
        }
    }

    /// Load or reload one file if its metadata changed.  Returns `true` on
    /// a change to the loaded set.
    fn consider(
        &mut self,
        path: &Path,
        format: Format,
        owner: Option<String>,
        label: String,
    ) -> bool {
        let file = match fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
        {
            Ok(f) => f,
            Err(e) => {
                if e.kind() == io::ErrorKind::NotFound {
                    return self.tabs.remove(path).is_some();
                }
                // Best-effort key from the symlink's own metadata, so a
                // repeatedly-failing symlink doesn't spam the log.
                let key = fs::symlink_metadata(path).map(|m| key_of(&m)).unwrap_or((
                    SystemTime::UNIX_EPOCH,
                    0,
                    0,
                ));
                if self.bad.get(path) == Some(&key) {
                    return self.tabs.remove(path).is_some();
                }
                if e.raw_os_error() == Some(libc::ELOOP) {
                    log::error!("({label}) NOT REGULAR ({})", path.display());
                } else {
                    log::warn!("({label}) CAN'T STAT ({}): {e}", path.display());
                }
                self.bad.insert(path.to_path_buf(), key);
                return self.tabs.remove(path).is_some();
            }
        };

        let meta = match file.metadata() {
            Ok(m) => m,
            Err(e) => {
                log::warn!("({label}) CAN'T STAT ({}): {e}", path.display());
                return self.tabs.remove(path).is_some();
            }
        };
        let key = key_of(&meta);

        if let Some(existing) = self.tabs.get(path)
            && (existing.mtime, existing.ctime, existing.ctime_nsec) == key
        {
            return false;
        }
        if let Some(bad_key) = self.bad.get(path)
            && *bad_key == key
        {
            return false;
        }

        if !self.permissive {
            match check_file_security(&meta, format, owner.as_deref()) {
                Ok(()) => {}
                Err(SecurityError::LookupFailed(e)) => {
                    log::warn!("({label}) CAN'T LOOKUP USER ({e})");
                    // Transient (e.g. NSS outage): don't cache, keep
                    // whatever's currently loaded, retry next refresh.
                    return false;
                }
                Err(reason) => {
                    log::error!("({label}) {reason} ({})", path.display());
                    self.bad.insert(path.to_path_buf(), key);
                    return self.tabs.remove(path).is_some();
                }
            }
        }
        if let Some(user) = &owner {
            match User::from_name(user) {
                Ok(Some(_)) => {
                    self.orphan_logged.remove(path);
                }
                Ok(None) => {
                    if self.orphan_logged.get(path) != Some(&key) {
                        log::error!("({label}) ORPHAN (no passwd entry)");
                        self.orphan_logged.insert(path.to_path_buf(), key);
                    }
                    // Not cached in `bad`: retry every refresh so the tab
                    // loads as soon as the passwd entry appears.
                    return self.tabs.remove(path).is_some();
                }
                Err(e) => {
                    log::warn!("({label}) CAN'T LOOKUP USER ({e})");
                    return false;
                }
            }
        }

        let mut bytes = Vec::new();
        let mut file = file;
        let read_result = file.read_to_end(&mut bytes);
        let text = match read_result {
            Ok(_) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) => {
                log::error!("({label}) CAN'T OPEN ({}): {e}", path.display());
                self.bad.insert(path.to_path_buf(), key);
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
                        mtime: key.0,
                        ctime: key.1,
                        ctime_nsec: key.2,
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
                self.bad.insert(path.to_path_buf(), key);
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
) -> Result<(), SecurityError> {
    if !meta.is_file() {
        return Err(SecurityError::NotRegular);
    }
    let mode = meta.mode() & 0o7777;
    match format {
        Format::System => {
            if meta.uid() != 0 {
                return Err(SecurityError::WrongOwner);
            }
            if mode & 0o022 != 0 {
                return Err(SecurityError::InsecureMode("(group/world writable)"));
            }
        }
        Format::User => {
            let owner = owner.unwrap_or("");
            let owner_uid = match User::from_name(owner) {
                Ok(found) => found.map(|u| u.uid.as_raw()),
                Err(e) => return Err(SecurityError::LookupFailed(e)),
            };
            if meta.uid() != 0 && Some(meta.uid()) != owner_uid {
                return Err(SecurityError::WrongOwner);
            }
            if meta.nlink() != 1 {
                return Err(SecurityError::WrongLinkCount);
            }
            if mode & 0o077 != 0 {
                return Err(SecurityError::InsecureMode("(group/world accessible)"));
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
        filetime_bump(&spool);
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
        // Fixing just the mode (no content change, so mtime may not move)
        // must still be picked up because the key also tracks ctime.
        fs::set_permissions(&spool, fs::Permissions::from_mode(0o600)).unwrap();
        db.refresh();
        assert_eq!(db.tabs.len(), 1);
        assert!(cron_d_name_ok("0hourly"));
        assert!(!cron_d_name_ok("job~"));
    }

    #[test]
    fn rejects_hard_linked_spool_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        fs::create_dir_all(&cfg.spool_dir).unwrap();
        let spool = cfg.user_crontab(&me());
        fs::write(&spool, "* * * * * echo user\n").unwrap();
        fs::set_permissions(&spool, fs::Permissions::from_mode(0o600)).unwrap();
        let link = cfg.spool_dir.join("extra-link");
        fs::hard_link(&spool, &link).unwrap();

        let mut db = Database::new(cfg.clone(), false, 0.0);
        db.refresh();
        assert!(
            !db.tabs.contains_key(&spool),
            "hard-linked spool file must be rejected (WRONG FILE LINK COUNT)"
        );
    }

    #[test]
    fn rejects_directory_at_spool_path() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        fs::create_dir_all(&cfg.spool_dir).unwrap();
        let spool = cfg.user_crontab(&me());
        fs::create_dir(&spool).unwrap();

        let mut db = Database::new(cfg.clone(), false, 0.0);
        // Must not panic, and must not load a directory as a crontab.
        db.refresh();
        assert!(!db.tabs.contains_key(&spool));
    }

    #[test]
    fn rejects_symlink_at_spool_path() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        fs::create_dir_all(&cfg.spool_dir).unwrap();
        let real = dir.path().join("real-crontab");
        fs::write(&real, "* * * * * echo user\n").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o600)).unwrap();
        let spool = cfg.user_crontab(&me());
        std::os::unix::fs::symlink(&real, &spool).unwrap();

        let mut db = Database::new(cfg.clone(), false, 0.0);
        db.refresh();
        assert!(
            !db.tabs.contains_key(&spool),
            "a symlink at a spool path must be rejected, even if it points to a valid file"
        );
    }

    /// `WRONG FILE OWNER` for a non-root-owned mode-666 system crontab
    /// needs a file genuinely owned by someone other than root, which
    /// needs root privileges to arrange convincingly — skip elsewhere.
    #[test]
    fn wrong_file_owner_for_system_crontab() {
        use std::os::unix::fs::PermissionsExt;
        if !nix::unistd::geteuid().is_root() {
            eprintln!("skipping: requires root");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        fs::write(&cfg.system_crontab, "* * * * * root echo sys\n").unwrap();
        fs::set_permissions(&cfg.system_crontab, fs::Permissions::from_mode(0o666)).unwrap();
        nix::unistd::chown(
            &cfg.system_crontab,
            Some(nix::unistd::Uid::from_raw(1)),
            None,
        )
        .unwrap();

        let mut db = Database::new(cfg.clone(), false, 0.0);
        db.refresh();
        assert!(db.tabs.is_empty());
    }

    /// An unreadable spool directory must not unload tabs already loaded
    /// from it — the daemon should keep running the last-known-good set
    /// until the directory is readable again.
    #[test]
    fn unreadable_spool_dir_keeps_loaded_tabs() {
        use std::os::unix::fs::PermissionsExt;
        if nix::unistd::geteuid().is_root() {
            eprintln!("skipping: root bypasses directory permissions");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        fs::create_dir_all(&cfg.spool_dir).unwrap();
        let spool = cfg.user_crontab(&me());
        fs::write(&spool, "* * * * * echo user\n").unwrap();
        fs::set_permissions(&spool, fs::Permissions::from_mode(0o600)).unwrap();

        let mut db = Database::new(cfg.clone(), false, 0.0);
        db.refresh();
        assert_eq!(db.tabs.len(), 1, "initial load must succeed");

        let restore = fs::Permissions::from_mode(0o755);
        fs::set_permissions(&cfg.spool_dir, fs::Permissions::from_mode(0o000)).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            db.refresh();
        }));
        fs::set_permissions(&cfg.spool_dir, restore).unwrap();
        result.unwrap();

        assert_eq!(
            db.tabs.len(),
            1,
            "tab loaded before the directory became unreadable must stay loaded"
        );
    }
}
