# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Crontab parsing now follows cronie 1.7.2. Differential tests against cronie and a code review found each of these differences:
  - A step may only follow `*` or a range, so `5/10` is rejected.
  - A step larger than its range is accepted with cronie's warning.
  - Reversed ranges such as `5-3` and `sat-sun` are accepted and select nothing.
  - Numbers follow C `int` conversion, so `4294967296` is 0 and `*/2147483648` is invalid.
  - A random range cannot take a step, and only the value it picks must be in range.
  - Month and weekday names must be the three-letter abbreviations. `@` shortcuts are case-sensitive.
  - Environment lines use a port of cronie's parser, so names such as `1FOO` are valid.
  - A `*` right after the time fields, or a missing command, is a bad command.
  - Commands keep trailing spaces, and a NUL byte ends a line as in C.
  - The last line must end with a newline.
- `CRON_TZ` is resolved like glibc's `TZ`, including zoneinfo paths and POSIX strings with DST rules. An empty `CRON_TZ` means local time. Jobs with `CRON_TZ` are skipped while the local UTC offset is changing, as in cronie.
- `RANDOM_DELAY` uses cronie's parsing and applies to the entries after it. `LANG`, `LC_*`, `LANGUAGE`, `RANDOM_DELAY` and `MAILFROM` are inherited from the daemon's environment.
- The `-q` job option is replaced by cronie's leading `-`, which hides a job from the log. Only system crontabs and root may use it. `-n` may appear once.
- `crond` skips only the bad lines of a crontab and loads the rest. A job naming an unknown user is skipped when it is due.
- `crontab` stops at the first syntax error, prints the warnings before it, and uses cronie's messages. `crontab -T` checks the leading `-` against the `-u` user.
- `crond` and `crontab` exit with status 1 on usage errors and on `-h`, as in cronie.
- `crond -n` no longer logs every line twice when its stderr is the systemd journal, and still logs to stderr when syslog is unavailable.
- `RANDOM_DELAY` delays a job by matching it late, as cronie does, instead of holding a job slot while sleeping.
- Zone files are read only by `crond`, never by the set-user-ID `crontab`. FIFOs, devices and oversized files are treated as unreadable, crafted zone files can no longer abort the process, and set-ID programs get glibc's path restrictions. Zones are cached per reload.
- Crontabs need not be UTF-8; commands and variables keep their exact bytes.
- cronie's limits on variables, entries, comment content and field length are enforced by `crontab` and, for user crontabs, by `crond`.
- `crontab -u` requires root even for your own name and cannot be combined with `-T`, `-n` or `-c`. `crontab` refuses to read a new crontab from a terminal without a file argument. `crontab -T` reports on stderr, and `crontab -e` exits 0 after leaving edits behind.
- `crond` logs parser problems in cronie's format at the info level, logs an unknown user once, and backs off between syslog reconnects. Help and version output no longer panic on write errors.
- Library API: `Entry::quiet` is now `Entry::dont_log`, `Entry::tz` was replaced by `Entry::cron_tz` (the raw value), and `Entry::random_delay` is a `RandomDelay`. `Crontab::random_delay` was removed. `Crontab::parse_with`, `Crontab::parse_bytes`, `Entry::encoding`, `ParseOptions`, `ParseOutput`, `Diagnostic` (including `BadRandomDelay` and `TooMuchGarbage`), the limit constants and the `tz` module were added. `EntryError::BadTimezone`, `BadRandomDelay` and `BadUsername` were removed, and `BadOption` and `PrematureEof` were added. `Schedule::parse_prefix_with` now takes a warnings sink.
- The Arch package depends on `tzdata`, and `chrono-tz` is no longer a dependency.

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
