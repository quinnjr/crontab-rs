# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1] - 2026-09-14

### Added

- Arch Linux `PKGBUILD` that replaces cronie as the system cron. It provides `cron`, conflicts with `cronie`, and aliases `cronie.service` to `crond.service`.
  - Ships `/etc/crontab`, `/etc/cron.deny`, `/etc/cron.d/0hourly` and the periodic job directories.
  - Adds `/etc/cron.d/0periodic`, which runs the daily, weekly and monthly jobs with `run-parts` because there is no anacron.
  - Adds a pacman hook that restarts `crond` after glibc or crontab-rs upgrades.

## [0.1.0] - 2026-09-14

### Added

- `crond`, a cron daemon compatible with cronie and Vixie cron.
  - Loads `/etc/crontab`, `/etc/cron.d/*` and `/var/spool/cron/<user>`, and rescans them every minute.
  - Checks crontab ownership, mode and link count. Files are opened without following symlinks.
  - Handles clock jumps the Vixie way. Gaps of up to 5 minutes are replayed. Forward jumps of up to 3 hours run fixed-time jobs once. Backward jumps keep wildcard jobs running without repeating fixed-time jobs.
  - Runs `@reboot` jobs once per boot.
  - Runs jobs as the target user, with its supplementary groups, in a new session and a clean environment.
  - Mails job output through sendmail or a `-m` command, or logs it to syslog.
  - Supports `MAILTO`, `MAILFROM`, `CONTENT_TYPE`, `CRON_TZ` and `RANDOM_DELAY`.
  - Takes a pid-file lock, reloads on `SIGHUP` and stops on `SIGTERM`.
  - Supports cluster mode (`-c`).
  - `--run-at` runs the jobs due at one minute and exits non-zero if any job cannot run.
  - Caps concurrent jobs at 64 per user and 512 in total, and job output at 1 MiB.
  - Refuses to run jobs for expired accounts.
- `crontab`, a crontab management utility.
  - Install from a file or stdin, list, edit, remove, prompt before removal, and syntax-test crontabs.
  - Operate on another user's crontab with `-u` (root only).
  - Set and show the cluster host with `-n` and `-c`.
  - Honours `cron.allow` and `cron.deny`, and denies access if either file is unreadable.
  - Validates crontabs before installing them, and writes the spool file atomically with mode 0600.
  - Safe to install set-user-ID root. Input files and the editor run as the invoking user, and privilege drops fail closed.
- Crontab syntax parser covering ranges, steps, lists, month and weekday names, `~` random ranges and `@` shortcuts.
  - Applies the day-of-month or day-of-week rule and splits commands at `%` for stdin.
  - Handles the `-n` and `-q` job prefixes and quoted environment lines.
- A library exposing the `crontab` and `schedule` modules as its stable API.
- A systemd unit, a sample `cron.d` hourly file, and a README.
- Unit and end-to-end test suites.
- Dual MIT and Apache-2.0 licensing, with `LICENSE-MIT` and `LICENSE-APACHE`.

[Unreleased]: https://github.com/quinnjr/crontab-rs/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/quinnjr/crontab-rs/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/quinnjr/crontab-rs/releases/tag/v0.1.0
