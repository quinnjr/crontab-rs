//! Time-zone rules selected by a `TZ` / `CRON_TZ` value, resolved exactly the
//! way glibc's `tzset` + `localtime` resolve them.
//!
//! cronie applies `CRON_TZ=<value>` with `setenv("TZ", value); tzset();
//! localtime()`, so this module is a pure-Rust transcription of glibc's
//! `time/tzset.c` and `time/tzfile.c` (glibc 2.44), quirks included:
//!
//! * A leading `:` is stripped, then the value is tried as a TZif file
//!   (absolute path, or relative to `$TZDIR`, default `/usr/share/zoneinfo`).
//!   `posix/…` and `right/…` names resolve the same way.
//! * Otherwise it is parsed as a POSIX TZ string with glibc's lenient parser:
//!   offsets are read with `sscanf("%hu")` (so whitespace, signs and 16-bit
//!   wrap-around are accepted), hours are clamped to 24 and minutes/seconds to
//!   59, rule times accept any `%hu` hour and a leading `-`, and parse failures
//!   part-way through leave zeroed or partially filled rules in place rather
//!   than rejecting the string.
//! * A DST name without rules (`EST5EDT` when no such file exists, `CET-1CEST`)
//!   loads `$TZDIR/posixrules` and shifts its transitions with glibc's
//!   `__tzfile_default` arithmetic. That arithmetic uses the file-global
//!   `rule_dstoff`, which `__tzfile_read` never sets for files that have
//!   transitions, so in a fresh process it is `0` and fall-back transitions are
//!   shifted by the full DST offset. After the last transition of `posixrules`
//!   (2037) glibc switches to that file's footer (`EST5EDT,M3.2.0,M11.1.0`),
//!   i.e. US Eastern offsets. Both effects are reproduced because `date` (and
//!   therefore cronie) exhibit them.
//! * If `posixrules` is unusable, the built-in `M3.2.0,M11.1.0` rules apply.
//! * Unparsable values yield UTC offsets.
//!
//! Known, deliberate differences from glibc (all require corrupt or
//! pathological input):
//!
//! * In a long-running glibc process the stale `rule_dstoff` global depends on
//!   which zones were loaded before; we model a fresh process (`0`).
//! * A TZif footer that names a DST zone but has no rules would make glibc
//!   reload `posixrules` mid-computation and clobber its loaded file; we apply
//!   the built-in `M3.2.0,M11.1.0` rules instead. tzdata never emits this.
//! * TZif files with `typecnt == 0` are rejected (glibc reads uninitialised
//!   memory for them) and fall through to POSIX parsing.
//! * `Mm.n.d` rules that failed to parse with a month outside 1..=12 make glibc
//!   index `__mon_yday` out of bounds; we read `0` for such slots (which is what
//!   glibc 2.44 on x86-64 observably reads before the table).
//! * During an inserted leap second in a `right/` zone glibc displays second
//!   `60`; [`Zone::wall_time`] has no leap-second representation and shows `59`.
//! * Zone files must be regular files of at most [`MAX_TZIF_BYTES`] (1 MiB);
//!   glibc would block forever on a FIFO, open devices, and read files of any
//!   size. The path is first opened `O_PATH` (which follows symlinks but never
//!   runs a device's open handler), checked with `fstat`, and only then that
//!   exact inode is re-opened for reading through `/proc/self/fd`. Anything
//!   else, including a system without `/proc/self/fd`, is treated like a
//!   missing file (POSIX parsing).
//!
//! glibc re-`stat`s the zone file on every `localtime` and reloads it when it
//! changed. [`ZoneCache`] provides the same freshness for a long-running
//! daemon: every lookup re-`stat`s the resolved path, and file-backed zones
//! are shared by file identity, so differently spelled paths to one file
//! share one parsed copy.
//!
//! Like glibc, a set-user-ID or set-group-ID process (`getuid() != geteuid()`
//! or `getgid() != getegid()`, glibc's `__libc_enable_secure`) refuses
//! absolute paths other than `/etc/localtime` or ones starting with
//! `/usr/share/zoneinfo`, refuses values containing `../`, and ignores
//! `$TZDIR` (which the dynamic loader strips for such processes).

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::sync::Arc;

use chrono::{DateTime, NaiveDateTime};

const SECSPERDAY: i128 = 86_400;
const DEFAULT_TZDIR: &str = "/usr/share/zoneinfo";
const TZDEFRULES: &[u8] = b"posixrules";
const TZDEFAULT: &[u8] = b"/etc/localtime";
/// Largest zone file we read: 1 MiB. Real TZif files are a few KiB (the
/// biggest in tzdata is about 4.4 KiB); larger files are treated as unreadable
/// so a crontab cannot make the daemon slurp arbitrary amounts of data. This
/// also bounds the memory one cached zone can retain to what parsing 1 MiB can
/// produce, so a [`ZoneCache`] retains at most `capacity` times that.
const MAX_TZIF_BYTES: u64 = 1024 * 1024;
/// Where an `O_PATH` descriptor is re-opened for reading.
const PROC_FD_DIR: &str = "/proc/self/fd";

/// Time-zone rules selected by a `TZ`/`CRON_TZ` value, resolved like glibc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zone {
    inner: Inner,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Inner {
    Posix(PosixTz),
    File(Arc<TzFile>),
}

impl Zone {
    /// Resolve a non-empty TZ value exactly as glibc's tzset does:
    ///  1. strip one leading ':'
    ///  2. try to load it as a TZif file: absolute path as-is, otherwise
    ///     relative to $TZDIR (default /usr/share/zoneinfo).
    ///  3. otherwise parse it as a POSIX TZ string, consulting
    ///     `$TZDIR/posixrules` when a DST name is given without rules.
    ///  4. otherwise UTC.
    ///
    /// Callers handle the empty value themselves (cronie then uses the
    /// daemon's local zone); an empty value (or just ":") yields UTC here.
    pub fn from_tz_value(value: &str) -> Zone {
        Zone::from_tz_bytes(value.as_bytes())
    }

    /// Like [`Zone::from_tz_value`], for raw bytes (`CRON_TZ` values from
    /// non-UTF-8 crontabs are paths/strings of bytes).
    pub fn from_tz_bytes(value: &[u8]) -> Zone {
        let value = normalize_value(value);
        if value.is_empty() {
            return Zone::utc();
        }
        let tzdir = tzdir();
        if let Some(raw) = load_zone_file(&tzdir, value) {
            return Zone {
                inner: Inner::File(Arc::new(raw.file)),
            };
        }
        resolve_posix(&tzdir, value).0
    }

    /// Seconds east of UTC in effect at the given Unix time (glibc's
    /// `tm_gmtoff`, i.e. what `date +%z` prints).
    ///
    /// For `right/` zones this does not include the leap-second correction;
    /// [`Zone::wall_time`] does.
    pub fn utc_offset_at(&self, unix_seconds: i64) -> i32 {
        self.local(unix_seconds).0
    }

    /// Wall-clock time in this zone at the given Unix time (seconds only, no
    /// leap-second display). Equals `unix_seconds + offset - leap_correction`,
    /// saturating at chrono's representable range.
    pub fn wall_time(&self, unix_seconds: i64) -> chrono::NaiveDateTime {
        let (offset, correction) = self.local(unix_seconds);
        let secs = i128::from(unix_seconds) + i128::from(offset) - i128::from(correction);
        let clamped = i64::try_from(secs).unwrap_or(if secs < 0 { i64::MIN } else { i64::MAX });
        match DateTime::from_timestamp(clamped, 0) {
            Some(dt) => dt.naive_utc(),
            None if clamped < 0 => NaiveDateTime::MIN,
            None => NaiveDateTime::MAX,
        }
    }

    /// UTC, for convenience/tests.
    pub fn utc() -> Zone {
        Zone {
            inner: Inner::Posix(PosixTz::default()),
        }
    }

    /// `(tm_gmtoff, leap_correction)` as computed by glibc's `__tz_convert`.
    fn local(&self, t: i64) -> (i32, i64) {
        match &self.inner {
            Inner::Posix(p) => (p.offset_at(t), 0),
            Inner::File(f) => f.local(t),
        }
    }
}

fn tzdir() -> OsString {
    if secure_mode() {
        // ld.so removes TZDIR from the environment of secure processes.
        return OsString::from(DEFAULT_TZDIR);
    }
    match std::env::var_os("TZDIR") {
        Some(d) if !d.is_empty() => d,
        _ => OsString::from(DEFAULT_TZDIR),
    }
}

/// glibc's `__libc_enable_secure`: set-user-ID or set-group-ID process.
fn secure_mode() -> bool {
    // SAFETY: these calls take no arguments and cannot fail.
    unsafe { libc::getuid() != libc::geteuid() || libc::getgid() != libc::getegid() }
}

/// The path guard `__tzfile_read` applies when `__libc_enable_secure` is set,
/// with glibc's exact comparisons (the TZDIR check is a plain prefix match, so
/// `/usr/share/zoneinfoX/…` passes).
fn path_allowed_in_secure_mode<V: AsRef<[u8]> + ?Sized>(value: &V) -> bool {
    let value = value.as_ref();
    let bad_absolute = value.starts_with(b"/")
        && value != TZDEFAULT
        && !value.starts_with(DEFAULT_TZDIR.as_bytes());
    !(bad_absolute || value.windows(3).any(|w| w == b"../"))
}

/// The value as glibc sees it: cut at the first NUL (it reaches glibc as a C
/// string), then one leading `:` removed.
fn normalize_value(value: &[u8]) -> &[u8] {
    let value = value.split(|&c| c == 0).next().unwrap_or(&[]);
    value.strip_prefix(b":").unwrap_or(value)
}

/// The file `__tzfile_read(value)` would open, or `None` when the secure-mode
/// guard refuses it.
fn zone_path(tzdir: &OsStr, value: &[u8]) -> Option<OsString> {
    if secure_mode() && !path_allowed_in_secure_mode(value) {
        return None;
    }
    Some(if value.starts_with(b"/") {
        OsString::from_vec(value.to_vec())
    } else {
        let mut p = tzdir.to_os_string();
        p.push("/");
        p.push(OsStr::from_bytes(value));
        p
    })
}

/// `__tzfile_read(value)`: resolve against `tzdir` and load, honouring the
/// secure-mode guard. `None` means "no usable file" (glibc's `lose` paths).
fn load_zone_file(tzdir: &OsStr, value: &[u8]) -> Option<RawTzif> {
    load_tzif(&zone_path(tzdir, value)?)
}

/// Parse `value` as a POSIX TZ string (step 3 of [`Zone::from_tz_bytes`]).
/// Also returns, when `$TZDIR/posixrules` was consulted, the identity of that
/// file as probed just before loading it (`None` inside: no regular file).
fn resolve_posix(tzdir: &OsStr, value: &[u8]) -> (Zone, Option<Option<FileId>>) {
    let mut posixrules = None;
    let parsed = parse_posix(value, &mut |stdoff, dstoff| {
        posixrules = Some(zone_path(tzdir, TZDEFRULES).and_then(|p| probe(&p)));
        posixrules_default(tzdir, stdoff, dstoff)
    });
    let inner = match parsed {
        Parsed::Posix(p) => Inner::Posix(p),
        Parsed::File(f) => Inner::File(Arc::new(f)),
    };
    (Zone { inner }, posixrules)
}

// ---------------------------------------------------------------------------
// Zone cache for long-running processes
// ---------------------------------------------------------------------------

/// Identity of a regular file, as `stat` reports it after following symlinks.
/// The size and timestamps detect in-place rewrites (glibc compares dev, ino
/// and mtime).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FileId {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl FileId {
    fn of(meta: &std::fs::Metadata) -> FileId {
        FileId {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        }
    }
}

/// `stat(path)` following symlinks: the identity of the regular file there, or
/// `None` if there is none (missing, unreachable, or not a regular file). This
/// never opens the path.
fn probe(path: &OsStr) -> Option<FileId> {
    let meta = std::fs::metadata(path).ok()?;
    meta.is_file().then(|| FileId::of(&meta))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CacheKey {
    /// A zone file, by identity: every spelling of its path shares the entry.
    File(FileId),
    /// A value resolved as a POSIX TZ string, by its normalized bytes.
    Posix(Vec<u8>),
}

#[derive(Debug)]
enum CacheValue {
    /// The parsed file, or `None` if this exact file is not a usable TZif file
    /// (too large or unparsable), so it is not re-read on every lookup.
    File(Option<Arc<TzFile>>),
    Posix {
        zone: Zone,
        /// `$TZDIR` in effect when resolved.
        tzdir: OsString,
        /// Probed identity of `posixrules` if the string consulted it.
        posixrules: Option<Option<FileId>>,
    },
}

/// A bounded cache of resolved zones for a long-running daemon.
///
/// Every [`ZoneCache::get`] re-`stat`s the file the value names (like glibc's
/// `localtime`), so a replaced, rewritten, created or removed zone file is
/// picked up on the next lookup. File-backed zones are keyed by file identity
/// (device, inode, size, mtime, ctime), so all spellings of one path share a
/// single parsed [`Arc`]; POSIX-string results are keyed by the value. At most
/// `capacity` entries are held; the least recently used one is evicted.
#[derive(Debug)]
pub struct ZoneCache {
    capacity: usize,
    tick: u64,
    entries: HashMap<CacheKey, (CacheValue, u64)>,
}

impl ZoneCache {
    /// An empty cache holding at most `capacity` entries (`0` caches nothing).
    pub fn new(capacity: usize) -> ZoneCache {
        ZoneCache {
            capacity,
            tick: 0,
            entries: HashMap::new(),
        }
    }

    /// Resolve `value` (non-empty TZ/CRON_TZ bytes) exactly like
    /// [`Zone::from_tz_bytes`], reusing parsed data.
    pub fn get(&mut self, value: &[u8]) -> Zone {
        let value = normalize_value(value);
        if value.is_empty() {
            return Zone::utc();
        }
        let tzdir = tzdir();
        if let Some(path) = zone_path(&tzdir, value)
            && let Some(id) = probe(&path)
        {
            match self.lookup(&CacheKey::File(id)) {
                Some(CacheValue::File(Some(file))) => {
                    return Zone {
                        inner: Inner::File(Arc::clone(file)),
                    };
                }
                Some(_) => {}
                None => match open_zone(&path, PROC_FD_DIR) {
                    Opened::Bytes(id, bytes) => {
                        let file = parse_tzif(&bytes).map(|raw| Arc::new(raw.file));
                        self.insert(CacheKey::File(id), CacheValue::File(file.clone()));
                        if let Some(file) = file {
                            return Zone {
                                inner: Inner::File(file),
                            };
                        }
                    }
                    Opened::Rejected(id) => {
                        self.insert(CacheKey::File(id), CacheValue::File(None));
                    }
                    Opened::Unusable => {}
                },
            }
        }

        let key = CacheKey::Posix(value.to_vec());
        if let Some(CacheValue::Posix {
            zone,
            tzdir: cached_tzdir,
            posixrules,
        }) = self.lookup(&key)
            && *cached_tzdir == tzdir
            && posixrules
                .is_none_or(|id| zone_path(&tzdir, TZDEFRULES).and_then(|p| probe(&p)) == id)
        {
            return zone.clone();
        }
        let (zone, posixrules) = resolve_posix(&tzdir, value);
        self.insert(
            key,
            CacheValue::Posix {
                zone: zone.clone(),
                tzdir,
                posixrules,
            },
        );
        zone
    }

    /// The entry for `key`, marked as most recently used.
    fn lookup(&mut self, key: &CacheKey) -> Option<&CacheValue> {
        self.tick += 1;
        let tick = self.tick;
        self.entries.get_mut(key).map(|(value, used)| {
            *used = tick;
            &*value
        })
    }

    /// Store `value`, evicting least recently used entries beyond capacity.
    /// Eviction scans all entries; it only runs on a miss.
    fn insert(&mut self, key: CacheKey, value: CacheValue) {
        if self.capacity == 0 {
            return;
        }
        self.tick += 1;
        self.entries.insert(key, (value, self.tick));
        while self.entries.len() > self.capacity {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }

    #[cfg(test)]
    fn file_entries(&self) -> usize {
        self.entries
            .keys()
            .filter(|k| matches!(k, CacheKey::File(_)))
            .count()
    }
}

// ---------------------------------------------------------------------------
// POSIX TZ strings (glibc time/tzset.c)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum RuleKind {
    /// `n`: zero-based day of year, counting Feb 29.
    #[default]
    J0,
    /// `Jn`: one-based day of year, never counting Feb 29.
    J1,
    /// `Mm.n.d`.
    M,
}

/// glibc's `tz_rule`; the all-zero value is what `memset` leaves behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Rule {
    kind: RuleKind,
    m: u16,
    n: u16,
    d: u16,
    secs: i32,
    /// Seconds east of UTC.
    offset: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct PosixTz {
    /// `[standard, daylight]`.
    rules: [Rule; 2],
}

enum Parsed {
    Posix(PosixTz),
    File(TzFile),
}

/// Byte at `i`, or NUL past the end (C-string semantics).
fn at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

/// `sscanf("%hu")` on `s[p..]`: returns the value and the index after it.
fn scan_hu(s: &[u8], p: usize) -> Option<(u16, usize)> {
    let mut i = p;
    while matches!(at(s, i), b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
        i += 1;
    }
    let negative = match at(s, i) {
        b'-' => {
            i += 1;
            true
        }
        b'+' => {
            i += 1;
            false
        }
        _ => false,
    };
    let start = i;
    let mut value: u64 = 0;
    let mut overflow = false;
    while at(s, i).is_ascii_digit() {
        let digit = u64::from(at(s, i) - b'0');
        match value.checked_mul(10).and_then(|v| v.checked_add(digit)) {
            Some(v) => value = v,
            None => overflow = true,
        }
        i += 1;
    }
    if i == start {
        return None;
    }
    // strtoul semantics, then truncation to unsigned short.
    let value = if overflow {
        u64::MAX
    } else if negative {
        value.wrapping_neg()
    } else {
        value
    };
    Some((value as u16, i))
}

/// `sscanf(s, "%hu%n:%hu%n:%hu%n", ...)`: fills `vals` in order and returns
/// `(conversions, consumed)`.
fn scan_hms(s: &[u8], p: usize, vals: &mut [u16; 3]) -> (usize, usize) {
    let mut q = p;
    let mut count = 0;
    let mut consumed = 0;
    for (k, slot) in vals.iter_mut().enumerate() {
        if k > 0 {
            if at(s, q) != b':' {
                break;
            }
            q += 1;
        }
        match scan_hu(s, q) {
            Some((v, end)) => {
                *slot = v;
                q = end;
                count += 1;
                consumed = q - p;
            }
            None => break,
        }
    }
    (count, consumed)
}

fn parse_tzname(s: &[u8], p: usize) -> Option<usize> {
    let mut q = p;
    while at(s, q).is_ascii_alphabetic() {
        q += 1;
    }
    if q - p >= 3 {
        return Some(q);
    }
    if at(s, p) != b'<' {
        return None;
    }
    let start = p + 1;
    q = start;
    while matches!(at(s, q), b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'+' | b'-') {
        q += 1;
    }
    if at(s, q) != b'>' || q - start < 3 {
        return None;
    }
    Some(q + 1)
}

fn parse_offset(s: &[u8], mut p: usize, which: usize, rules: &mut [Rule; 2]) -> (bool, usize) {
    let c = at(s, p);
    if which == 0 && (c == 0 || (c != b'+' && c != b'-' && !c.is_ascii_digit())) {
        return (false, p);
    }
    let sign: i32 = match c {
        b'-' => {
            p += 1;
            1
        }
        b'+' => {
            p += 1;
            -1
        }
        _ => -1,
    };
    let mut hms = [0u16; 3];
    let (count, consumed) = scan_hms(s, p, &mut hms);
    if count > 0 {
        let secs = u32::from(hms[2].min(59))
            + u32::from(hms[1].min(59)) * 60
            + u32::from(hms[0].min(24)) * 3600;
        rules[which].offset = sign * secs as i32;
    } else if which == 0 {
        rules[0].offset = 0;
        return (false, p);
    } else {
        rules[1].offset = rules[0].offset + 3600;
    }
    (true, p + consumed)
}

fn parse_rule(s: &[u8], mut p: usize, which: usize, rules: &mut [Rule; 2]) -> Option<usize> {
    let tzr = &mut rules[which];
    if at(s, p) == b',' {
        p += 1;
    }
    let c = at(s, p);
    if c == b'J' || c.is_ascii_digit() {
        tzr.kind = if c == b'J' {
            RuleKind::J1
        } else {
            RuleKind::J0
        };
        if tzr.kind == RuleKind::J1 {
            p += 1;
            if !at(s, p).is_ascii_digit() {
                return None;
            }
        }
        let mut d: u64 = 0;
        while at(s, p).is_ascii_digit() {
            d = d
                .saturating_mul(10)
                .saturating_add(u64::from(at(s, p) - b'0'));
            p += 1;
        }
        if d > 365 || (tzr.kind == RuleKind::J1 && d == 0) {
            return None;
        }
        tzr.d = d as u16;
    } else if c == b'M' {
        tzr.kind = RuleKind::M;
        let mut q = p + 1;
        let mut count = 0;
        for k in 0..3 {
            if k > 0 {
                if at(s, q) != b'.' {
                    break;
                }
                q += 1;
            }
            let Some((v, end)) = scan_hu(s, q) else {
                break;
            };
            match k {
                0 => tzr.m = v,
                1 => tzr.n = v,
                _ => tzr.d = v,
            }
            q = end;
            count += 1;
        }
        if count != 3 || !(1..=12).contains(&tzr.m) || !(1..=5).contains(&tzr.n) || tzr.d > 6 {
            return None;
        }
        p = q;
    } else if c == 0 {
        tzr.kind = RuleKind::M;
        (tzr.m, tzr.n, tzr.d) = if which == 0 { (3, 2, 0) } else { (11, 1, 0) };
    } else {
        return None;
    }

    match at(s, p) {
        0 | b',' => tzr.secs = 2 * 3600,
        b'/' => {
            p += 1;
            if at(s, p) == 0 {
                return None;
            }
            let negative = at(s, p) == b'-';
            if negative {
                p += 1;
            }
            let mut hms = [2u16, 0, 0];
            let (_, consumed) = scan_hms(s, p, &mut hms);
            p += consumed;
            let secs = i32::from(hms[0]) * 3600 + i32::from(hms[1]) * 60 + i32::from(hms[2]);
            tzr.secs = if negative { -secs } else { secs };
        }
        _ => return None,
    }
    Some(p)
}

/// glibc's `__tzset_parse_tz`. `default_rules` models `__tzfile_default`.
fn parse_posix(s: &[u8], default_rules: &mut dyn FnMut(i32, i32) -> Option<TzFile>) -> Parsed {
    let mut rules = [Rule::default(); 2];
    if let Some(mut p) = parse_tzname(s, 0) {
        let (ok, q) = parse_offset(s, p, 0, &mut rules);
        if ok {
            p = q;
            if at(s, p) != 0 {
                if let Some(q) = parse_tzname(s, p) {
                    let (_, q) = parse_offset(s, q, 1, &mut rules);
                    p = q;
                    let no_rules = at(s, p) == 0 || (at(s, p) == b',' && at(s, p + 1) == 0);
                    if no_rules && let Some(file) = default_rules(rules[0].offset, rules[1].offset)
                    {
                        return Parsed::File(file);
                    }
                }
                if let Some(q) = parse_rule(s, p, 0, &mut rules) {
                    parse_rule(s, q, 1, &mut rules);
                }
            } else {
                rules[1].offset = rules[0].offset;
            }
        }
    }
    Parsed::Posix(PosixTz { rules })
}

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// glibc's `__mon_yday`, flattened; out-of-range reads yield 0.
fn mon_yday(index: i64) -> i128 {
    const TABLE: [u16; 26] = [
        0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334, 365, //
        0, 31, 60, 91, 121, 152, 182, 213, 244, 274, 305, 335, 366,
    ];
    usize::try_from(index)
        .ok()
        .and_then(|i| TABLE.get(i))
        .map_or(0, |&v| i128::from(v))
}

/// glibc's `compute_change`: when `rule` takes effect in `year`.
fn compute_change(rule: &Rule, year: i64) -> i128 {
    let y = i128::from(year);
    let mut t: i128 = if year > 1970 {
        ((y - 1970) * 365 + ((y - 1) / 4 - 1970 / 4) - ((y - 1) / 100 - 1970 / 100)
            + ((y - 1) / 400 - 1970 / 400))
            * SECSPERDAY
    } else {
        0
    };
    match rule.kind {
        RuleKind::J1 => {
            t += (i128::from(rule.d) - 1) * SECSPERDAY;
            if rule.d >= 60 && is_leap(year) {
                t += SECSPERDAY;
            }
        }
        RuleKind::J0 => t += i128::from(rule.d) * SECSPERDAY,
        RuleKind::M => {
            let base = if is_leap(year) { 13 } else { 0 } + i64::from(rule.m);
            let myday0 = mon_yday(base);
            let myday_prev = mon_yday(base - 1);
            t += myday_prev * SECSPERDAY;
            let m = i128::from(rule.m);
            let m1 = (m + 9) % 12 + 1;
            let yy0 = if rule.m <= 2 { y - 1 } else { y };
            let yy1 = yy0 / 100;
            let yy2 = yy0 % 100;
            let mut dow = ((26 * m1 - 2) / 10 + 1 + yy2 + yy2 / 4 + yy1 / 4 - 2 * yy1) % 7;
            if dow < 0 {
                dow += 7;
            }
            let mut d = i128::from(rule.d) - dow;
            if d < 0 {
                d += 7;
            }
            for _ in 1..rule.n {
                if d + 7 >= myday0 - myday_prev {
                    break;
                }
                d += 7;
            }
            t += d * SECSPERDAY;
        }
    }
    t - i128::from(rule.offset) + i128::from(rule.secs)
}

/// Proleptic Gregorian UTC year containing Unix time `t`.
fn utc_year(t: i64) -> i64 {
    let z = t.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let year = yoe + era * 400;
    if mp >= 10 { year + 1 } else { year }
}

impl PosixTz {
    /// glibc's `__tz_compute` (the year is the UTC year of `t`).
    fn offset_at(&self, t: i64) -> i32 {
        let year = utc_year(t);
        let start = compute_change(&self.rules[0], year);
        let end = compute_change(&self.rules[1], year);
        let t = i128::from(t);
        let isdst = if start > end {
            t < end || t >= start
        } else {
            t >= start && t < end
        };
        self.rules[usize::from(isdst)].offset
    }
}

// ---------------------------------------------------------------------------
// TZif files (glibc time/tzfile.c)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TType {
    offset: i32,
    isdst: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Leap {
    transition: i64,
    change: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TzFile {
    transitions: Vec<i64>,
    type_idxs: Vec<u8>,
    types: Vec<TType>,
    leaps: Vec<Leap>,
    footer: Option<PosixTz>,
}

/// A TZif file plus the per-type indicators only `__tzfile_default` needs.
struct RawTzif {
    file: TzFile,
    isstd: Vec<bool>,
    isgmt: Vec<bool>,
}

struct Header {
    version: u8,
    isutcnt: usize,
    isstdcnt: usize,
    leapcnt: usize,
    timecnt: usize,
    typecnt: usize,
    charcnt: usize,
}

fn read_header(b: &[u8], pos: usize) -> Option<Header> {
    let h = b.get(pos..pos.checked_add(44)?)?;
    if &h[..4] != b"TZif" {
        return None;
    }
    let count = |i: usize| {
        let raw = i32::from_be_bytes([h[20 + 4 * i], h[21 + 4 * i], h[22 + 4 * i], h[23 + 4 * i]]);
        usize::try_from(raw).ok()
    };
    let header = Header {
        version: h[4],
        isutcnt: count(0)?,
        isstdcnt: count(1)?,
        leapcnt: count(2)?,
        timecnt: count(3)?,
        typecnt: count(4)?,
        charcnt: count(5)?,
    };
    if header.isstdcnt > header.typecnt || header.isutcnt > header.typecnt {
        return None;
    }
    Some(header)
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.b.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }

    fn int(&mut self, width: usize) -> Option<i64> {
        let s = self.take(width)?;
        Some(if width == 4 {
            i64::from(i32::from_be_bytes([s[0], s[1], s[2], s[3]]))
        } else {
            i64::from_be_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]])
        })
    }
}

fn load_tzif(path: &OsStr) -> Option<RawTzif> {
    parse_tzif(&read_zone_bytes(path)?)
}

/// Bytes of the zone file at `path`, if it is a readable regular file within
/// [`MAX_TZIF_BYTES`] (see [`open_zone`]).
fn read_zone_bytes(path: &OsStr) -> Option<Vec<u8>> {
    match open_zone(path, PROC_FD_DIR) {
        Opened::Bytes(_, bytes) => Some(bytes),
        Opened::Rejected(_) | Opened::Unusable => None,
    }
}

/// Outcome of [`open_zone`].
#[derive(Debug)]
enum Opened {
    /// The whole content of the regular file with this identity.
    Bytes(FileId, Vec<u8>),
    /// A regular file, larger than [`MAX_TZIF_BYTES`].
    Rejected(FileId),
    /// Missing, not a regular file, or not readable right now.
    Unusable,
}

/// `open(path, O_PATH | O_CLOEXEC)`: follows symlinks but neither reads nor
/// runs a device's or FIFO's open handler, so it cannot block or have side
/// effects.
fn open_o_path(path: &OsStr) -> Option<OwnedFd> {
    let c = std::ffi::CString::new(path.as_bytes()).ok()?;
    // SAFETY: `c` is a valid NUL-terminated string.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    // SAFETY: a non-negative return is a fresh descriptor we now own.
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Read a zone file without side effects and with bounded memory. The path is
/// opened `O_PATH` and `fstat`ed; only a regular file within
/// [`MAX_TZIF_BYTES`] is then re-opened for reading, as that exact inode,
/// through `<proc_fd_dir>/<fd>` (`O_RDONLY | O_NONBLOCK | O_CLOEXEC`), and
/// checked again with `fstat` before reading. Without `proc_fd_dir` the file
/// counts as unreadable; the path itself is never opened for reading.
fn open_zone(path: &OsStr, proc_fd_dir: &str) -> Opened {
    let Some(handle) = open_o_path(path) else {
        return Opened::Unusable;
    };
    let handle = std::fs::File::from(handle);
    let Ok(meta) = handle.metadata() else {
        return Opened::Unusable;
    };
    if !meta.is_file() {
        return Opened::Unusable;
    }
    let id = FileId::of(&meta);
    if meta.size() > MAX_TZIF_BYTES {
        return Opened::Rejected(id);
    }
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(format!("{proc_fd_dir}/{}", handle.as_raw_fd()))
    else {
        return Opened::Unusable;
    };
    match file.metadata() {
        Ok(m) if m.is_file() && m.dev() == id.dev && m.ino() == id.ino => {}
        _ => return Opened::Unusable,
    }
    match read_capped(file, MAX_TZIF_BYTES) {
        Some(bytes) => Opened::Bytes(id, bytes),
        None => Opened::Unusable,
    }
}

/// All bytes of `reader`, or `None` on error or if it holds more than `cap`.
fn read_capped(reader: impl Read, cap: u64) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(cap.checked_add(1)?)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > cap {
        return None;
    }
    Some(bytes)
}

/// Bytes the data block following a header occupies, excluding the v2+
/// footer: `timecnt*width + timecnt + typecnt*6 + charcnt +
/// leapcnt*(width+4) + isstdcnt + isutcnt`.
fn data_block_len(h: &Header, width: usize) -> Option<usize> {
    h.timecnt
        .checked_mul(width.checked_add(1)?)?
        .checked_add(h.typecnt.checked_mul(6)?)?
        .checked_add(h.charcnt)?
        .checked_add(h.leapcnt.checked_mul(width.checked_add(4)?)?)?
        .checked_add(h.isstdcnt)?
        .checked_add(h.isutcnt)
}

/// glibc's `__tzfile_read`, minus the global state.
fn parse_tzif(b: &[u8]) -> Option<RawTzif> {
    let first = read_header(b, 0)?;
    let (h, pos, width) = if first.version != 0 {
        let skip = data_block_len(&first, 4)?;
        let second_pos = skip.checked_add(44)?;
        (
            read_header(b, second_pos)?,
            second_pos.checked_add(44)?,
            8usize,
        )
    } else {
        (first, 44, 4usize)
    };
    // typecnt == 0 makes glibc read uninitialised memory; reject it.
    if h.typecnt == 0 {
        return None;
    }
    // Every count-driven allocation below is bounded by the input size: the
    // buffer must hold the whole data block before anything is allocated.
    // (glibc fails the same inputs with short reads.)
    let rem = b.len().checked_sub(pos)?;
    if data_block_len(&h, width)? > rem {
        return None;
    }

    let tzspec_len = if width == 8 {
        let data = h
            .timecnt
            .checked_mul(9)?
            .checked_add(h.typecnt.checked_mul(6)?)?
            .checked_add(h.charcnt)?;
        let mut len = rem.checked_sub(data)?;
        len = len.checked_sub(h.leapcnt.checked_mul(12)?)?;
        len = len.checked_sub(h.isstdcnt)?;
        if len == 0 || len - 1 < h.isutcnt {
            return None;
        }
        len -= h.isutcnt + 1;
        if len == 0 {
            return None;
        }
        Some(len)
    } else {
        None
    };

    let mut r = Reader { b, pos };
    let mut transitions = Vec::with_capacity(h.timecnt);
    for _ in 0..h.timecnt {
        transitions.push(r.int(width)?);
    }
    let type_idxs = r.take(h.timecnt)?.to_vec();
    if type_idxs.iter().any(|&i| usize::from(i) >= h.typecnt) {
        return None;
    }
    let mut types = Vec::with_capacity(h.typecnt);
    for _ in 0..h.typecnt {
        let offset = r.int(4)? as i32;
        let rec = r.take(2)?;
        if rec[0] > 1 || usize::from(rec[1]) > h.charcnt {
            return None;
        }
        types.push(TType {
            offset,
            isdst: rec[0] == 1,
        });
    }
    r.take(h.charcnt)?;
    let mut leaps = Vec::with_capacity(h.leapcnt);
    for _ in 0..h.leapcnt {
        let transition = r.int(width)?;
        let change = r.int(4)?;
        leaps.push(Leap { transition, change });
    }
    let mut isstd = vec![false; h.typecnt];
    for flag in isstd.iter_mut().take(h.isstdcnt) {
        *flag = r.take(1)?[0] != 0;
    }
    let mut isgmt = vec![false; h.typecnt];
    for flag in isgmt.iter_mut().take(h.isutcnt) {
        *flag = r.take(1)?[0] != 0;
    }

    let footer = tzspec_len.and_then(|len| {
        if r.take(1)? != b"\n" {
            return None;
        }
        let spec = r.take(len - 1)?;
        let spec = &spec[..spec.iter().position(|&c| c == 0).unwrap_or(spec.len())];
        if spec.is_empty() {
            return None;
        }
        match parse_posix(spec, &mut |_, _| None) {
            Parsed::Posix(p) => Some(p),
            Parsed::File(_) => None,
        }
    });

    Some(RawTzif {
        file: TzFile {
            transitions,
            type_idxs,
            types,
            leaps,
            footer,
        },
        isstd,
        isgmt,
    })
}

/// glibc's `__tzfile_default`: apply user offsets to `posixrules` transitions.
fn posixrules_default(tzdir: &OsStr, stdoff: i32, dstoff: i32) -> Option<TzFile> {
    let RawTzif {
        mut file,
        isstd,
        isgmt,
    } = load_zone_file(tzdir, TZDEFRULES)?;
    if file.types.len() < 2 {
        return None;
    }
    // `rule_stdoff` as left by `__tzfile_read`; `rule_dstoff` is only assigned
    // for transition-less files, otherwise it keeps its initial value 0.
    let (rule_stdoff, rule_dstoff) = if file.transitions.is_empty() {
        (file.types[0].offset, file.types[0].offset)
    } else {
        let stdoff = file
            .type_idxs
            .iter()
            .rev()
            .map(|&i| file.types[usize::from(i)])
            .find(|t| !t.isdst)
            .map_or(0, |t| t.offset);
        (stdoff, 0)
    };
    let mut isdst = false;
    for (trans, idx) in file.transitions.iter_mut().zip(file.type_idxs.iter_mut()) {
        let ty = usize::from(*idx);
        let tt = file.types[ty];
        *idx = u8::from(tt.isdst);
        if isgmt[ty] {
            // Transition time is in UT: no correction.
        } else if isdst && !isstd[ty] {
            *trans = trans.wrapping_add(i64::from(dstoff) - i64::from(rule_dstoff));
        } else {
            *trans = trans.wrapping_add(i64::from(stdoff) - i64::from(rule_stdoff));
        }
        isdst = tt.isdst;
    }
    file.types = vec![
        TType {
            offset: stdoff,
            isdst: false,
        },
        TType {
            offset: dstoff,
            isdst: true,
        },
    ];
    Some(file)
}

impl TzFile {
    /// glibc's `__tzfile_compute`: `(tm_gmtoff, leap_correction)`.
    fn local(&self, t: i64) -> (i32, i64) {
        let tr = &self.transitions;
        let n = tr.len();
        let offset = if n == 0 || t < tr[0] {
            let i = self.types.iter().position(|ty| !ty.isdst).unwrap_or(0);
            self.types[i].offset
        } else if t >= tr[n - 1] {
            match &self.footer {
                Some(p) => p.offset_at(t),
                None => self.types[usize::from(self.type_idxs[n - 1])].offset,
            }
        } else {
            let i = self.search(t);
            self.types[usize::from(self.type_idxs[i - 1])].offset
        };
        let correction = self
            .leaps
            .iter()
            .rev()
            .find(|l| t >= l.transition)
            .map_or(0, |l| l.change);
        (offset, correction)
    }

    /// Index of the first transition after `t`, found with glibc's search
    /// (identical to a binary search on well-formed, sorted files).
    /// Requires `transitions[0] <= t < transitions[n - 1]`.
    fn search(&self, t: i64) -> usize {
        let tr = &self.transitions;
        let n = tr.len();
        let mut lo = 0usize;
        let mut hi = n - 1;
        let guess = (i128::from(tr[n - 1]) - i128::from(t)) / 15_778_476;
        if guess < n as i128 {
            let mut i = n - 1 - guess as usize;
            if t < tr[i] {
                if i < 10 || t >= tr[i - 10] {
                    while i > 1 && t < tr[i - 1] {
                        i -= 1;
                    }
                    return i.max(1);
                }
                hi = i - 10;
            } else {
                if i + 10 >= n || t < tr[i + 10] {
                    while i < n - 1 && t >= tr[i] {
                        i += 1;
                    }
                    return i.max(1);
                }
                lo = i + 10;
            }
        }
        while lo + 1 < hi {
            let i = (lo + hi) / 2;
            if t < tr[i] {
                hi = i;
            } else {
                lo = i;
            }
        }
        hi.max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NY: &str = "/usr/share/zoneinfo/America/New_York";

    fn have_zoneinfo() -> bool {
        std::path::Path::new(NY).exists()
    }

    fn fmt_offset(off: i32) -> String {
        let sign = if off < 0 { '-' } else { '+' };
        let a = off.unsigned_abs();
        format!("{sign}{:02}:{:02}:{:02}", a / 3600, a / 60 % 60, a % 60)
    }

    fn render(zone: &Zone, t: i64) -> String {
        format!(
            "{} {}",
            zone.wall_time(t).format("%Y-%m-%d %H:%M:%S"),
            fmt_offset(zone.utc_offset_at(t))
        )
    }

    /// Each case: TZ value, Unix time, expected `date '+%F %T %::z'` output.
    fn check(cases: &[(&str, i64, &str)]) {
        let mut failures = Vec::new();
        for &(tz, t, want) in cases {
            let got = render(&Zone::from_tz_value(tz), t);
            if got != want {
                failures.push(format!("TZ={tz:?} @{t}: got {got}, want {want}"));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    // All expected strings below were produced on Arch Linux (glibc 2.44,
    // tzdata in /usr/share/zoneinfo) with `TZ=<value> date -d @<t> '+%F %T %::z'`.

    #[test]
    fn iana_names_across_dst_transitions() {
        if !have_zoneinfo() {
            return;
        }
        check(&[
            (
                "America/New_York",
                1772953199,
                "2026-03-08 01:59:59 -05:00:00",
            ),
            (
                "America/New_York",
                1772953200,
                "2026-03-08 03:00:00 -04:00:00",
            ),
            (
                "America/New_York",
                1793512799,
                "2026-11-01 01:59:59 -04:00:00",
            ),
            (
                "America/New_York",
                1793512800,
                "2026-11-01 01:00:00 -05:00:00",
            ),
            (
                "America/New_York",
                1782907200,
                "2026-07-01 08:00:00 -04:00:00",
            ),
            // After the last transition: TZif footer rules.
            (
                "America/New_York",
                2855908800,
                "2060-07-01 08:00:00 -04:00:00",
            ),
            // Before the first transition: LMT.
            (
                "America/New_York",
                -3786825600,
                "1849-12-31 19:03:58 -04:56:02",
            ),
            (
                "Australia/Sydney",
                1775318399,
                "2026-04-05 02:59:59 +11:00:00",
            ),
            (
                "Australia/Sydney",
                1775318400,
                "2026-04-05 02:00:00 +10:00:00",
            ),
            (
                "Australia/Sydney",
                1791043199,
                "2026-10-04 01:59:59 +10:00:00",
            ),
            (
                "Australia/Sydney",
                1791043200,
                "2026-10-04 03:00:00 +11:00:00",
            ),
            ("Europe/London", 1774745999, "2026-03-29 00:59:59 +00:00:00"),
            ("Europe/London", 1774746000, "2026-03-29 02:00:00 +01:00:00"),
            ("Europe/London", 1792889999, "2026-10-25 01:59:59 +01:00:00"),
            ("Europe/London", 1792890000, "2026-10-25 01:00:00 +00:00:00"),
            (":Europe/Paris", 1782864000, "2026-07-01 02:00:00 +02:00:00"),
            (":Europe/Paris", 1767225600, "2026-01-01 01:00:00 +01:00:00"),
        ]);
    }

    #[test]
    fn file_paths_and_zero_transition_files() {
        if !have_zoneinfo() {
            return;
        }
        check(&[
            (
                "/usr/share/zoneinfo/Asia/Tokyo",
                1782864000,
                "2026-07-01 09:00:00 +09:00:00",
            ),
            (
                "posix/Asia/Tokyo",
                1782864000,
                "2026-07-01 09:00:00 +09:00:00",
            ),
            ("Etc/GMT-3", 1782864000, "2026-07-01 03:00:00 +03:00:00"),
            ("UTC", 1782864000, "2026-07-01 00:00:00 +00:00:00"),
            // A name without an offset that is also a tzdata file.
            ("EST", 1782864000, "2026-06-30 19:00:00 -05:00:00"),
            // A directory is not a TZif file; "America" has no offset -> UTC.
            ("America", 1782864000, "2026-07-01 00:00:00 +00:00:00"),
        ]);
    }

    #[test]
    fn right_zones_apply_leap_second_correction() {
        if !have_zoneinfo() {
            return;
        }
        check(&[
            ("right/UTC", 0, "1970-01-01 00:00:00 +00:00:00"),
            ("right/UTC", 78796799, "1972-06-30 23:59:59 +00:00:00"),
            // `date` shows 23:59:60 here; wall_time cannot represent second 60.
            ("right/UTC", 78796800, "1972-06-30 23:59:59 +00:00:00"),
            ("right/UTC", 78796801, "1972-07-01 00:00:00 +00:00:00"),
            // `date` shows 2016-12-31 23:59:60.
            ("right/UTC", 1483228826, "2016-12-31 23:59:59 +00:00:00"),
            ("right/UTC", 1483228827, "2017-01-01 00:00:00 +00:00:00"),
            ("right/UTC", 1483228828, "2017-01-01 00:00:01 +00:00:00"),
            ("right/UTC", 1782864000, "2026-06-30 23:59:33 +00:00:00"),
            (
                "right/America/New_York",
                1782864000,
                "2026-06-30 19:59:33 -04:00:00",
            ),
        ]);
    }

    #[test]
    fn posix_strings_with_rules() {
        check(&[
            (
                "CET-1CEST,M3.5.0,M10.5.0/3",
                1782864000,
                "2026-07-01 02:00:00 +02:00:00",
            ),
            (
                "CET-1CEST,M3.5.0,M10.5.0/3",
                1767225600,
                "2026-01-01 01:00:00 +01:00:00",
            ),
            (
                "CET-1CEST,M3.5.0,M10.5.0/3",
                1774745999,
                "2026-03-29 01:59:59 +01:00:00",
            ),
            (
                "CET-1CEST,M3.5.0,M10.5.0/3",
                1774746000,
                "2026-03-29 03:00:00 +02:00:00",
            ),
            (
                "CET-1CEST,M3.5.0,M10.5.0/3",
                1792889999,
                "2026-10-25 02:59:59 +02:00:00",
            ),
            (
                "CET-1CEST,M3.5.0,M10.5.0/3",
                1792890000,
                "2026-10-25 02:00:00 +01:00:00",
            ),
            // Southern hemisphere: DST spans the new year.
            (
                "AEST-10AEDT,M10.1.0,M4.1.0/3",
                1768435200,
                "2026-01-15 11:00:00 +11:00:00",
            ),
            (
                "AEST-10AEDT,M10.1.0,M4.1.0/3",
                1782864000,
                "2026-07-01 10:00:00 +10:00:00",
            ),
            (
                "AEST-10AEDT,M10.1.0,M4.1.0/3",
                1767223800,
                "2026-01-01 10:30:00 +11:00:00",
            ),
            (
                "AEST-10AEDT,M10.1.0,M4.1.0/3",
                1775318399,
                "2026-04-05 02:59:59 +11:00:00",
            ),
            (
                "AEST-10AEDT,M10.1.0,M4.1.0/3",
                1775318400,
                "2026-04-05 02:00:00 +10:00:00",
            ),
            (
                "AEST-10AEDT,M10.1.0,M4.1.0/3",
                1791043199,
                "2026-10-04 01:59:59 +10:00:00",
            ),
            (
                "AEST-10AEDT,M10.1.0,M4.1.0/3",
                1791043200,
                "2026-10-04 03:00:00 +11:00:00",
            ),
            // Jn never counts Feb 29: J60 is March 1 in a leap year.
            (
                "EST5EDT,J60,J300",
                1709276399,
                "2024-03-01 01:59:59 -05:00:00",
            ),
            (
                "EST5EDT,J60,J300",
                1709276400,
                "2024-03-01 03:00:00 -04:00:00",
            ),
            (
                "EST5EDT,J60,J300",
                1709208000,
                "2024-02-29 07:00:00 -05:00:00",
            ),
            // n counts Feb 29: day 59 is Feb 29 in a leap year, March 1 otherwise.
            (
                "EST5EDT,59,300",
                1709189999,
                "2024-02-29 01:59:59 -05:00:00",
            ),
            (
                "EST5EDT,59,300",
                1709190000,
                "2024-02-29 03:00:00 -04:00:00",
            ),
            (
                "EST5EDT,59,300",
                1677653999,
                "2023-03-01 01:59:59 -05:00:00",
            ),
            (
                "EST5EDT,59,300",
                1677654000,
                "2023-03-01 03:00:00 -04:00:00",
            ),
            // Extended rule times: negative and beyond 24 hours.
            (
                "EST5EDT,M3.2.0/-1,M11.1.0/26",
                1772942399,
                "2026-03-07 22:59:59 -05:00:00",
            ),
            (
                "EST5EDT,M3.2.0/-1,M11.1.0/26",
                1772942400,
                "2026-03-08 00:00:00 -04:00:00",
            ),
            (
                "EST5EDT,M3.2.0/-1,M11.1.0/26",
                1793599199,
                "2026-11-02 01:59:59 -04:00:00",
            ),
            (
                "EST5EDT,M3.2.0/-1,M11.1.0/26",
                1793599200,
                "2026-11-02 01:00:00 -05:00:00",
            ),
        ]);
    }

    #[test]
    fn posix_strings_without_dst() {
        check(&[
            ("EST5", 1782864000, "2026-06-30 19:00:00 -05:00:00"),
            (":EST5", 1782864000, "2026-06-30 19:00:00 -05:00:00"),
            ("<+0330>-3:30", 1782864000, "2026-07-01 03:30:00 +03:30:00"),
            ("UTC0", 1782864000, "2026-07-01 00:00:00 +00:00:00"),
            ("GMT+5", 1782864000, "2026-06-30 19:00:00 -05:00:00"),
        ]);
    }

    #[test]
    fn glibc_parser_quirks() {
        check(&[
            // Minutes and seconds clamp to 59, hours to 24.
            ("EST5:99:99", 1782864000, "2026-06-30 18:00:01 -05:59:59"),
            // sscanf("%hu") accepts "-5" and wraps it to 65531, clamped to 24.
            ("EST+-5", 1782864000, "2026-06-30 00:00:00 -24:00:00"),
            // Bad DST name: zeroed rules make DST (offset 0) nearly all year.
            ("EST5x", 1782864000, "2026-07-01 00:00:00 +00:00:00"),
            ("EST5x", 1767236400, "2025-12-31 22:00:00 -05:00:00"),
            // Half-parsed Mm.n.d rule (month 0).
            ("EST5EDT,M", 1672808399, "2023-01-03 23:59:59 -05:00:00"),
            ("EST5EDT,M", 1672808400, "2023-01-04 01:00:00 -04:00:00"),
        ]);
    }

    #[test]
    fn dst_name_without_rules_uses_posixrules() {
        if !have_zoneinfo() || !std::path::Path::new("/usr/share/zoneinfo/posixrules").exists() {
            return;
        }
        check(&[
            // EST5EDT is itself a tzdata file.
            ("EST5EDT", 1772953199, "2026-03-08 01:59:59 -05:00:00"),
            ("EST5EDT", 1772953200, "2026-03-08 03:00:00 -04:00:00"),
            ("EST5EDT", -3786825600, "1849-12-31 19:03:58 -04:56:02"),
            // glibc's __tzfile_default shifts the New York transitions.
            ("CET-1CEST", 1772974799, "2026-03-08 13:59:59 +01:00:00"),
            ("CET-1CEST", 1772974800, "2026-03-08 15:00:00 +02:00:00"),
            ("CET-1CEST", 1793519999, "2026-11-01 09:59:59 +02:00:00"),
            ("CET-1CEST", 1793520000, "2026-11-01 09:00:00 +01:00:00"),
            // After 2037 glibc uses posixrules' own footer (US Eastern offsets).
            ("CET-1CEST", 2224713600, "2040-06-30 20:00:00 -04:00:00"),
            ("CET-1CEST", -3771187200, "1850-07-01 01:00:00 +01:00:00"),
            ("XST5XDT,", 1782864000, "2026-06-30 20:00:00 -04:00:00"),
        ]);
    }

    #[test]
    fn unparsable_values_are_utc() {
        check(&[
            ("Mars/Olympus", 1782864000, "2026-07-01 00:00:00 +00:00:00"),
            ("+++", 1782864000, "2026-07-01 00:00:00 +00:00:00"),
            ("ABC", 1782864000, "2026-07-01 00:00:00 +00:00:00"),
            ("<A>5", 1782864000, "2026-07-01 00:00:00 +00:00:00"),
            (":", 1782864000, "2026-07-01 00:00:00 +00:00:00"),
        ]);
        assert_eq!(Zone::from_tz_value("+++"), Zone::utc());
        assert_eq!(Zone::utc().utc_offset_at(0), 0);
    }

    #[test]
    fn corrupt_tzif_data_does_not_panic() {
        if !have_zoneinfo() {
            return;
        }
        let good = std::fs::read(NY).unwrap();
        assert!(parse_tzif(&good).is_some());
        for len in 0..good.len() {
            let _ = parse_tzif(&good[..len]);
        }
        let mut state: u32 = 12345;
        for _ in 0..2000 {
            let mut bytes = good.clone();
            for _ in 0..8 {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12345);
                let i = (state as usize >> 4) % bytes.len();
                bytes[i] = (state >> 24) as u8;
            }
            if let Some(raw) = parse_tzif(&bytes) {
                let zone = Zone {
                    inner: Inner::File(Arc::new(raw.file)),
                };
                for t in [i64::MIN, -1 << 40, 0, 1_782_864_000, 1 << 40, i64::MAX] {
                    let _ = zone.wall_time(t);
                }
            }
        }
        for garbage in ["", "TZif", "TZif2\0\0\0"] {
            assert!(parse_tzif(garbage.as_bytes()).is_none());
        }
    }

    /// A fresh, empty directory under the system temp dir.
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "tz-test-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `Zone::from_tz_value(value)` on another thread, failing after 5 s.
    fn resolve_with_timeout(value: &str) -> Zone {
        let (tx, rx) = std::sync::mpsc::channel();
        let owned = value.to_owned();
        std::thread::spawn(move || {
            let _ = tx.send(Zone::from_tz_value(&owned));
        });
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap_or_else(|_| panic!("Zone::from_tz_value({value:?}) did not return"))
    }

    /// Asserts `zone` behaves like an unreadable file (UTC for these values).
    fn assert_utc_like(zone: &Zone, value: &str) {
        let garbage = Zone::from_tz_value("garbage-no-offset");
        assert_eq!(*zone, garbage, "{value}");
        for t in [-1_000_000_000, 0, 1_782_864_000, 4_000_000_000] {
            assert_eq!(zone.utc_offset_at(t), 0, "{value} @{t}");
        }
    }

    #[test]
    fn fifo_is_not_read() {
        let dir = temp_dir("fifo");
        let fifo = dir.join("zone");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let value = fifo.to_str().unwrap();
        assert_utc_like(&resolve_with_timeout(value), value);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn directories_and_devices_are_unreadable() {
        let dir = temp_dir("dir");
        for value in [dir.to_str().unwrap(), "/dev/zero"] {
            assert_utc_like(&resolve_with_timeout(value), value);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tzif_header(version: u8, counts: [u32; 6]) -> Vec<u8> {
        let mut h = b"TZif".to_vec();
        h.push(version);
        h.extend_from_slice(&[0; 15]);
        for c in counts {
            h.extend_from_slice(&c.to_be_bytes());
        }
        assert_eq!(h.len(), 44);
        h
    }

    #[test]
    fn huge_header_counts_do_not_allocate() {
        // [isutcnt, isstdcnt, leapcnt, timecnt, typecnt, charcnt]
        let v0 = tzif_header(0, [0, 0, 0, 0x7FFF_FFFF, 1, 0]);
        let mut v2 = tzif_header(b'2', [0, 0, 0, 0, 1, 0]);
        v2.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        v2.extend(tzif_header(b'2', [0, 0, 0x7FFF_FFFF, 0x7FFF_FFFF, 1, 0]));
        v2.extend_from_slice(b"\nUTC0\n");
        let mut v2_types = tzif_header(b'2', [0, 0, 0, 0, 1, 0]);
        v2_types.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        v2_types.extend(tzif_header(b'2', [0x7FFF_FFFF, 0, 0, 0, 0x7FFF_FFFF, 0]));
        let mut v0_types = tzif_header(0, [0x7FFF_FFFF, 0x7FFF_FFFF, 0, 0, 0x7FFF_FFFF, 0]);
        v0_types.extend_from_slice(&[0; 64]);

        let dir = temp_dir("huge");
        for (name, bytes) in [
            ("v0", &v0),
            ("v2", &v2),
            ("v2_types", &v2_types),
            ("v0_types", &v0_types),
        ] {
            assert!(parse_tzif(bytes).is_none(), "{name}");
            let path = dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            let value = path.to_str().unwrap();
            assert_utc_like(&resolve_with_timeout(value), value);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn minimal_tzif_files_still_parse() {
        let mut v0 = tzif_header(0, [0, 0, 0, 0, 1, 4]);
        v0.extend_from_slice(&[0, 0, 0x1c, 0x20, 0, 0]);
        v0.extend_from_slice(b"JST\0");
        let raw = parse_tzif(&v0).expect("v0 parses");
        assert_eq!(raw.file.types[0].offset, 7200);
        // One byte short of the data block.
        assert!(parse_tzif(&v0[..v0.len() - 1]).is_none());

        let mut v2 = v0.clone();
        v2[4] = b'2';
        v2.extend(tzif_header(b'2', [0, 0, 0, 0, 1, 4]));
        v2.extend_from_slice(&[0, 0, 0x1c, 0x20, 0, 0]);
        v2.extend_from_slice(b"JST\0");
        v2.extend_from_slice(b"\nJST-2\n");
        let raw = parse_tzif(&v2).expect("v2 parses");
        assert!(raw.file.footer.is_some());
    }

    #[test]
    fn oversized_input_is_rejected() {
        let cap = 1000u64;
        assert_eq!(
            read_capped(std::io::repeat(7).take(cap), cap)
                .unwrap()
                .len(),
            1000
        );
        assert!(read_capped(std::io::repeat(7).take(cap + 1), cap).is_none());
        // An endless reader stops at cap + 1 bytes.
        assert!(read_capped(std::io::repeat(7), cap).is_none());
        assert!(read_capped(std::io::empty(), cap).unwrap().is_empty());

        // A real file just over the real cap, created sparse.
        let dir = temp_dir("big");
        let path = dir.join("zone");
        if let Some(raw) = std::fs::read(NY)
            .ok()
            .and_then(|b| parse_tzif(&b).map(|_| b))
        {
            std::fs::write(&path, &raw).unwrap();
            assert!(read_zone_bytes(&path.clone().into_os_string()).is_some());
        }
        let f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        f.set_len(MAX_TZIF_BYTES + 1).unwrap();
        drop(f);
        assert!(read_zone_bytes(&path.clone().into_os_string()).is_none());
        let value = path.to_str().unwrap();
        assert_utc_like(&resolve_with_timeout(value), value);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn secure_mode_path_guard_matches_glibc() {
        for (value, allowed) in [
            ("/etc/localtime", true),
            ("/usr/share/zoneinfo/Asia/Tokyo", true),
            ("/usr/share/zoneinfoX/y", true),
            ("/usr/share/zoneinfo", true),
            ("/usr/share/zoneinf", false),
            ("/etc/localtime2", false),
            ("/tmp/x", false),
            ("Asia/Tokyo", true),
            ("posixrules", true),
            ("EST5EDT,M3.2.0,M11.1.0", true),
            ("../../etc/shadow", false),
            ("posix/../x", false),
            ("/usr/share/zoneinfo/../../../etc/shadow", false),
            ("..", true),
            ("a/..", true),
        ] {
            assert_eq!(path_allowed_in_secure_mode(value), allowed, "{value}");
        }
    }

    #[test]
    fn symlinked_zone_files_still_load() {
        if !have_zoneinfo() {
            return;
        }
        let dir = temp_dir("link");
        let link = dir.join("zone");
        std::os::unix::fs::symlink(NY, &link).unwrap();
        let zone = resolve_with_timeout(link.to_str().unwrap());
        assert_eq!(zone, Zone::from_tz_value("America/New_York"));
        assert_eq!(zone.utc_offset_at(1_782_864_000), -4 * 3600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extreme_timestamps_do_not_panic() {
        for tz in [
            "EST5EDT,M3.2.0,M11.1.0",
            "AEST-10AEDT,M10.1.0,M4.1.0/3",
            "America/New_York",
        ] {
            let zone = Zone::from_tz_value(tz);
            for t in [i64::MIN, i64::MIN + 1, -1 << 50, 1 << 50, i64::MAX] {
                let _ = zone.wall_time(t);
                let _ = zone.utc_offset_at(t);
            }
        }
    }

    const TOKYO: &str = "/usr/share/zoneinfo/Asia/Tokyo";
    const PARIS: &str = "/usr/share/zoneinfo/Europe/Paris";

    fn file_arc(zone: &Zone) -> &Arc<TzFile> {
        match &zone.inner {
            Inner::File(f) => f,
            Inner::Posix(_) => panic!("not a file-backed zone: {zone:?}"),
        }
    }

    /// Runs `f` on another thread, failing after 5 s.
    fn with_timeout<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap_or_else(|_| panic!("{what} did not return"))
    }

    #[test]
    fn cache_shares_one_arc_across_path_spellings() {
        if !std::path::Path::new(TOKYO).exists() {
            return;
        }
        let dir = temp_dir("spell");
        std::fs::create_dir(dir.join("sub")).unwrap();
        let path = dir.join("tz");
        std::fs::copy(TOKYO, &path).unwrap();
        std::os::unix::fs::symlink(&path, dir.join("link")).unwrap();
        let d = dir.to_str().unwrap();
        let spellings = [
            format!("{d}/tz"),
            format!("/{d}/tz"),
            format!("{d}/./tz"),
            format!(":{d}/tz"),
            format!("{d}//tz"),
            format!("{d}/sub/../tz"),
            format!("{d}/link"),
        ];
        let mut cache = ZoneCache::new(8);
        let first = cache.get(spellings[0].as_bytes());
        assert_eq!(first, Zone::from_tz_bytes(spellings[0].as_bytes()));
        assert_eq!(first.utc_offset_at(1_782_864_000), 9 * 3600);
        for s in &spellings {
            let zone = cache.get(s.as_bytes());
            assert_eq!(zone, first, "{s}");
            assert!(Arc::ptr_eq(file_arc(&zone), file_arc(&first)), "{s}");
        }
        assert_eq!(cache.file_entries(), 1);
        assert_eq!(cache.entries.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_evicts_least_recently_used_entries() {
        let values: Vec<String> = (0..20u8)
            .map(|i| format!("X{}ST-{}", char::from(b'A' + i), i % 12))
            .collect();
        let mut cache = ZoneCache::new(4);
        for v in &values {
            assert_eq!(
                cache.get(v.as_bytes()),
                Zone::from_tz_bytes(v.as_bytes()),
                "{v}"
            );
            assert!(cache.entries.len() <= 4);
        }
        assert_eq!(cache.entries.len(), 4);
        // Keep an early value hot; it must survive further inserts.
        let hot = values[16].as_bytes();
        for v in &values[..8] {
            cache.get(hot);
            assert_eq!(
                cache.get(v.as_bytes()),
                Zone::from_tz_bytes(v.as_bytes()),
                "{v}"
            );
            assert!(cache.entries.len() <= 4);
        }
        assert!(cache.entries.contains_key(&CacheKey::Posix(hot.to_vec())));
        assert!(
            !cache
                .entries
                .contains_key(&CacheKey::Posix(values[19].as_bytes().to_vec()))
        );
        for (i, v) in values.iter().enumerate() {
            // "-N" in a POSIX string means N hours east of UTC.
            let want = 3600 * (i as i32 % 12);
            assert_eq!(cache.get(v.as_bytes()).utc_offset_at(0), want, "{v}");
        }
        // Capacity 0 stores nothing but still resolves.
        let mut none = ZoneCache::new(0);
        assert_eq!(none.get(b"EST5").utc_offset_at(0), -5 * 3600);
        assert!(none.entries.is_empty());
        assert_eq!(none.get(b":"), Zone::utc());
    }

    #[test]
    fn cache_revalidates_zone_files() {
        if !std::path::Path::new(TOKYO).exists() || !std::path::Path::new(PARIS).exists() {
            return;
        }
        let dir = temp_dir("reval");
        let path = dir.join("zone");
        let value = path.to_str().unwrap().as_bytes().to_vec();
        let mut cache = ZoneCache::new(8);

        let missing = cache.get(&value);
        assert_utc_like(&missing, path.to_str().unwrap());
        assert_eq!(cache.get(&value), missing);

        std::fs::copy(TOKYO, &path).unwrap();
        let tokyo = cache.get(&value);
        assert_eq!(tokyo.utc_offset_at(1_782_864_000), 9 * 3600);
        assert_eq!(tokyo, Zone::from_tz_value(TOKYO));
        assert!(Arc::ptr_eq(file_arc(&tokyo), file_arc(&cache.get(&value))));

        // Replace with another zone under a new inode.
        let tmp = dir.join("zone.new");
        std::fs::copy(PARIS, &tmp).unwrap();
        std::fs::rename(&tmp, &path).unwrap();
        let paris = cache.get(&value);
        assert_eq!(paris.utc_offset_at(1_782_864_000), 2 * 3600);
        assert_eq!(paris, Zone::from_tz_value(PARIS));

        // Rewrite in place (same inode): size and timestamps change.
        std::fs::write(&path, std::fs::read(TOKYO).unwrap()).unwrap();
        assert_eq!(cache.get(&value).utc_offset_at(1_782_864_000), 9 * 3600);

        // Removed again: back to the POSIX fall-through.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(cache.get(&value), missing);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_and_bytes_treat_non_regular_files_as_unreadable() {
        let dir = temp_dir("nonreg");
        let fifo = dir.join("fifo");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let values = [
            "/dev/null".to_owned(),
            "/dev/zero".to_owned(),
            fifo.to_str().unwrap().to_owned(),
            dir.to_str().unwrap().to_owned(),
        ];
        for value in values {
            let v = value.clone();
            let (cached, direct) = with_timeout(&value, move || {
                let mut cache = ZoneCache::new(4);
                let first = cache.get(v.as_bytes());
                let again = cache.get(v.as_bytes());
                assert_eq!(first, again);
                (first, Zone::from_tz_bytes(v.as_bytes()))
            });
            assert_utc_like(&cached, &value);
            assert_utc_like(&direct, &value);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fifo_without_writer_is_never_opened_for_reading() {
        let dir = temp_dir("nowriter");
        let fifo = dir.join("fifo");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let path = fifo.into_os_string();
        // The first open carries no O_NONBLOCK: a read-open of a writer-less
        // FIFO would block here. O_PATH returns at once and reports a FIFO.
        let p = path.clone();
        let kind = with_timeout("open_o_path(fifo)", move || {
            let fd = open_o_path(&p).expect("O_PATH open");
            std::fs::File::from(fd).metadata().unwrap().file_type()
        });
        assert!(std::os::unix::fs::FileTypeExt::is_fifo(&kind));
        let p = path.clone();
        let opened = with_timeout("open_zone(fifo)", move || open_zone(&p, PROC_FD_DIR));
        assert!(matches!(opened, Opened::Unusable), "{opened:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_proc_fd_makes_files_unreadable() {
        if !std::path::Path::new(TOKYO).exists() {
            return;
        }
        let tokyo = OsStr::new(TOKYO);
        assert!(matches!(open_zone(tokyo, PROC_FD_DIR), Opened::Bytes(..)));
        let opened = open_zone(tokyo, "/nonexistent-proc/self/fd");
        assert!(matches!(opened, Opened::Unusable), "{opened:?}");
    }

    #[test]
    fn non_utf8_paths_resolve() {
        if !std::path::Path::new(TOKYO).exists() {
            return;
        }
        let dir = temp_dir("bytes");
        let mut raw = dir.into_os_string().into_vec();
        raw.extend_from_slice(b"/caf\xe9");
        let path = OsString::from_vec(raw.clone());
        std::fs::copy(TOKYO, &path).unwrap();
        let zone = Zone::from_tz_bytes(&raw);
        assert_eq!(zone, Zone::from_tz_value(TOKYO));
        let mut colon = b":".to_vec();
        colon.extend_from_slice(&raw);
        assert_eq!(ZoneCache::new(2).get(&colon), zone);
        let _ = std::fs::remove_dir_all(std::path::Path::new(&path).parent().unwrap());
    }

    #[test]
    fn files_over_one_mib_are_unreadable() {
        assert_eq!(MAX_TZIF_BYTES, 1024 * 1024);
        let dir = temp_dir("mib");
        let path = dir.join("zone");
        let mut bytes = std::fs::read(TOKYO).unwrap_or_default();
        bytes.resize(MAX_TZIF_BYTES as usize + 1, 0);
        std::fs::write(&path, &bytes).unwrap();
        let value = path.to_str().unwrap().to_owned();
        assert!(matches!(
            open_zone(path.as_os_str(), PROC_FD_DIR),
            Opened::Rejected(_)
        ));
        let v = value.clone();
        let (direct, cached) = with_timeout(&value, move || {
            let mut cache = ZoneCache::new(4);
            let first = cache.get(v.as_bytes());
            assert_eq!(cache.file_entries(), 1);
            assert_eq!(cache.get(v.as_bytes()), first);
            (Zone::from_tz_bytes(v.as_bytes()), first)
        });
        assert_utc_like(&direct, &value);
        assert_utc_like(&cached, &value);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_matches_uncached_resolution() {
        let mut cache = ZoneCache::new(3);
        let values = [
            "America/New_York",
            ":Europe/Paris",
            "posix/Asia/Tokyo",
            "CET-1CEST",
            "EST5EDT,M3.2.0,M11.1.0",
            "Mars/Olympus",
            "America",
            "right/UTC",
            "EST5\0junk",
        ];
        for _ in 0..3 {
            for v in values {
                assert_eq!(cache.get(v.as_bytes()), Zone::from_tz_value(v), "{v}");
            }
        }
    }

    /// Differential test against the system `date` (glibc). Run with
    /// `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn differential_against_date() {
        use std::io::Write;
        use std::process::{Command, Stdio};

        if !have_zoneinfo() {
            return;
        }
        let zones = [
            "America/New_York",
            "Australia/Sydney",
            "Europe/London",
            ":Europe/Paris",
            "/usr/share/zoneinfo/Asia/Tokyo",
            "posix/Asia/Tokyo",
            "right/UTC",
            "right/America/New_York",
            "Europe/Dublin",
            "Africa/Casablanca",
            "America/Sao_Paulo",
            "Asia/Tehran",
            "Antarctica/Troll",
            "Etc/GMT-3",
            "CET-1CEST,M3.5.0,M10.5.0/3",
            "AEST-10AEDT,M10.1.0,M4.1.0/3",
            "EST5EDT",
            "CET-1CEST",
            "XST5XDT",
            "AAA-10BBB",
            "EST5",
            ":EST5",
            "<+0330>-3:30",
            "EST5EDT,J60/-1,300/26",
            "UTC0DST,59/0,J365/25",
            "ABC-2DEF,M10.1.0,M3.1.0",
            "EST5EDT,M",
            "EST5EDT,M3.2.0/x,M11.1.0",
            "EST5x",
            "EST+-5",
            "Mars/Olympus",
            "UTC",
            "+++",
        ];
        let mut state: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut timestamps: Vec<i64> = (0..200).map(|_| (next() % 4_102_444_800) as i64).collect();
        timestamps.extend((0..20).map(|_| -((next() % 2_208_988_800) as i64)));

        let mut compared = 0usize;
        let mut mismatches = Vec::new();
        for tz in zones {
            let zone = Zone::from_tz_value(tz);
            let mut child = Command::new("date")
                .env("TZ", tz)
                .env("LC_ALL", "C")
                .arg("-f")
                .arg("-")
                .arg("+%F %T %::z")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .expect("spawn date");
            {
                let mut stdin = child.stdin.take().unwrap();
                for t in &timestamps {
                    writeln!(stdin, "@{t}").unwrap();
                }
            }
            let out = child.wait_with_output().expect("run date");
            let text = String::from_utf8(out.stdout).unwrap();
            let lines: Vec<&str> = text.lines().collect();
            assert_eq!(lines.len(), timestamps.len(), "date output for {tz}");
            for (t, want) in timestamps.iter().zip(lines) {
                compared += 1;
                // gnulib prints "-00:00:00" for zones abbreviated "-00".
                let want = want.replace(" -00:00:00", " +00:00:00");
                // An inserted leap second is displayed as :60 by glibc only.
                if want.get(17..19) == Some("60") {
                    continue;
                }
                let got = render(&zone, *t);
                if got != want {
                    mismatches.push(format!("TZ={tz:?} @{t}: got {got}, date {want}"));
                }
            }
        }
        eprintln!(
            "differential: {compared} comparisons, {} mismatches",
            mismatches.len()
        );
        assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
    }
}
