//! A cross-process lease file with stale-holder detection (pid + heartbeat TTL).
//!
//! This is not [`crate::FileLock`]. A `FileLock` is an OS advisory lock the kernel drops when its
//! holder dies. A [`Lease`] is a small record in a plain file: who holds it (pid, a unique token, a
//! label), when it was taken and when the holder last heartbeated, and the TTL the holder promises
//! to heartbeat within. It outlives the holder's file handles, can be observed by anyone without
//! locking, and is taken over when the holder is gone. Use it to elect one process out of many (a
//! daemon autostarter, a single scheduler) where the winner keeps running work for a long time.
//!
//! * [`Lease::try_acquire`]: create the record exclusively ([`ReplaceMode::NoReplace`], so a
//!   reader never sees a half-written record and exactly one racing creator wins), or observe the
//!   current holder ([`Acquire::Held`]). A stale holder is taken over.
//! * [`Lease::renew`]: heartbeat. Rewrites the record atomically with a new heartbeat time, after
//!   checking, under the takeover guard, that the record is still exactly the one this lease last
//!   wrote. Call it at least every `ttl / 3`.
//! * [`Lease::release`] (also on drop): removes the record if, under the takeover guard, it is
//!   still exactly this lease's last-written record.
//! * [`Lease::peek`]: read the record and judge it ([`LeaseInfo::staleness`]) without taking it.
//!
//! # Stale-holder detection
//!
//! A record is stale when either
//!
//! 1. the holder's pid is **confirmed dead** (the crate's tri-state process probe: `kill(pid, 0)`
//!    `ESRCH` on Unix; `OpenProcess` / `GetExitCodeProcess`, with a complete process snapshot as the
//!    proof of absence, on Windows). A probe that fails or is denied is `Unknown`, never `Dead`; or
//! 2. the holder stopped heartbeating: `now - heartbeat > ttl` (the holder's own `ttl`, written in
//!    the record), or the heartbeat is more than one `ttl` in the future (a clock that jumped is
//!    not trusted to hold a lease forever).
//!
//! A live pid with a fresh heartbeat is never taken. A live pid that stopped heartbeating (a hung
//! process, or a recycled pid) is taken after its TTL.
//!
//! # Takeover and the guard
//!
//! Every record mutation (initial creation, takeover, [`Lease::renew`],
//! [`Lease::release`]) first takes a short-lived **guard**: an exclusive OS lock
//! (`File::try_lock`) on the stable file `<lease path>.takeover`, held through the whole
//! validate/mutate step. Only the lock's holder may act, so two takers cannot both judge, remove
//! and create, and a renewal cannot slip in between a judgement and the removal. The kernel grants
//! the lock to exactly one holder and drops it when the holder exits or is killed, so a live
//! guard can never be stolen by age or TTL, and a dead holder's guard frees itself.
//!
//! The guard file is **never unlinked or renamed**: deleting it would let one process lock the
//! old inode while another creates and locks a new file at the same path, which is the race this
//! guard exists to close. Its content (pid, creation time, token, under a `rightkit-lease-guard
//! v2` header) is evidence for diagnosis only; the OS lock decides.
//!
//! A taker takes the guard, judges the record, re-reads it and compares the **full text**
//! (heartbeat included, not just the token) with what it judged. Only an identical, stale record is
//! removed, and the normal exclusive create follows while the guard is still held. Anything else
//! (the record changed, vanished or was replaced) removes nothing; the call goes round again. If
//! the guard is held by someone else, the taker does not wait: that round is lost (after four
//! rounds the call reports `WouldBlock`, and calling again is safe). `renew` reports the same
//! retryable `WouldBlock` and writes nothing; `release` leaves the file in place.
//!
//! Mixed-version note: the previous protocol held the guard by the file's mere existence (no
//! header, no OS lock) and reclaimed it by renaming a stale-looking file aside. This protocol
//! fails closed for live or unknown legacy owners regardless of age. Only a positively dead
//! legacy owner permits migration; unknown records require offline cleanup with legacy writers
//! stopped. This protocol never renames guards aside; a legacy binary can still rename a
//! guard file this code has locked, so mixed binaries on one lease path do not get the full
//! guarantee until every writer is upgraded.
//!
//! # Limits
//!
//! * Local machine only. The pid probe and the wall clock are meaningful on one host and one pid
//!   namespace (a container or a sandbox sees different pids); do not put a lease file on a network
//!   share used from several machines or share it across pid namespaces.
//! * Time is the wall clock (Unix milliseconds) because processes must agree on it. A holder
//!   stalled for longer than its TTL between two heartbeats can lose the lease without knowing
//!   until its next `renew`; there is no fencing token beyond [`Lease::token`]. Keep the TTL several
//!   times longer than the longest possible stall.
//! * A writer that bypasses this module (edits or replaces the record directly) is not covered by
//!   the guard.
//! * A record that cannot be parsed, including one written by a future version of this format, is
//!   judged by the file's modification time against the caller's TTL alone (never by pid), and
//!   reported with pid 0 and a zero TTL.
//! * Nothing here sleeps or polls: contention is observed, never waited on.
//!
//! Not compiled or run when written.

use crate::atomic::{atomic_write_with_options, AtomicWriteOptions, ReplaceMode};
use crate::error::{FsError, Result};
use crate::{process_liveness, ProcessLiveness};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const HEADER: &str = "rightkit-lease v1";
/// Attempts of create / judge / take down before the call reports contention.
const ATTEMPTS: u32 = 4;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// What a caller asks of a lease.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct LeaseOptions {
    /// The heartbeat promise: the lease is stale when the last heartbeat is older than this.
    pub ttl: Duration,
    /// Free text stored in the record (a role or program name). Line breaks are replaced by spaces.
    pub label: String,
}

impl LeaseOptions {
    /// A lease with this TTL and no label.
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            label: String::new(),
        }
    }

    /// Set the label stored in the record.
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }
}

/// Why a record is, or is not, stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Staleness {
    /// The holder heartbeated within its TTL (or its liveness is unknown and the TTL has not run
    /// out).
    Fresh,
    /// The holder's pid is confirmed dead.
    HolderDead,
    /// The last heartbeat is older than the TTL (or implausibly far in the future).
    Expired,
}

impl Staleness {
    /// True for [`Staleness::HolderDead`] and [`Staleness::Expired`].
    pub fn is_stale(self) -> bool {
        self != Staleness::Fresh
    }
}

/// The content of a lease record.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LeaseInfo {
    /// The holder's process id (0 when the record could not be parsed).
    pub pid: u32,
    /// The unique token of the holder's [`Lease`] (empty when the record could not be parsed).
    pub token: String,
    /// The holder's label.
    pub label: String,
    /// The TTL the holder promised to heartbeat within.
    pub ttl: Duration,
    /// When the lease was taken, Unix milliseconds.
    pub acquired_ms: u64,
    /// The last heartbeat, Unix milliseconds.
    pub heartbeat_ms: u64,
}

impl LeaseInfo {
    /// Judge this record now.
    pub fn staleness(&self) -> Staleness {
        self.staleness_at(now_ms())
    }

    fn staleness_at(&self, now_ms: u64) -> Staleness {
        if self.pid != 0 && process_liveness(self.pid) == ProcessLiveness::Dead {
            return Staleness::HolderDead;
        }
        let ttl_ms = millis(self.ttl);
        if self.heartbeat_ms > now_ms.saturating_add(ttl_ms) {
            return Staleness::Expired;
        }
        if now_ms.saturating_sub(self.heartbeat_ms) > ttl_ms {
            return Staleness::Expired;
        }
        Staleness::Fresh
    }

    fn encode(&self) -> String {
        format!(
            "{HEADER}\npid={}\ntoken={}\nlabel={}\nttl_ms={}\nacquired_ms={}\nheartbeat_ms={}\n",
            self.pid,
            self.token,
            self.label,
            millis(self.ttl),
            self.acquired_ms,
            self.heartbeat_ms
        )
    }

    fn parse(text: &str) -> Option<LeaseInfo> {
        let mut lines = text.lines();
        if lines.next()? != HEADER {
            return None;
        }
        let (mut pid, mut token, mut label) = (None, None, String::new());
        let (mut ttl, mut acquired, mut heartbeat) = (None, None, None);
        for line in lines {
            let (key, value) = line.split_once('=')?;
            match key {
                "pid" => pid = value.parse::<u32>().ok(),
                "token" => token = Some(value.to_string()),
                "label" => label = value.to_string(),
                "ttl_ms" => ttl = value.parse::<u64>().ok(),
                "acquired_ms" => acquired = value.parse::<u64>().ok(),
                "heartbeat_ms" => heartbeat = value.parse::<u64>().ok(),
                _ => {}
            }
        }
        Some(LeaseInfo {
            pid: pid?,
            token: token.filter(|t| !t.is_empty())?,
            label,
            ttl: Duration::from_millis(ttl?),
            acquired_ms: acquired?,
            heartbeat_ms: heartbeat?,
        })
    }
}

/// The outcome of [`Lease::try_acquire`].
#[derive(Debug)]
#[must_use]
pub enum Acquire {
    /// This caller now holds the lease.
    Acquired(Lease),
    /// Another live holder has it; this is its record.
    Held(LeaseInfo),
}

impl Acquire {
    /// The lease, if this caller got it.
    pub fn acquired(self) -> Option<Lease> {
        match self {
            Acquire::Acquired(lease) => Some(lease),
            Acquire::Held(_) => None,
        }
    }
}

/// A held lease. Dropping it releases the record if it is still this lease's.
#[derive(Debug)]
pub struct Lease {
    path: PathBuf,
    info: LeaseInfo,
    held: bool,
}

impl Lease {
    /// Take the lease at `path`, or observe who holds it.
    ///
    /// Creates the record exclusively (and the missing parent directories). When a record exists
    /// it is judged: a live, fresh holder gives [`Acquire::Held`]; a stale one is taken over (see
    /// the module docs) and the create is tried again. Contention that keeps changing under the
    /// call (four rounds) is reported as an `Io` error of kind `WouldBlock`; calling again is safe.
    pub fn try_acquire(path: &Path, options: &LeaseOptions) -> Result<Acquire> {
        Self::try_acquire_at(path, options, now_ms())
    }

    fn try_acquire_at(path: &Path, options: &LeaseOptions, now: u64) -> Result<Acquire> {
        let info = LeaseInfo {
            pid: std::process::id(),
            token: new_token(),
            label: options.label.replace(['\r', '\n'], " "),
            ttl: options.ttl,
            acquired_ms: now,
            heartbeat_ms: now,
        };
        for _ in 0..ATTEMPTS {
            // Even an absent lease may be inside a legacy mutation or a v2 takeover's
            // remove/create interval. Initial creation must obey the same exclusion.
            let Some(guard) = Guard::acquire(path, options.ttl, now)? else {
                continue;
            };
            match create_exclusive(path, &info.encode()) {
                Ok(()) => {
                    return Ok(Acquire::Acquired(Lease {
                        path: path.to_path_buf(),
                        info,
                        held: true,
                    }))
                }
                Err(e) if e.kind() == Some(io::ErrorKind::AlreadyExists) => {}
                Err(e) => return Err(e),
            }
            match judge(path, options.ttl, now)? {
                Judged::Gone => {}
                Judged::Live(holder) => return Ok(Acquire::Held(holder)),
                Judged::Stale { text } => {
                    if take_down(path, &text, &info.encode(), &guard)? == Takedown::Created {
                        return Ok(Acquire::Acquired(Lease {
                            path: path.to_path_buf(),
                            info,
                            held: true,
                        }));
                    }
                }
            }
        }
        Err(FsError::io(
            path,
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "the lease record kept changing while it was being acquired",
            ),
        ))
    }

    /// Read the record at `path` without taking it. `None` when there is none. An unparseable
    /// record is returned with pid 0 and an empty token.
    pub fn peek(path: &Path) -> Result<Option<LeaseInfo>> {
        match read_record(path)? {
            None => Ok(None),
            Some(Record::Good(info)) => Ok(Some(info)),
            Some(Record::Unreadable { mtime_ms, .. }) => Ok(Some(unreadable_info(mtime_ms))),
        }
    }

    /// Heartbeat: rewrite the record with the current time.
    ///
    /// Fails, and changes nothing, when the record is gone (`Io`, `NotFound`) or carries another
    /// token ([`FsError::Locked`], with the new holder described): the lease was lost and the
    /// caller must stop the work it protects.
    pub fn renew(&mut self) -> Result<()> {
        self.renew_at(now_ms())
    }

    fn renew_at(&mut self, now: u64) -> Result<()> {
        let Some(_guard) = Guard::acquire(&self.path, self.info.ttl, now)? else {
            return Err(FsError::io(
                &self.path,
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "the lease is being changed by another process; renew again",
                ),
            ));
        };
        match read_record(&self.path)? {
            None => {
                self.held = false;
                Err(FsError::io(
                    &self.path,
                    io::Error::new(io::ErrorKind::NotFound, "the lease record is gone"),
                ))
            }
            Some(Record::Good(current)) if current.encode() == self.info.encode() => {
                let mut next = self.info.clone();
                next.heartbeat_ms = now.max(current.heartbeat_ms);
                write_replace(&self.path, &next.encode())?;
                self.info = next;
                Ok(())
            }
            Some(other) => {
                self.held = false;
                let who = match other {
                    Record::Good(info) => {
                        format!("pid {} ({})", info.pid, info.label)
                    }
                    Record::Unreadable { .. } => "an unreadable record".to_string(),
                };
                Err(FsError::Locked {
                    path: self.path.clone(),
                    owner: who,
                })
            }
        }
    }

    /// Give the lease up: remove the record if it is still exactly this lease's last-written
    /// record. A record that was taken over, changed or is already gone is left alone and is not
    /// an error; so is a record left in place because another process holds the takeover guard.
    pub fn release(mut self) -> Result<()> {
        self.release_inner()
    }

    fn release_inner(&mut self) -> Result<()> {
        if !self.held {
            return Ok(());
        }
        self.held = false;
        let Some(_guard) = Guard::acquire(&self.path, self.info.ttl, now_ms())? else {
            // Someone is changing the record: leave it rather than race them.
            return Ok(());
        };
        match read_record(&self.path)? {
            Some(Record::Good(current)) if current.encode() == self.info.encode() => {
                match fs::remove_file(&self.path) {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(FsError::io(&self.path, e)),
                }
            }
            _ => Ok(()),
        }
    }

    /// The record this lease last wrote.
    pub fn info(&self) -> &LeaseInfo {
        &self.info
    }

    /// This lease's unique token (the identity checked by `renew` and `release`).
    pub fn token(&self) -> &str {
        &self.info.token
    }

    /// The lease file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let _ = self.release_inner();
    }
}

enum Record {
    Good(LeaseInfo),
    Unreadable { text: String, mtime_ms: u64 },
}

enum Judged {
    Gone,
    Live(LeaseInfo),
    Stale { text: String },
}

fn read_record(path: &Path) -> Result<Option<Record>> {
    let text = match crate::replaced::read_to_string_replaced(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        // Not UTF-8: unreadable as a record, but still a file to judge by its age.
        Err(e) if e.kind() == io::ErrorKind::InvalidData => fs::read(path)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .map_err(|e| FsError::io(path, e))?,
        Err(e) => return Err(FsError::io(path, e)),
    };
    Ok(Some(match LeaseInfo::parse(&text) {
        Some(info) => Record::Good(info),
        None => {
            let mtime_ms = fs::metadata(path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            Record::Unreadable { text, mtime_ms }
        }
    }))
}

fn unreadable_info(mtime_ms: u64) -> LeaseInfo {
    LeaseInfo {
        pid: 0,
        token: String::new(),
        label: String::new(),
        ttl: Duration::ZERO,
        acquired_ms: mtime_ms,
        heartbeat_ms: mtime_ms,
    }
}

fn judge(path: &Path, ttl: Duration, now: u64) -> Result<Judged> {
    Ok(match read_record(path)? {
        None => Judged::Gone,
        Some(Record::Good(info)) => {
            if info.staleness_at(now).is_stale() {
                Judged::Stale {
                    text: info.encode(),
                }
            } else {
                Judged::Live(info)
            }
        }
        Some(Record::Unreadable { text, mtime_ms }) => {
            let mut info = unreadable_info(mtime_ms);
            info.ttl = ttl;
            if info.staleness_at(now).is_stale() {
                Judged::Stale { text }
            } else {
                Judged::Live(info)
            }
        }
    })
}

fn record_text(record: &Record) -> String {
    match record {
        Record::Good(info) => info.encode(),
        Record::Unreadable { text, .. } => text.clone(),
    }
}

/// How a takedown ended.
#[derive(Debug, PartialEq, Eq)]
enum Takedown {
    /// The judged record was removed (or was already gone) and the new record was created.
    Created,
    /// Nothing was removed: the guard is held elsewhere, the record is not the one judged, or
    /// another creator got there first.
    Declined,
}

/// Replace the record judged stale with `new_text`, under the takeover guard. The record is removed
/// only if it is still exactly the judged text (heartbeat included); a record that differs in any
/// way is never removed. The exclusive create runs while the guard is held.
fn take_down(path: &Path, judged_text: &str, new_text: &str, _guard: &Guard) -> Result<Takedown> {
    match read_record(path)? {
        None => {}
        Some(current) if record_text(&current) == judged_text => match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(FsError::io(path, e)),
        },
        Some(_) => return Ok(Takedown::Declined),
    }
    match create_exclusive(path, new_text) {
        Ok(()) => Ok(Takedown::Created),
        Err(e) if e.kind() == Some(io::ErrorKind::AlreadyExists) => Ok(Takedown::Declined),
        Err(e) => Err(e),
    }
}

/// First line of every guard record this protocol writes; a guard file without it is either
/// empty or a legacy (existence-held) guard.
const GUARD_HEADER: &str = "rightkit-lease-guard v2";

/// An exclusive OS lock on the stable file `<lease>.takeover`, held through one whole
/// validate/mutate step of a takeover, renew or release. The file is never unlinked or renamed
/// (see the module docs); dropping the guard only unlocks.
struct Guard {
    file: fs::File,
}

impl Guard {
    fn path_for(lease: &Path) -> PathBuf {
        let mut name = lease.as_os_str().to_os_string();
        name.push(".takeover");
        PathBuf::from(name)
    }

    /// Take the guard, or `None` when another holder has the OS lock (however long it has held
    /// it: a live exclusion is never stolen by age) or when a live/unknown legacy guard is present
    /// (mixed protocol: fail closed). A killed holder's lock is dropped by the kernel, so the
    /// next take simply succeeds.
    fn acquire(lease: &Path, _ttl: Duration, now: u64) -> Result<Option<Guard>> {
        let path = Self::path_for(lease);
        if let Ok(meta) = fs::symlink_metadata(&path) {
            if meta.file_type().is_symlink() {
                return Err(FsError::Symlink(path));
            }
        }
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent).map_err(|e| FsError::io(parent, e))?;
        }
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let (mut file, newly_created) = match options.create_new(true).open(&path) {
            Ok(file) => (file, true),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                options.create(false).create_new(false);
                (
                    options.open(&path).map_err(|e| FsError::io(&path, e))?,
                    false,
                )
            }
            // A symlink swapped in after the check above: O_NOFOLLOW reports ELOOP on Unix.
            Err(e) => return Err(FsError::io(&path, e)),
        };
        match file.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Ok(None),
            Err(fs::TryLockError::Error(e)) => return Err(FsError::io(&path, e)),
        }
        // A pre-existing headerless file is an existence-held legacy guard, including
        // an empty/partially written record. Only positively dead owners may migrate.
        // Age, clock skew, malformed records and unknown liveness grant no exclusion.
        let existing = {
            use std::io::{Read, Seek, SeekFrom};
            let mut text = String::new();
            file.seek(SeekFrom::Start(0))
                .map_err(|e| FsError::io(&path, e))?;
            Read::take(&mut file, 4097)
                .read_to_string(&mut text)
                .map_err(|e| FsError::io(&path, e))?;
            text
        };
        if !newly_created
            && !existing.starts_with(&format!("{GUARD_HEADER}\n"))
            && !legacy_guard_owner_is_dead(&existing)
        {
            let _ = file.unlock();
            return Ok(None);
        }
        {
            use std::io::{Seek, SeekFrom};
            let text = format!(
                "{GUARD_HEADER}\npid={}\ncreated_ms={now}\ntoken={}\n",
                std::process::id(),
                new_token()
            );
            let written = (|| -> io::Result<()> {
                file.seek(SeekFrom::Start(0))?;
                file.write_all(text.as_bytes())?;
                file.set_len(text.len() as u64)
            })();
            if let Err(e) = written {
                let _ = file.unlock();
                return Err(FsError::io(&path, e));
            }
        }
        Ok(Some(Guard { file }))
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        // Explicit unlock (flock descriptors are inherited across fork, so release here rather
        // than waiting for the descriptor to close). The file itself stays: never unlink a lock
        // file another process may be about to open.
        let _ = self.file.unlock();
    }
}

/// Migration is permitted only with one unambiguous, valid PID positively known dead.
/// Unknown/empty records require coordinated offline removal after all legacy writers stop.
fn legacy_guard_owner_is_dead(text: &str) -> bool {
    if text.len() > 4096 {
        return false;
    }
    let mut pids = text.lines().filter_map(|line| line.strip_prefix("pid="));
    let Some(pid) = pids.next().and_then(|value| value.parse::<u32>().ok()) else {
        return false;
    };
    pids.next().is_none() && pid != 0 && process_liveness(pid) == ProcessLiveness::Dead
}

fn create_exclusive(path: &Path, text: &str) -> Result<()> {
    atomic_write_with_options(
        path,
        AtomicWriteOptions {
            replace: ReplaceMode::NoReplace,
            create_parents: true,
            durable: false,
            ..AtomicWriteOptions::default()
        },
        |f| f.write_all(text.as_bytes()),
    )
}

fn write_replace(path: &Path, text: &str) -> Result<()> {
    atomic_write_with_options(
        path,
        AtomicWriteOptions {
            durable: false,
            retry_sharing_violation: Some(crate::atomic::RetryPolicy::new(
                5,
                Duration::from_millis(10),
            )),
            ..AtomicWriteOptions::default()
        },
        |f| f.write_all(text.as_bytes()),
    )
}

fn new_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{}-{:x}-{}",
        std::process::id(),
        nanos,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_secs(10);
    const T0: u64 = 1_000_000;

    fn opts() -> LeaseOptions {
        LeaseOptions::new(TTL).label("test")
    }

    fn lease_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("sub").join("daemon.lease")
    }

    fn acquire_at(path: &Path, now: u64) -> Acquire {
        Lease::try_acquire_at(path, &opts(), now).unwrap()
    }

    fn dead_pid() -> u32 {
        let exe = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(exe)
            .arg("--list")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    fn write_record(path: &Path, pid: u32, token: &str, heartbeat_ms: u64) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let info = LeaseInfo {
            pid,
            token: token.to_string(),
            label: "other".into(),
            ttl: TTL,
            acquired_ms: heartbeat_ms,
            heartbeat_ms,
        };
        fs::write(path, info.encode()).unwrap();
    }

    #[test]
    fn acquire_creates_the_record_and_peek_reads_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        let lease = acquire_at(&path, T0).acquired().expect("acquired");
        let seen = Lease::peek(&path).unwrap().unwrap();
        assert_eq!(seen.pid, std::process::id());
        assert_eq!(seen.token, lease.token());
        assert_eq!(seen.label, "test");
        assert_eq!(seen.ttl, TTL);
        assert_eq!(seen.heartbeat_ms, T0);
        assert_eq!(seen.staleness_at(T0 + 1), Staleness::Fresh);
        assert_eq!(Lease::peek(&dir.path().join("none")).unwrap(), None);
    }

    #[test]
    fn a_second_acquirer_observes_the_live_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        let first = acquire_at(&path, T0).acquired().unwrap();
        match acquire_at(&path, T0 + 5_000) {
            Acquire::Held(holder) => {
                assert_eq!(holder.token, first.token());
                assert_eq!(holder.pid, std::process::id());
            }
            Acquire::Acquired(_) => panic!("a fresh lease must not be taken"),
        }
        // Only the lease & permanent coordination inode remain; no loser temp files.
        let mut names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["daemon.lease", "daemon.lease.takeover"]);
        assert_eq!(Lease::peek(&path).unwrap().unwrap().token, first.token());
    }

    #[test]
    fn a_holder_past_its_ttl_is_taken_over_and_learns_it_on_renew() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        let mut old = acquire_at(&path, T0).acquired().unwrap();
        // Within the TTL: still held. Past it: the pid is alive but silent, so it is taken.
        assert!(matches!(acquire_at(&path, T0 + 10_000), Acquire::Held(_)));
        let new = acquire_at(&path, T0 + 10_001)
            .acquired()
            .expect("taken over");
        assert_ne!(new.token(), old.token());
        assert_eq!(Lease::peek(&path).unwrap().unwrap().token, new.token());

        let err = old.renew_at(T0 + 10_002).unwrap_err();
        assert!(matches!(err, FsError::Locked { .. }), "{err:?}");
        // The displaced lease must not remove the new holder's record, on release or on drop.
        old.release().unwrap();
        assert_eq!(Lease::peek(&path).unwrap().unwrap().token, new.token());
    }

    #[test]
    fn a_dead_holder_is_taken_over_even_with_a_fresh_heartbeat() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        write_record(&path, dead_pid(), "ghost", T0);
        let seen = Lease::peek(&path).unwrap().unwrap();
        assert_eq!(seen.staleness_at(T0 + 1), Staleness::HolderDead);
        let lease = acquire_at(&path, T0 + 1).acquired().expect("taken over");
        assert_eq!(Lease::peek(&path).unwrap().unwrap().token, lease.token());
        assert_eq!(Lease::peek(&path).unwrap().unwrap().pid, std::process::id());
    }

    #[test]
    fn a_live_holder_with_a_fresh_heartbeat_is_never_taken() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        write_record(&path, std::process::id(), "mine-elsewhere", T0);
        assert!(matches!(acquire_at(&path, T0 + 9_999), Acquire::Held(_)));
    }

    #[test]
    fn a_heartbeat_far_in_the_future_is_not_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        write_record(&path, std::process::id(), "skewed", T0 + 3_600_000);
        let seen = Lease::peek(&path).unwrap().unwrap();
        assert_eq!(seen.staleness_at(T0), Staleness::Expired);
        assert!(acquire_at(&path, T0).acquired().is_some());
    }

    #[test]
    fn renew_moves_the_heartbeat_and_keeps_the_lease_alive() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        let mut lease = acquire_at(&path, T0).acquired().unwrap();
        lease.renew_at(T0 + 8_000).unwrap();
        assert_eq!(
            Lease::peek(&path).unwrap().unwrap().heartbeat_ms,
            T0 + 8_000
        );
        assert_eq!(lease.info().heartbeat_ms, T0 + 8_000);
        // 15 s after the first heartbeat but 7 s after the renewed one: still held.
        assert!(matches!(acquire_at(&path, T0 + 15_000), Acquire::Held(_)));
        // The real clock path works too.
        lease.renew().unwrap();
    }

    #[test]
    fn renew_after_the_record_vanished_reports_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        let mut lease = acquire_at(&path, T0).acquired().unwrap();
        fs::remove_file(&path).unwrap();
        assert!(lease.renew_at(T0 + 1).unwrap_err().is_not_found());
    }

    #[test]
    fn release_removes_the_record_and_allows_a_new_acquire() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        let lease = acquire_at(&path, T0).acquired().unwrap();
        lease.release().unwrap();
        assert!(!path.exists());
        assert!(acquire_at(&path, T0 + 1).acquired().is_some());
    }

    #[test]
    fn dropping_a_lease_releases_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        drop(acquire_at(&path, T0).acquired().unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn an_unparseable_record_is_judged_by_its_age() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"garbage").unwrap();
        let mtime = Lease::peek(&path).unwrap().unwrap().heartbeat_ms;
        assert_eq!(Lease::peek(&path).unwrap().unwrap().pid, 0);
        // Fresh by its modification time: observed as held.
        assert!(matches!(acquire_at(&path, mtime + 1_000), Acquire::Held(_)));
        // Older than the caller's TTL: taken.
        assert!(acquire_at(&path, mtime + 10_001).acquired().is_some());
    }

    #[test]
    fn encode_and_parse_round_trip_and_the_label_cannot_break_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        let lease = Lease::try_acquire_at(&path, &LeaseOptions::new(TTL).label("a\nb\r\nc"), T0)
            .unwrap()
            .acquired()
            .unwrap();
        let seen = Lease::peek(&path).unwrap().unwrap();
        assert_eq!(seen.label, "a b  c");
        assert_eq!(&seen, lease.info());
        assert_eq!(LeaseInfo::parse("rightkit-lease v1\npid=1\n"), None);
        assert_eq!(LeaseInfo::parse("other\npid=1\n"), None);
    }

    #[test]
    fn real_clock_acquire_contention_and_release() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        let first = Lease::try_acquire(&path, &opts())
            .unwrap()
            .acquired()
            .unwrap();
        assert!(Lease::try_acquire(&path, &opts())
            .unwrap()
            .acquired()
            .is_none());
        assert!(!Lease::peek(&path).unwrap().unwrap().staleness().is_stale());
        first.release().unwrap();
        assert!(Lease::try_acquire(&path, &opts())
            .unwrap()
            .acquired()
            .is_some());
    }

    #[test]
    fn racing_threads_elect_exactly_one_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || Lease::try_acquire(&path, &opts()))
            })
            .collect();
        // Retain every returned lease until all competitors finish, so releasing a
        // winner cannot admit another winner while the race is still being observed.
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let mut winners = Vec::new();
        for result in &results {
            match result {
                Ok(Acquire::Acquired(lease)) => winners.push(lease.token().to_string()),
                Ok(Acquire::Held(_)) => {}
                Err(e) => assert_eq!(e.kind(), Some(io::ErrorKind::WouldBlock), "{e:?}"),
            }
        }
        assert_eq!(winners.len(), 1, "{results:?}");
        assert_eq!(Lease::peek(&path).unwrap().unwrap().token, winners[0]);
    }

    #[test]
    fn racing_takers_of_a_stale_record_elect_exactly_one_and_delete_no_live_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        write_record(&path, dead_pid(), "ghost", now_ms());
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || Lease::try_acquire(&path, &opts()))
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let mut winners = Vec::new();
        for result in &results {
            match result {
                Ok(Acquire::Acquired(lease)) => winners.push(lease.token().to_string()),
                Ok(Acquire::Held(_)) => {}
                Err(e) => assert_eq!(e.kind(), Some(io::ErrorKind::WouldBlock), "{e:?}"),
            }
        }
        assert_eq!(winners.len(), 1, "{results:?}");
        assert_eq!(Lease::peek(&path).unwrap().unwrap().token, winners[0]);
        // The guard file is never unlinked (a new inode at the same path could be locked while
        // another holder still locks the old one); only the OS lock is released.
        assert!(Guard::path_for(&path).exists(), "the guard file stays");
        assert!(
            Guard::acquire(&path, TTL, now_ms()).unwrap().is_some(),
            "the guard lock is released"
        );
    }

    #[test]
    fn a_taker_declines_when_the_record_is_not_the_one_judged() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        write_record(&path, std::process::id(), "live", T0);
        let judged = Lease::peek(&path).unwrap().unwrap();
        let guard = Guard::acquire(&path, TTL, T0 + 5_001).unwrap().unwrap();
        // Simulate a changed record to exercise the defensive identity check.
        write_record(&path, std::process::id(), "live", T0 + 5_000);
        assert_eq!(
            take_down(&path, &judged.encode(), "new\n", &guard).unwrap(),
            Takedown::Declined
        );
        assert_eq!(
            Lease::peek(&path).unwrap().unwrap().heartbeat_ms,
            T0 + 5_000
        );
        // An identical record is replaced.
        let current = Lease::peek(&path).unwrap().unwrap().encode();
        assert_eq!(
            take_down(&path, &current, "new\n", &guard).unwrap(),
            Takedown::Created
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
    }

    #[test]
    fn a_held_guard_stops_a_takeover_and_removes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        write_record(&path, dead_pid(), "ghost", T0);
        let before = fs::read(&path).unwrap();
        let guard = Guard::acquire(&path, TTL, T0).unwrap().expect("guard");
        let err = Lease::try_acquire_at(&path, &opts(), T0 + 1).unwrap_err();
        assert_eq!(err.kind(), Some(io::ErrorKind::WouldBlock));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(Guard::acquire(&path, TTL, T0 + 1).unwrap().is_none());
        drop(guard);
        assert!(acquire_at(&path, T0 + 1).acquired().is_some());
    }

    #[test]
    fn only_a_positively_dead_legacy_guard_can_migrate() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Legacy (headerless) guard of a dead pid: overwritten under the OS lock, never renamed.
        fs::write(
            Guard::path_for(&path),
            format!("pid={}\ncreated_ms={T0}\ntoken=x\n", dead_pid()),
        )
        .unwrap();
        let guard = Guard::acquire(&path, TTL, T0 + 1).unwrap().expect("guard");
        drop(guard);
        assert!(fs::read_to_string(Guard::path_for(&path))
            .unwrap()
            .starts_with(GUARD_HEADER));
        // A paused live legacy owner remains exclusive even far beyond TTL.
        fs::write(
            Guard::path_for(&path),
            format!("pid={}\ncreated_ms={T0}\ntoken=x\n", std::process::id()),
        )
        .unwrap();
        assert!(Guard::acquire(&path, TTL, T0 + 10_001).unwrap().is_none());
        // Fresh legacy guard of a live pid: a mixed-protocol holder may be mid-operation, so
        // this protocol declines instead of interleaving with it.
        fs::write(
            Guard::path_for(&path),
            format!("pid={}\ncreated_ms={T0}\ntoken=x\n", std::process::id()),
        )
        .unwrap();
        assert!(Guard::acquire(&path, TTL, T0 + 1).unwrap().is_none());
        assert_eq!(
            fs::read_to_string(Guard::path_for(&path)).unwrap(),
            format!("pid={}\ncreated_ms={T0}\ntoken=x\n", std::process::id()),
            "the legacy guard is left exactly as it was"
        );
    }

    #[test]
    fn paused_legacy_holder_resumes_without_v2_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        write_record(&path, dead_pid(), "old-lease", T0);
        let before = fs::read(&path).unwrap();
        let guard_path = Guard::path_for(&path);
        let record = format!(
            "pid={}\ncreated_ms={T0}\ntoken=legacy\n",
            std::process::id()
        );
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            fs::write(&guard_path, &record).unwrap();
            ready_tx.send(()).unwrap();
            resume_rx.recv().unwrap();
            assert_eq!(fs::read_to_string(&guard_path).unwrap(), record);
            fs::remove_file(guard_path).unwrap();
        });
        ready_rx.recv().unwrap();
        assert!(Lease::try_acquire_at(&path, &opts(), T0 + 3_600_000).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        resume_tx.send(()).unwrap();
        holder.join().unwrap();
        assert!(acquire_at(&path, T0 + 3_600_001).acquired().is_some());
    }

    #[test]
    fn initial_creation_refuses_live_and_unknown_legacy_guards() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        for record in [
            format!(
                "pid={}\ncreated_ms={T0}\ntoken=legacy\n",
                std::process::id()
            ),
            String::new(),
            "pid=unknown\n".into(),
        ] {
            fs::write(Guard::path_for(&path), &record).unwrap();
            let error = Lease::try_acquire_at(&path, &opts(), T0 + 3_600_000).unwrap_err();
            assert_eq!(error.kind(), Some(io::ErrorKind::WouldBlock));
            assert!(!path.exists(), "initial creation obeys the legacy guard");
            assert_eq!(fs::read_to_string(Guard::path_for(&path)).unwrap(), record);
        }
    }

    #[test]
    fn initial_creation_refuses_a_paused_v2_takeover_guard() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        // Models a takeover paused after removing the old lease, before replacement.
        let guard = Guard::acquire(&path, TTL, T0).unwrap().unwrap();
        let error = Lease::try_acquire_at(&path, &opts(), T0 + 3_600_000).unwrap_err();
        assert_eq!(error.kind(), Some(io::ErrorKind::WouldBlock));
        assert!(!path.exists(), "no creator may enter the takeover interval");
        drop(guard);
        assert!(acquire_at(&path, T0 + 3_600_001).acquired().is_some());
    }

    #[test]
    fn unknown_legacy_guards_fail_closed_even_when_old() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        for record in [
            "",
            "pid=garbage\n",
            "created_ms=0\n",
            "pid=0\n",
            "pid=4294967296\n",
        ] {
            fs::write(Guard::path_for(&path), record).unwrap();
            assert!(Guard::acquire(&path, TTL, T0 + 3_600_000)
                .unwrap()
                .is_none());
            assert_eq!(fs::read_to_string(Guard::path_for(&path)).unwrap(), record);
        }
    }

    #[test]
    fn a_live_guard_lock_is_never_stolen_by_age() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        let guard = Guard::acquire(&path, TTL, T0).unwrap().expect("guard");
        // Far past any TTL, the held OS lock still excludes: only unlock (or process death)
        // frees it, never a second taker's clock.
        assert!(Guard::acquire(&path, TTL, T0 + 3_600_000)
            .unwrap()
            .is_none());
        drop(guard);
        assert!(Guard::acquire(&path, TTL, T0 + 3_600_001)
            .unwrap()
            .is_some());
    }

    #[test]
    fn renew_and_release_refuse_when_the_record_changed() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        let mut lease = acquire_at(&path, T0).acquired().unwrap();
        // Same token, different heartbeat: not the record this lease wrote.
        write_record(&path, std::process::id(), lease.token(), T0 + 99);
        let before = fs::read(&path).unwrap();
        let err = lease.renew_at(T0 + 100).unwrap_err();
        assert!(matches!(err, FsError::Locked { .. }), "{err:?}");
        assert_eq!(fs::read(&path).unwrap(), before);
        lease.release().unwrap();
        assert_eq!(fs::read(&path).unwrap(), before, "release leaves it");
    }

    #[test]
    fn release_of_a_changed_record_is_refused_even_before_any_renew() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        let lease = acquire_at(&path, T0).acquired().unwrap();
        write_record(&path, std::process::id(), lease.token(), T0 + 7);
        let before = fs::read(&path).unwrap();
        lease.release().unwrap();
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn renew_is_retryable_while_the_guard_is_held_and_release_leaves_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        let mut lease = acquire_at(&path, T0).acquired().unwrap();
        let before = fs::read(&path).unwrap();
        let guard = Guard::acquire(&path, TTL, T0).unwrap().expect("guard");
        let err = lease.renew_at(T0 + 1).unwrap_err();
        assert_eq!(err.kind(), Some(io::ErrorKind::WouldBlock));
        assert_eq!(fs::read(&path).unwrap(), before);
        drop(guard);
        lease.renew_at(T0 + 2).unwrap(); // the retry succeeds
        let guard = Guard::acquire(&path, TTL, now_ms())
            .unwrap()
            .expect("guard");
        let after = fs::read(&path).unwrap();
        lease.release().unwrap();
        assert_eq!(fs::read(&path).unwrap(), after, "release leaves the file");
        drop(guard);
    }
}
