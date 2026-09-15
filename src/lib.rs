//! crontab-rs: a cronie/Vixie-compatible cron daemon (`crond`) and
//! crontab management utility (`crontab`).
//!
//! The documented, semver-stable API of this crate is limited to the
//! [`crontab`], [`schedule`] and [`tz`] modules (re-exported below as
//! [`Crontab`], [`Entry`], [`Format`] and [`Schedule`]). Every other
//! module is an implementation detail shared with the `crond` and
//! `crontab` binaries bundled in this crate; it is hidden from the
//! generated docs and is not covered by semver guarantees.

pub mod crontab;
pub mod schedule;
pub mod tz;

// Implementation details shared with the bundled `crond`/`crontab` binaries
// (which are separate crates and need access to these modules). Not part of
// the public API: not covered by semver, and hidden from generated docs.
#[doc(hidden)]
pub mod allow;
#[doc(hidden)]
pub mod cli;
#[doc(hidden)]
pub mod clock;
#[doc(hidden)]
pub mod config;
#[doc(hidden)]
pub mod daemon;
#[doc(hidden)]
pub mod database;
#[doc(hidden)]
pub mod job;
#[doc(hidden)]
pub mod logging;
#[doc(hidden)]
pub mod mail;
#[doc(hidden)]
pub mod privs;

pub use crontab::{Crontab, Entry, Format};
pub use schedule::Schedule;
