//! The scheduler: decides which minutes to evaluate after each wake-up
//! (with Vixie cron's clock-jump / DST handling) and dispatches jobs.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use chrono::NaiveDateTime;

use crate::clock;
use crate::crontab::{Entry, Format, RandomDelay};
use crate::database::{Database, LoadedTab};
use crate::job::Runner;

/// Which jobs to consider for a minute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pass {
    All,
    /// Only jobs with `*` in minute or hour.
    WildOnly,
    /// Only jobs with fixed minute and hour.
    FixedOnly,
}

/// Largest clock jump (minutes) treated as DST/adjustment rather than a
/// reset.
const MAX_JUMP: i64 = 3 * 60;
/// Largest forward gap in which every missed minute is replayed fully.
const MAX_CATCHUP: i64 = 5;

/// Maximum number of concurrently running (or delayed) jobs overall.
pub const MAX_JOBS_TOTAL: usize = 512;
/// Maximum number of concurrently running (or delayed) jobs per user.
pub const MAX_JOBS_PER_USER: usize = 64;

/// Which UTC offset an evaluation uses (cronie's `vGMToff`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offset {
    /// The offset in effect at the previous normal tick (cronie's
    /// `oldGMToff`), used for minutes replayed across a clock jump.
    Previous,
    /// The offset in effect now.
    Current,
}

/// One evaluation: a local minute, which jobs to consider, and the offset.
pub type Step = (i64, Pass, Offset);

/// Given the last processed minute and the current minute, return the new
/// "virtual" minute and the evaluations to perform, following cronie's main
/// loop.
///
/// * `diff == 1`: normal tick.
/// * `1 < diff <= 5`: woke up late; replay every minute. Replayed minutes use
///   the previous offset, the last one the current offset.
/// * `5 < diff <= 180`: clock jumped forward (e.g. DST start): wildcard jobs
///   for now, then fixed-time jobs for each skipped minute, with offsets as
///   above.
/// * `-180 <= diff <= 0`: clock went back (e.g. DST end) or is catching up
///   after going back: wildcard jobs for now, and the virtual clock holds so
///   fixed-time jobs are not repeated. A genuinely early wake is filtered by
///   [`skip_repeated_wild`].
/// * anything else: large change; resynchronise.
pub fn plan(virtual_time: i64, time_running: i64) -> (i64, Vec<Step>) {
    let diff = time_running - virtual_time;
    let offset_for = |m: i64| {
        if m >= time_running {
            Offset::Current
        } else {
            Offset::Previous
        }
    };
    match diff {
        1 => (
            time_running,
            vec![(time_running, Pass::All, Offset::Current)],
        ),
        d if d > 1 && d <= MAX_CATCHUP => (
            time_running,
            ((virtual_time + 1)..=time_running)
                .map(|m| (m, Pass::All, offset_for(m)))
                .collect(),
        ),
        d if d > MAX_CATCHUP && d <= MAX_JUMP => {
            let mut steps = vec![(time_running, Pass::WildOnly, Offset::Current)];
            steps.extend(
                ((virtual_time + 1)..=time_running).map(|m| (m, Pass::FixedOnly, offset_for(m))),
            );
            (time_running, steps)
        }
        d if (-MAX_JUMP..=0).contains(&d) => (
            virtual_time,
            vec![(time_running, Pass::WildOnly, Offset::Current)],
        ),
        _ => (
            time_running,
            vec![(time_running, Pass::All, Offset::Current)],
        ),
    }
}

/// Whether cronie updates `oldGMToff` after a wake-up with this difference
/// (every case except a backward step).
pub fn updates_previous_offset(virtual_time: i64, time_running: i64) -> bool {
    !(-MAX_JUMP..=0).contains(&(time_running - virtual_time))
}

/// Drop a `WildOnly` evaluation of a minute whose wildcard jobs were just
/// evaluated (an early wake within the same minute), and record which minute
/// last had its wildcard jobs run.
pub fn skip_repeated_wild(work: Vec<Step>, last_wild: &mut Option<i64>) -> Vec<Step> {
    let mut out = Vec::with_capacity(work.len());
    for (minute, pass, offset) in work {
        match pass {
            Pass::WildOnly if *last_wild == Some(minute) => continue,
            Pass::WildOnly | Pass::All => *last_wild = Some(minute),
            Pass::FixedOnly => {}
        }
        out.push((minute, pass, offset));
    }
    out
}

/// Cluster mode decision: whether user crontabs may run on this host, given
/// the result of reading `<spool>/.cron.hostname`.
pub fn cluster_allows(cluster: bool, hostname_file: &io::Result<String>, hostname: &str) -> bool {
    if !cluster {
        return true;
    }
    match hostname_file {
        Ok(h) => h.trim() == hostname,
        Err(_) => false,
    }
}

#[derive(Debug, Default)]
struct SlotCounts {
    total: usize,
    per_user: HashMap<String, usize>,
}

/// Bounded accounting of running jobs, overall and per user.
#[derive(Debug, Clone)]
pub struct JobSlots {
    counts: Arc<Mutex<SlotCounts>>,
    max_total: usize,
    max_per_user: usize,
}

/// Holds one job slot; releases it on drop (including during a panic).
#[derive(Debug)]
pub struct SlotGuard {
    counts: Arc<Mutex<SlotCounts>>,
    user: String,
}

fn lock_counts(m: &Mutex<SlotCounts>) -> MutexGuard<'_, SlotCounts> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl JobSlots {
    pub fn new(max_total: usize, max_per_user: usize) -> JobSlots {
        JobSlots {
            counts: Arc::new(Mutex::new(SlotCounts::default())),
            max_total,
            max_per_user,
        }
    }

    /// Reserve a slot for `user`, or `None` when a limit is reached.
    pub fn try_acquire(&self, user: &str) -> Option<SlotGuard> {
        let mut c = lock_counts(&self.counts);
        let mine = c.per_user.get(user).copied().unwrap_or(0);
        if c.total >= self.max_total || mine >= self.max_per_user {
            return None;
        }
        c.total += 1;
        *c.per_user.entry(user.to_string()).or_insert(0) += 1;
        Some(SlotGuard {
            counts: Arc::clone(&self.counts),
            user: user.to_string(),
        })
    }

    /// Currently held slots (total, for `user`).
    pub fn in_use(&self, user: &str) -> (usize, usize) {
        let c = lock_counts(&self.counts);
        (c.total, c.per_user.get(user).copied().unwrap_or(0))
    }
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        let mut c = lock_counts(&self.counts);
        c.total = c.total.saturating_sub(1);
        if let Some(n) = c.per_user.get_mut(&self.user) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                c.per_user.remove(&self.user);
            }
        }
    }
}

pub struct Scheduler {
    pub db: Database,
    pub runner: Arc<Runner>,
    /// `-c`: only run spool crontabs when this host is the cluster host.
    pub cluster: bool,
    /// Apply `RANDOM_DELAY` offsets (disabled for one-shot runs).
    pub honor_delay: bool,
    slots: JobSlots,
    /// Last `.cron.hostname` read error kind, so it is logged once per change.
    cluster_err: Mutex<Option<io::ErrorKind>>,
    /// Jobs that could not be started (thread spawn failure or over cap).
    dispatch_failures: AtomicUsize,
}

impl Scheduler {
    pub fn new(db: Database, runner: Arc<Runner>, cluster: bool, honor_delay: bool) -> Scheduler {
        Scheduler {
            db,
            runner,
            cluster,
            honor_delay,
            slots: JobSlots::new(MAX_JOBS_TOTAL, MAX_JOBS_PER_USER),
            cluster_err: Mutex::new(None),
            dispatch_failures: AtomicUsize::new(0),
        }
    }

    /// Number of jobs that could not be started since the last call.
    pub fn take_dispatch_failures(&self) -> usize {
        self.dispatch_failures.swap(0, Ordering::Relaxed)
    }

    fn cluster_allows_user_tabs(&self) -> bool {
        if !self.cluster {
            return true;
        }
        let path = self.db.config().spool_dir.join(".cron.hostname");
        let read = std::fs::read_to_string(&path);
        let mut last = self.cluster_err.lock().unwrap_or_else(|e| e.into_inner());
        match &read {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                if *last != Some(e.kind()) {
                    log::error!(
                        "(CRON) ERROR (can't read {}: {e}; user crontabs disabled)",
                        path.display()
                    );
                    *last = Some(e.kind());
                }
            }
            _ => *last = None,
        }
        cluster_allows(true, &read, &self.runner.hostname)
    }

    fn job_user(tab: &LoadedTab, entry: &Entry) -> Option<String> {
        entry.user.clone().or_else(|| tab.owner.clone())
    }

    fn dispatch(
        &self,
        tab: &LoadedTab,
        entry: &Entry,
        delay_minutes: u32,
    ) -> Option<JoinHandle<bool>> {
        let user = Self::job_user(tab, entry)?;
        let Some(slot) = self.slots.try_acquire(&user) else {
            log::error!(
                "({user}) ERROR (too many running jobs; job skipped: {})",
                entry.raw_command
            );
            self.dispatch_failures.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        let runner = Arc::clone(&self.runner);
        let entry = entry.clone();
        let label = tab.label.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("job-{user}"))
            .spawn(move || {
                let _slot = slot;
                if delay_minutes > 0 {
                    std::thread::sleep(Duration::from_secs(delay_minutes as u64 * 60));
                }
                match runner.run(&user, &entry) {
                    Ok(_) => true,
                    Err(e) => {
                        log::error!("({label}) ERROR running job for {user}: {e}");
                        false
                    }
                }
            });
        match spawned {
            Ok(h) => Some(h),
            Err(e) => {
                // The closure (and its slot guard) was dropped with the error.
                log::error!("can't spawn job thread: {e}");
                self.dispatch_failures.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Start every `@reboot` job.
    pub fn run_reboot(&self) -> Vec<JoinHandle<bool>> {
        let users_ok = self.cluster_allows_user_tabs();
        let mut handles = Vec::new();
        for tab in self.db.tabs.values() {
            if tab.format == Format::User && !users_ok {
                continue;
            }
            for entry in tab
                .crontab
                .entries
                .iter()
                .filter(|e| e.schedule.is_reboot())
            {
                handles.extend(self.dispatch(tab, entry, 0));
            }
        }
        handles
    }

    /// Start the jobs due at local `minute`. `v_gmtoff` is the offset this
    /// evaluation uses and `gmtoff` the offset in effect now. As in cronie,
    /// jobs with `CRON_TZ` are skipped while the two differ.
    pub fn run_minute(
        &self,
        minute: i64,
        v_gmtoff: i32,
        gmtoff: i32,
        pass: Pass,
    ) -> Vec<JoinHandle<bool>> {
        let users_ok = self.cluster_allows_user_tabs();
        let local = clock::wall_time(minute);
        let mut handles = Vec::new();
        for tab in self.db.tabs.values() {
            if tab.format == Format::User && !users_ok {
                continue;
            }
            for entry in &tab.crontab.entries {
                let wild = entry.schedule.is_wild();
                let wanted = match pass {
                    Pass::All => true,
                    Pass::WildOnly => wild,
                    Pass::FixedOnly => !wild,
                };
                if !wanted {
                    continue;
                }
                let t: NaiveDateTime = match &entry.tz {
                    Some(_) if v_gmtoff != gmtoff => continue,
                    Some(zone) => clock::wall_time_in_tz(minute, v_gmtoff, zone),
                    None => local,
                };
                if entry.schedule.matches(&t) {
                    let delay = match (self.honor_delay, entry.random_delay) {
                        (true, RandomDelay::Minutes(max)) => {
                            (f64::from(max) * self.db.random_scale()) as u32
                        }
                        _ => 0,
                    };
                    handles.extend(self.dispatch(tab, entry, delay));
                }
            }
        }
        handles
    }

    /// Main loop: sleep to each minute boundary, refresh crontabs, run jobs.
    /// Returns when `term` becomes set.  `hup` forces a full reload.
    pub fn run_forever(&mut self, term: &AtomicBool, hup: &AtomicBool) {
        let (mut virtual_time, mut prev_gmtoff) = clock::now();
        let mut last_wild: Option<i64> = None;
        while !term.load(Ordering::Relaxed) {
            let now_secs = chrono::Utc::now().timestamp();
            let (_, gmtoff) = clock::at_epoch(now_secs);
            // Wake one second past the boundary, as Vixie cron does.
            let wake = clock::next_minute_epoch(now_secs, gmtoff) + 1;
            loop {
                if term.load(Ordering::Relaxed) || hup.load(Ordering::Relaxed) {
                    break;
                }
                let left = wake - chrono::Utc::now().timestamp();
                // `left > 65` means the clock stepped backwards: stop waiting
                // for the old deadline so the planner sees the jump.
                if left <= 0 || left > 65 {
                    break;
                }
                std::thread::sleep(Duration::from_millis((left.min(1) * 1000) as u64));
            }
            if term.load(Ordering::Relaxed) {
                break;
            }
            if hup.swap(false, Ordering::Relaxed) {
                log::info!("(CRON) INFO (SIGHUP received, reloading all crontabs)");
                self.db.forget_all();
                self.db.refresh();
                continue;
            }

            let (time_running, gmtoff) = clock::now();
            self.db.refresh();
            let (new_virtual, work) = plan(virtual_time, time_running);
            if work.len() > 1 || time_running - virtual_time < 0 {
                log::debug!(
                    "(CRON) clock change: {} minute(s), {} evaluation(s)",
                    time_running - virtual_time,
                    work.len()
                );
            }
            for (minute, pass, offset) in skip_repeated_wild(work, &mut last_wild) {
                let v_gmtoff = match offset {
                    Offset::Previous => prev_gmtoff,
                    Offset::Current => gmtoff,
                };
                self.run_minute(minute, v_gmtoff, gmtoff, pass);
            }
            // Detached job threads; failures are logged by the threads.
            self.take_dispatch_failures();
            if updates_previous_offset(virtual_time, time_running) {
                prev_gmtoff = gmtoff;
            }
            virtual_time = new_virtual;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::mail::Mailer;
    use std::fs;

    use Offset::{Current, Previous};

    #[test]
    fn plan_normal_and_early() {
        assert_eq!(plan(100, 101), (101, vec![(101, Pass::All, Current)]));
        assert_eq!(plan(100, 100), (100, vec![(100, Pass::WildOnly, Current)]));
    }

    #[test]
    fn plan_catch_up_uses_previous_offset_until_the_last_minute() {
        let (v, w) = plan(100, 103);
        assert_eq!(v, 103);
        assert_eq!(
            w,
            vec![
                (101, Pass::All, Previous),
                (102, Pass::All, Previous),
                (103, Pass::All, Current)
            ]
        );
    }

    #[test]
    fn plan_dst_forward() {
        let (v, w) = plan(100, 160);
        assert_eq!(v, 160);
        assert_eq!(w.len(), 61);
        assert_eq!(w[0], (160, Pass::WildOnly, Current));
        assert_eq!(w[1], (101, Pass::FixedOnly, Previous));
        assert_eq!(w[59], (159, Pass::FixedOnly, Previous));
        assert_eq!(w[60], (160, Pass::FixedOnly, Current));
        assert!(updates_previous_offset(100, 160));
    }

    #[test]
    fn plan_dst_back() {
        assert_eq!(plan(160, 101), (160, vec![(101, Pass::WildOnly, Current)]));
        assert_eq!(plan(160, 160), (160, vec![(160, Pass::WildOnly, Current)]));
        assert_eq!(plan(160, 161), (161, vec![(161, Pass::All, Current)]));
        assert!(!updates_previous_offset(160, 101));
    }

    #[test]
    fn plan_large_jump_resyncs() {
        assert_eq!(
            plan(100, 10_000),
            (10_000, vec![(10_000, Pass::All, Current)])
        );
        assert_eq!(plan(10_000, 100), (100, vec![(100, Pass::All, Current)]));
    }

    #[test]
    fn early_wake_does_not_repeat_wildcards() {
        let mut last = None;
        let (v, w) = plan(160, 161);
        assert_eq!(
            skip_repeated_wild(w, &mut last),
            vec![(161, Pass::All, Current)]
        );
        let (_, w) = plan(v, 161);
        assert!(skip_repeated_wild(w, &mut last).is_empty());

        let mut last = Some(161);
        let (v, w) = plan(161, 159);
        assert_eq!(
            skip_repeated_wild(w, &mut last),
            vec![(159, Pass::WildOnly, Current)]
        );
        let (v, w) = plan(v, 160);
        assert_eq!(
            skip_repeated_wild(w, &mut last),
            vec![(160, Pass::WildOnly, Current)]
        );
        let (_, w) = plan(v, 161);
        assert_eq!(
            skip_repeated_wild(w, &mut last),
            vec![(161, Pass::WildOnly, Current)]
        );

        let mut last = Some(101);
        let w = vec![(101, Pass::FixedOnly, Previous), (102, Pass::All, Current)];
        assert_eq!(skip_repeated_wild(w.clone(), &mut last), w);
        assert_eq!(last, Some(102));
    }

    #[test]
    fn cluster_decision() {
        let nf = || Err(io::Error::from(io::ErrorKind::NotFound));
        assert!(cluster_allows(false, &nf(), "host"));
        assert!(!cluster_allows(true, &nf(), "host"));
        assert!(!cluster_allows(
            true,
            &Err(io::Error::from(io::ErrorKind::PermissionDenied)),
            "host"
        ));
        assert!(!cluster_allows(true, &Ok("other\n".into()), "host"));
        assert!(cluster_allows(true, &Ok("  host \n".into()), "host"));
    }

    #[test]
    fn job_slot_limits() {
        let slots = JobSlots::new(3, 2);
        let a1 = slots.try_acquire("a").unwrap();
        let a2 = slots.try_acquire("a").unwrap();
        assert!(slots.try_acquire("a").is_none(), "per-user cap");
        let b1 = slots.try_acquire("b").unwrap();
        assert!(slots.try_acquire("c").is_none(), "total cap");
        assert_eq!(slots.in_use("a"), (3, 2));
        drop(a1);
        assert_eq!(slots.in_use("a"), (2, 1));
        let a3 = slots.try_acquire("a").unwrap();
        drop((a2, a3, b1));
        assert_eq!(slots.in_use("a"), (0, 0));

        // A panicking holder still releases its slot.
        let s2 = slots.clone();
        let r = std::thread::spawn(move || {
            let _g = s2.try_acquire("p").unwrap();
            panic!("boom");
        })
        .join();
        assert!(r.is_err());
        assert_eq!(slots.in_use("p"), (0, 0));
    }

    #[test]
    fn cluster_mode_gates_user_crontabs() {
        let me = nix::unistd::User::from_uid(nix::unistd::getuid())
            .unwrap()
            .unwrap()
            .name;
        let hostname = nix::unistd::gethostname()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            spool_dir: dir.path().join("spool"),
            system_crontab: dir.path().join("crontab"),
            cron_d_dir: dir.path().join("cron.d"),
            ..Config::default()
        };
        fs::create_dir_all(&cfg.spool_dir).unwrap();
        let a = dir.path().join("sys-ran");
        let b = dir.path().join("user-ran");
        fs::write(
            &cfg.system_crontab,
            format!("* * * * * {me} touch '{}'\n", a.display()),
        )
        .unwrap();
        fs::write(
            cfg.user_crontab(&me),
            format!("* * * * * touch '{}'\n", b.display()),
        )
        .unwrap();

        let mut db = Database::new(cfg.clone(), true, 0.0);
        db.refresh();
        assert_eq!(db.tabs.len(), 2, "{:?}", db.tabs.keys());
        let runner = Arc::new(Runner {
            default_path: cfg.default_path.clone(),
            default_shell: cfg.default_shell.clone(),
            inherit_path: false,
            mailer: Mailer::Off,
            hostname: hostname.clone(),
        });
        let sched = Scheduler::new(db, runner, true, false);
        let (minute, gmtoff) = clock::now();

        let join_all = |hs: Vec<JoinHandle<bool>>| {
            for h in hs {
                assert!(h.join().unwrap());
            }
        };
        join_all(sched.run_minute(minute, gmtoff, gmtoff, Pass::All));
        assert!(a.exists(), "system crontab must run in cluster mode");
        assert!(
            !b.exists(),
            "user crontab must not run without .cron.hostname"
        );

        fs::write(cfg.spool_dir.join(".cron.hostname"), "not-this-host\n").unwrap();
        join_all(sched.run_minute(minute, gmtoff, gmtoff, Pass::All));
        assert!(!b.exists(), "user crontab must not run on another host");

        fs::write(
            cfg.spool_dir.join(".cron.hostname"),
            format!("{hostname}\n"),
        )
        .unwrap();
        join_all(sched.run_minute(minute, gmtoff, gmtoff, Pass::All));
        assert!(b.exists(), "user crontab must run on the cluster host");
        assert_eq!(sched.take_dispatch_failures(), 0);
    }

    #[test]
    fn cron_tz_jobs_skipped_while_offset_changes() {
        let me = nix::unistd::User::from_uid(nix::unistd::getuid())
            .unwrap()
            .unwrap()
            .name;
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            spool_dir: dir.path().join("spool"),
            system_crontab: dir.path().join("crontab"),
            cron_d_dir: dir.path().join("cron.d"),
            ..Config::default()
        };
        let local = dir.path().join("local-ran");
        let zoned = dir.path().join("zoned-ran");
        fs::write(
            &cfg.system_crontab,
            format!(
                "* * * * * {me} touch '{}'\nCRON_TZ=UTC\n* * * * * {me} touch '{}'\n",
                local.display(),
                zoned.display()
            ),
        )
        .unwrap();
        let mut db = Database::new(cfg.clone(), true, 0.0);
        db.refresh();
        let runner = Arc::new(Runner {
            default_path: cfg.default_path.clone(),
            default_shell: cfg.default_shell.clone(),
            inherit_path: false,
            mailer: Mailer::Off,
            hostname: "h".into(),
        });
        let sched = Scheduler::new(db, runner, false, false);
        let (minute, gmtoff) = clock::now();
        for h in sched.run_minute(minute, gmtoff - 3600, gmtoff, Pass::All) {
            assert!(h.join().unwrap());
        }
        assert!(
            local.exists(),
            "local-time jobs still run during the change"
        );
        assert!(
            !zoned.exists(),
            "CRON_TZ jobs are skipped during the change"
        );
        for h in sched.run_minute(minute, gmtoff, gmtoff, Pass::All) {
            assert!(h.join().unwrap());
        }
        assert!(zoned.exists());
    }
}
