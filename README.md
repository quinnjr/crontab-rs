# crontab-rs

A drop-in replacement for cronie's `crond` and `crontab`, written in Rust.

## Binaries

### `crontab`

```
crontab [-u user] <file | ->        install a crontab (validated first)
crontab [-u user] -l                list
crontab [-u user] -e                edit with $VISUAL / $EDITOR / vi
crontab [-u user] -r [-i]           remove (prompt with -i)
crontab -T <file | ->               syntax-check without installing
crontab -n <host> | -c              set / show cluster host
```

Crontabs are stored as `/var/spool/cron/<user>`, mode `0600`, owned by the
user, replaced atomically. If `crontab` has to create the spool directory
itself, it creates it mode `0700`. `/etc/cron.allow` and `/etc/cron.deny` are
honoured the same way cronie honours them. When `crontab` is installed
set-user-ID root, it reads input files and runs the editor with the invoking
user's privileges.

`crontab -e` edits a temporary copy of the crontab and installs it only if it
changed and parses cleanly. That temp file is created in `$TMPDIR` (or `/tmp`
when set-user-ID, since a setuid process can't trust the invoking user's
`TMPDIR`) and removed once editing finishes successfully; if the edited file
still has errors and you decline to retry, the temp file is left in place and
its path is printed ("edits left in ...") so you don't lose the changes.

### `crond`

```
-n / -f   foreground (also log to stderr, unless stderr is the systemd journal
          and syslog is reachable)
-p        permit any crontab (skip ownership/mode checks)
-s        send job output to syslog instead of mail
-m CMD    mail command reading an RFC 822 message on stdin, or "off"
-x FLAGS  debug logging
-P        inherit PATH from the daemon environment
-c        cluster mode (user crontabs run only on the host set by crontab -n)
-i        accepted for compatibility
--run-at "YYYY-MM-DD HH:MM"   run the jobs due at that minute once, then exit
```

`crond --version` and `crontab --version` print `crond 0.1.0 (cronie-compatible)`
and `crontab 0.1.0 (cronie-compatible)`.

`crond` reads `/etc/crontab`, `/etc/cron.d/*` and `/var/spool/cron/*`, and
rescans them every minute. `SIGHUP` forces a full reload and `SIGTERM` stops
the daemon. It keeps a lock and pid file at `/run/crond.pid`, and logs to
syslog under the `cron` facility.

`--run-at` rejects an unparsable time with "bad --run-at time" and a non-zero
exit. It also exits non-zero if any job that was due could not be run, for
example because the daemon is not root and the job belongs to another user, or
the job's user does not exist. A normal run where every due job started, or
where no job was due, exits zero.

## Crontab syntax supported

- Five time fields: numbers, ranges, lists, and steps (`*/15`, `1-10/3`).
  A step may only follow `*` or a range. A step larger than its range is
  accepted with cronie's warning. A reversed range such as `5-3` selects nothing.
  Numbers follow C `int` conversion, as in cronie.
  Month and weekday names are the three-letter abbreviations in any case, and
  `7` means Sunday.
- cronie's random ranges: `~`, `~30`, `10~20`. A random range takes no step,
  and only the value it picks must be in range.
- Shortcuts: `@reboot @yearly @annually @monthly @weekly @daily @midnight @hourly`.
- Classic day-of-month / day-of-week OR rule when both fields are restricted.
- `%` sends the rest of the line to stdin, and `\%` gives a literal `%`.
- `-n` mails output only on failure, and may appear once. A `-` before the time
  fields hides the job from the log. Only system crontabs and root may use it.
- Environment lines, including quoted values. `LOGNAME` and `USER` are protected.
  `SHELL`, `PATH` and `HOME` can be overridden.
- `MAILTO` (empty disables mail), `MAILFROM`, `CONTENT_TYPE`.
- `CRON_TZ` and `RANDOM_DELAY` apply to the entries after them. `CRON_TZ` is
  resolved like glibc's `TZ`: zone names, zoneinfo paths and POSIX strings with
  DST rules all work, and an unknown value means UTC. An empty `CRON_TZ` means
  the daemon's local time. Jobs with `CRON_TZ` set, even to an empty value, are
  skipped while the local UTC offset is changing, as in cronie.
- Only `crond` reads zone files, never `crontab`. FIFOs, devices and oversized
  files are treated as unreadable, and set-ID programs get glibc's path
  restrictions.
- `RANDOM_DELAY` makes a job run that many minutes (scaled by a random factor
  chosen at startup) after its scheduled time, as cronie does. An out-of-range
  value is logged by the daemon and ignored.
- `LANG`, `LC_*`, `LANGUAGE`, `RANDOM_DELAY` and `MAILFROM` are inherited from
  the environment of `crond` (or `crontab`) before the file's own lines.
- `@` shortcuts are case-sensitive. Environment lines follow cronie's parser,
  so names such as `1FOO` are valid.
- `crond` skips a line with an error and logs it, and runs the rest of the
  crontab. A job naming an unknown user is skipped when it is due, with
  cronie's "getpwnam() failed - user unknown" message. `crontab` refuses to
  install a file with an error and stops at the first one, printing any
  earlier warnings.
- A `*` right after the time fields is a bad command, commands keep trailing
  spaces, and the last line must end with a newline.
- Crontabs need not be UTF-8. Commands, variables and input keep their exact
  bytes.
- cronie's limits apply: 1000 variables, 10000 entries, 32768 characters of
  comments and blank space between lines, and 131072-byte fields. `crontab`
  refuses a file over them and `crond` does not load such a user crontab.
- `-u` requires root, even for your own name, and cannot be combined with `-T`,
  `-n` or `-c`. Without a file argument, `crontab` refuses to read a new
  crontab from a terminal.
- System crontab user field. `cron.d` skips `*.rpmsave`, `*.pacnew`, `*~` and
  other package-manager leftovers.

## Semantics

- Jobs run under the target user's uid, gid and supplementary groups, in a new
  session, with `HOME` as the working directory. Each job gets a clean
  environment: `SHELL`, `PATH`, `HOME`, `LOGNAME` and `USER`, plus crontab
  variables.
- Stdout and stderr are combined and mailed with sendmail (auto-detected) or
  the `-m` command. With no mailer, output is logged as `CMDOUT`.
- Clock handling follows Vixie cron. Gaps of up to 5 minutes are replayed. When
  the clock jumps forward by up to 3 hours, such as at DST start, fixed-time jobs
  run once and wildcard jobs run for the current minute. When the clock goes back
  by up to 3 hours, wildcard jobs keep running and fixed-time jobs are not repeated.
- `@reboot` jobs run once per boot, tracked by `/run/crond.reboot`.
- Spool files are rejected when they are group- or world-accessible, have the
  wrong owner, or have more than one link. `/etc` crontabs must be owned by root
  and must not be group- or world-writable. Ownership is checked before mode,
  so a system crontab owned by the wrong user is rejected as "WRONG FILE
  OWNER" even if its permissions look fine.
- `cron.allow` and `cron.deny` are checked fail-closed: if either file exists
  but can't be read, every non-root user is denied rather than let through.
- A job's account is checked for expiry (the shadow `sp_expire` field) before
  it runs; an expired account's jobs are skipped. There is no full PAM stack
  behind this check.
- The daemon caps concurrent jobs at 512 total and 64 per user. Jobs beyond
  the cap are not started that minute.
- Job output (stdout and stderr combined) is capped at 1 MiB. Output past the
  cap is discarded and a truncation notice is appended to what's mailed or
  logged, and a warning is logged with the number of bytes discarded.
- When no mailer is configured and no MTA can be found, `crond` logs "No MTA
  installed" at startup and job output is logged instead of mailed.

## Arch Linux package

`pkg/arch/PKGBUILD` builds a package that replaces cronie as the system cron.
It provides `cron` and conflicts with `cronie`, so pacman offers to remove
cronie when you install it.

```sh
cd pkg/arch
makepkg -si
systemctl enable --now crond.service
```

The package installs `crond`, a set-user-ID `crontab`, `crond.service`, and
`cronie.service` as an alias of `crond.service`. An existing cronie enablement
therefore keeps working. It also ships `/etc/crontab`, `/etc/cron.deny`,
`/etc/cron.d/0hourly`, the `cron.hourly`, `cron.daily`, `cron.weekly` and
`cron.monthly` directories, and a pacman hook that restarts `crond` after a
glibc or crontab-rs upgrade.

crontab-rs has no anacron. `/etc/cron.d/0periodic` runs the daily, weekly and
monthly directories at fixed times instead, and jobs missed while the machine
is off are not caught up. PAM is not used.

## Install

```sh
cargo build --release
install -Dm755 target/release/crond   /usr/bin/crond
install -Dm4755 target/release/crontab /usr/bin/crontab
install -dm700 /var/spool/cron
install -Dm644 contrib/crond.service /usr/lib/systemd/system/crond.service
systemctl enable --now crond
```

Stop and disable cronie first. The two daemons use the same spool files.

## Testing

`cargo test` runs the unit tests and end-to-end tests for both binaries. The
end-to-end tests use a temporary spool. Paths can be overridden with
`CRONTAB_RS_SPOOL_DIR`, `CRONTAB_RS_SYSTEM_CRONTAB`, `CRONTAB_RS_CRON_D`,
`CRONTAB_RS_ALLOW`, `CRONTAB_RS_DENY`, `CRONTAB_RS_PID_FILE` and
`CRONTAB_RS_REBOOT_FILE`. The overrides are ignored when a binary runs
set-user-ID or set-group-ID.

## Library

The crate's only stable, documented API is `crontab_rs::{crontab, schedule}`
(re-exported as `Crontab`, `Entry`, `Format` and `Schedule`), for parsing and
evaluating crontab syntax. Every other module (`allow`, `clock`, `config`,
`daemon`, `database`, `job`, `logging`, `mail`, `privs`) is an internal
implementation detail shared with the `crond` and `crontab` binaries. It is
hidden from the generated docs and carries no semver guarantee.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
