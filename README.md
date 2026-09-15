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
user, replaced atomically. `/etc/cron.allow` and `/etc/cron.deny` are honoured
the same way cronie honours them. When `crontab` is installed set-user-ID root,
it reads input files and runs the editor with the invoking user's privileges.

### `crond`

```
-n / -f   foreground (log to stderr as well as syslog)
-p        permit any crontab (skip ownership/mode checks)
-s        send job output to syslog instead of mail
-m CMD    mail command reading an RFC 822 message on stdin, or "off"
-x FLAGS  debug logging
-P        inherit PATH from the daemon environment
-c        cluster mode (user crontabs run only on the host set by crontab -n)
-i        accepted for compatibility
--run-at "YYYY-MM-DD HH:MM"   run the jobs due at that minute once, then exit
```

`crond` reads `/etc/crontab`, `/etc/cron.d/*` and `/var/spool/cron/*`, and
rescans them every minute. `SIGHUP` forces a full reload and `SIGTERM` stops
the daemon. It keeps a lock and pid file at `/run/crond.pid`, and logs to
syslog under the `cron` facility.

## Crontab syntax supported

- Five time fields: numbers, ranges, lists, and steps (`*/15`, `1-10/3`, `5/20`).
  Month and weekday names are accepted, and `7` means Sunday.
- cronie's random ranges: `~`, `10~20`, `0~59/10`.
- Shortcuts: `@reboot @yearly @annually @monthly @weekly @daily @midnight @hourly`.
- Classic day-of-month / day-of-week OR rule when both fields are restricted.
- `%` sends the rest of the line to stdin, and `\%` gives a literal `%`.
- `-n` mails output only on failure. `-q` skips logging the job start.
- Environment lines, including quoted values. `LOGNAME` and `USER` are protected.
  `SHELL`, `PATH` and `HOME` can be overridden.
- `MAILTO` (empty disables mail), `MAILFROM`, `CONTENT_TYPE`.
- `CRON_TZ` per entry, and `RANDOM_DELAY` per file.
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
  and must not be group- or world-writable.

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
