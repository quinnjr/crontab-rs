//! The scheduler: decides which minutes to evaluate after each wake-up
//! (with Vixie cron's clock-jump / DST handling) and dispatches jobs.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use chrono::NaiveDateTime;

use crate::clock;
use crate::crontab::{Entry, Format};
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

/// Given the last processed minute and the current minute, return the new
/// "virtual" minute and the (minute, pass) evaluations to perform.
///
/// * `diff == 1`: normal tick.
/// * `1 < diff <= 5`: short hiccup (load, suspend); replay every minute.
/// * `5 < diff <= 180`: clock jumped forward (e.g. DST start): run fixed-time
///   jobs for each skipped minute once, wildcard jobs only for now.
/// * `-180 <= diff < 0`: clock went back (e.g. DST end): run wildcard jobs
///   for now but hold the virtual clock so fixed-time jobs are not repeated.
/// * `diff == 0`: early wake-up; nothing to do.
/// * anything else: large change; resynchronise.
pub fn plan(virtual_time: i64, time_running: i64) -> (i64, Vec<(i64, Pass)>) {
    let diff = time_running - virtual_time;
    match diff {
        0 => (virtual_time, vec![]),
        1 => (time_running, vec![(time_running, Pass::All)]),
        d if d > 1 && d <= MAX_CATCHUP => (
            time_running,
            ((virtual_time + 1)..=time_running)
                .map(|m| (m, Pass::All))
                .collect(),
        ),
        d if d > MAX_CATCHUP && d <= MAX_JUMP => {
            let mut v: Vec<(i64, Pass)> = ((virtual_time + 1)..time_running)
                .map(|m| (m, Pass::FixedOnly))
                .collect();
            v.push((time_running, Pass::All));
            (time_running, v)
        }
        d if (-MAX_JUMP..0).contains(&d) => (virtual_time, vec![(time_running, Pass::WildOnly)]),
        _ => (time_running, vec![(time_running, Pass::All)]),
    }
}

pub struct Scheduler {
    pub db: Database,
    pub runner: Arc<Runner>,
    /// `-c`: only run spool crontabs when this host is the cluster host.
    pub cluster: bool,
    /// Apply `RANDOM_DELAY` offsets (disabled for one-shot runs).
    pub honor_delay: bool,
}

impl Scheduler {
    fn cluster_allows_user_tabs(&self) -> bool {
        if !self.cluster {
            return true;
        }
        let path = self.db.config().spool_dir.join(".cron.hostname");
        match std::fs::read_to_string(path) {
            Ok(h) => h.trim() == self.runner.hostname,
            Err(_) => false,
        }
    }

    fn job_user(tab: &LoadedTab, entry: &Entry) -> Option<String> {
        entry.user.clone().or_else(|| tab.owner.clone())
    }

    fn dispatch(
        &self,
        tab: &LoadedTab,
        entry: &Entry,
        delay_minutes: u32,
    ) -> Option<JoinHandle<()>> {
        let user = Self::job_user(tab, entry)?;
        let runner = Arc::clone(&self.runner);
        let entry = entry.clone();
        let label = tab.label.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("job-{user}"))
            .spawn(move || {
                if delay_minutes > 0 {
                    std::thread::sleep(Duration::from_secs(delay_minutes as u64 * 60));
                }
                if let Err(e) = runner.run(&user, &entry) {
                    log::error!("({label}) ERROR running job for {user}: {e}");
                }
            });
        match spawned {
            Ok(h) => Some(h),
            Err(e) => {
                log::error!("can't spawn job thread: {e}");
                None
            }
        }
    }

    /// Start every `@reboot` job.
    pub fn run_reboot(&self) -> Vec<JoinHandle<()>> {
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

    /// Start the jobs due at local `minute`.
    pub fn run_minute(&self, minute: i64, gmtoff: i32, pass: Pass) -> Vec<JoinHandle<()>> {
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
                let t: NaiveDateTime = match entry.tz {
                    Some(tz) => clock::wall_time_in_tz(minute, gmtoff, tz),
                    None => local,
                };
                if entry.schedule.matches(&t) {
                    let delay = if self.honor_delay { tab.delay } else { 0 };
                    handles.extend(self.dispatch(tab, entry, delay));
                }
            }
        }
        handles
    }

    /// Main loop: sleep to each minute boundary, refresh crontabs, run jobs.
    /// Returns when `term` becomes set.  `hup` forces a full reload.
    pub fn run_forever(&mut self, term: &AtomicBool, hup: &AtomicBool) {
        let (mut virtual_time, _) = clock::now();
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
                if left <= 0 {
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
            for (minute, pass) in work {
                self.run_minute(minute, gmtoff, pass);
            }
            virtual_time = new_virtual;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_normal_and_early() {
        assert_eq!(plan(100, 101), (101, vec![(101, Pass::All)]));
        assert_eq!(plan(100, 100), (100, vec![]));
    }

    #[test]
    fn plan_catch_up() {
        let (v, w) = plan(100, 104);
        assert_eq!(v, 104);
        assert_eq!(
            w,
            vec![
                (101, Pass::All),
                (102, Pass::All),
                (103, Pass::All),
                (104, Pass::All)
            ]
        );
    }

    #[test]
    fn plan_dst_forward() {
        let (v, w) = plan(100, 160);
        assert_eq!(v, 160);
        assert_eq!(w.len(), 60);
        assert_eq!(w[0], (101, Pass::FixedOnly));
        assert_eq!(w[58], (159, Pass::FixedOnly));
        assert_eq!(w[59], (160, Pass::All));
    }

    #[test]
    fn plan_dst_back() {
        // Clock went back an hour: wildcard jobs keep running, virtual time
        // holds until the clock catches up.
        assert_eq!(plan(160, 101), (160, vec![(101, Pass::WildOnly)]));
        assert_eq!(plan(160, 160), (160, vec![]));
        assert_eq!(plan(160, 161), (161, vec![(161, Pass::All)]));
    }

    #[test]
    fn plan_large_jump_resyncs() {
        assert_eq!(plan(100, 10_000), (10_000, vec![(10_000, Pass::All)]));
        assert_eq!(plan(10_000, 100), (100, vec![(100, Pass::All)]));
    }
}
