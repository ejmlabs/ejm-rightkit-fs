//! Stable file identity across renames.
//!
//! A [`StableId`] names "this file" independent of its path. It is bound to the filesystem's own
//! identity ([`FileKey`]: device + inode on unix, volume serial + file index on Windows), which
//! survives `rename` and move within a volume, and falls back to a content fingerprint when the
//! file was replaced (atomic save, editor "safe write") or copied to another volume.
//!
//! Resolution order, cheapest first:
//! 1. the last known path still holds the same `FileKey` -> [`Resolution::Unchanged`];
//! 2. a bounded walk of the caller's search roots finds the same `FileKey` -> [`Resolution::Moved`];
//! 3. the last known path (or a search-root file) has the same size and head/tail fingerprint
//!    but a new `FileKey` -> [`Resolution::Replaced`] (the ledger is rebound);
//! 4. otherwise [`Resolution::Missing`].
//!
//! The ledger is one JSON file written atomically under a cross-process lock, so any number of
//! processes can track and resolve concurrently. Donors: HeardRight/ScrapeRight/ViewRight file
//! grants and clipboard delivery, which each keyed files by path.

use crate::atomic::atomic_write_private;
use crate::error::{FsError, Result};
pub use crate::file_key::FileKey;
use crate::lock::{FileLock, LockOptions};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const LEDGER_FILE: &str = "identity-ledger.json";
const LOCK_FILE: &str = "identity-ledger.lock";
const SAMPLE: u64 = 64 * 1024;
const LEDGER_VERSION: u32 = 1;

pub type StableId = String;

/// Cheap content fingerprint: size plus FNV-1a over the first and last 64 KiB. Only ever used to
/// shortlist a replaced or copied file; never as proof of identity on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub size: u64,
    pub sample_hash: u64,
}

impl Fingerprint {
    pub fn of_path(path: &Path) -> Result<Self> {
        let mut file = File::open(path).map_err(|e| FsError::io(path, e))?;
        let size = file.metadata().map_err(|e| FsError::io(path, e))?.len();
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        let mut feed = |bytes: &[u8]| {
            for b in bytes {
                hash ^= u64::from(*b);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        let mut buf = vec![0u8; SAMPLE as usize];
        let head = size.min(SAMPLE) as usize;
        file.read_exact(&mut buf[..head])
            .map_err(|e| FsError::io(path, e))?;
        feed(&buf[..head]);
        if size > SAMPLE {
            let tail = size.min(SAMPLE) as usize;
            file.seek(SeekFrom::Start(size - tail as u64))
                .map_err(|e| FsError::io(path, e))?;
            file.read_exact(&mut buf[..tail])
                .map_err(|e| FsError::io(path, e))?;
            feed(&buf[..tail]);
        }
        Ok(Self {
            size,
            sample_hash: hash,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tracked {
    pub id: StableId,
    pub key: FileKey,
    pub last_path: PathBuf,
    pub fingerprint: Fingerprint,
    pub tracked_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Same file at the same path.
    Unchanged(PathBuf),
    /// Same file (same `FileKey`) found at a new path; the ledger now points there.
    Moved(PathBuf),
    /// The file's inode changed (replaced/copied) but size and fingerprint match; rebound.
    Replaced(PathBuf),
    Missing,
}

impl Resolution {
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Unchanged(p) | Self::Moved(p) | Self::Replaced(p) => Some(p),
            Self::Missing => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SearchLimits {
    pub max_entries: usize,
    pub max_depth: usize,
}

impl Default for SearchLimits {
    fn default() -> Self {
        Self {
            max_entries: 200_000,
            max_depth: 12,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Ledger {
    version: u32,
    entries: BTreeMap<StableId, Tracked>,
}

pub struct IdentityStore {
    dir: PathBuf,
    limits: SearchLimits,
}

impl IdentityStore {
    /// `dir` is the app's state directory; the ledger and its lock live inside it.
    pub fn open(dir: &Path) -> Result<Self> {
        crate::private::ensure_private_dir(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            limits: SearchLimits::default(),
        })
    }

    pub fn with_limits(mut self, limits: SearchLimits) -> Self {
        self.limits = limits;
        self
    }

    fn locked<T>(&self, f: impl FnOnce(&mut Ledger) -> Result<(T, bool)>) -> Result<T> {
        let _lock = FileLock::acquire(
            &self.dir.join(LOCK_FILE),
            &LockOptions::wait(Duration::from_secs(10)).labeled("identity-ledger"),
        )?;
        let path = self.dir.join(LEDGER_FILE);
        let mut ledger = match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice::<Ledger>(&bytes).map_err(|e| FsError::Corrupt {
                    path: path.clone(),
                    reason: e.to_string(),
                })?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ledger {
                version: LEDGER_VERSION,
                ..Ledger::default()
            },
            Err(e) => return Err(FsError::io(&path, e)),
        };
        let (out, dirty) = f(&mut ledger)?;
        if dirty {
            let bytes = serde_json::to_vec_pretty(&ledger).expect("ledger serializes");
            atomic_write_private(&path, &bytes)?;
        }
        Ok(out)
    }

    /// Start tracking `path`; returns a new stable id.
    pub fn track(&self, path: &Path) -> Result<StableId> {
        let abs = absolute(path)?;
        let key = FileKey::of_path(&abs)?;
        let fingerprint = Fingerprint::of_path(&abs)?;
        let id = new_id()?;
        let entry = Tracked {
            id: id.clone(),
            key,
            last_path: abs,
            fingerprint,
            tracked_at_ms: now_ms(),
        };
        self.locked(|ledger| {
            ledger.entries.insert(id.clone(), entry);
            Ok(((), true))
        })?;
        Ok(id)
    }

    /// Same as [`track`](Self::track), but returns the existing id when this file (by `FileKey`)
    /// is already tracked.
    pub fn track_or_get(&self, path: &Path) -> Result<StableId> {
        let abs = absolute(path)?;
        let key = FileKey::of_path(&abs)?;
        let fingerprint = Fingerprint::of_path(&abs)?;
        self.locked(|ledger| {
            if let Some(existing) = ledger.entries.values().find(|t| t.key == key) {
                return Ok((existing.id.clone(), false));
            }
            let id = new_id()?;
            ledger.entries.insert(
                id.clone(),
                Tracked {
                    id: id.clone(),
                    key,
                    last_path: abs,
                    fingerprint,
                    tracked_at_ms: now_ms(),
                },
            );
            Ok((id, true))
        })
    }

    pub fn lookup(&self, id: &str) -> Result<Option<Tracked>> {
        self.locked(|ledger| Ok((ledger.entries.get(id).cloned(), false)))
    }

    pub fn forget(&self, id: &str) -> Result<bool> {
        self.locked(|ledger| {
            let removed = ledger.entries.remove(id).is_some();
            Ok((removed, removed))
        })
    }

    /// Re-record identity and fingerprint for `id` at `path` (after a deliberate rewrite).
    pub fn rebind(&self, id: &str, path: &Path) -> Result<()> {
        let abs = absolute(path)?;
        let key = FileKey::of_path(&abs)?;
        let fingerprint = Fingerprint::of_path(&abs)?;
        self.locked(|ledger| {
            let entry = ledger
                .entries
                .get_mut(id)
                .ok_or_else(|| FsError::UnknownId(id.to_string()))?;
            entry.key = key;
            entry.fingerprint = fingerprint;
            entry.last_path = abs;
            Ok(((), true))
        })
    }

    /// Find where `id` lives now, searching `roots` (bounded, never following symlinks) when the
    /// last known path no longer holds the file.
    pub fn resolve(&self, id: &str, roots: &[PathBuf]) -> Result<Resolution> {
        let tracked = self
            .lookup(id)?
            .ok_or_else(|| FsError::UnknownId(id.to_string()))?;

        // 1. Same file, same place.
        if let Ok(key) = FileKey::of_path(&tracked.last_path) {
            if key == tracked.key {
                return Ok(Resolution::Unchanged(tracked.last_path));
            }
        }

        // 2. Same file (by filesystem identity), new place; remember size-matching candidates.
        let mut candidates: Vec<PathBuf> = Vec::new();
        let mut moved: Option<PathBuf> = None;
        let mut budget = self.limits.max_entries;
        for root in roots {
            walk(
                root,
                0,
                self.limits.max_depth,
                &mut budget,
                &mut |path, meta| {
                    if FileKey::of_meta(meta, path) == Some(tracked.key) {
                        moved = Some(path.to_path_buf());
                        return true;
                    }
                    if meta.len() == tracked.fingerprint.size && candidates.len() < 64 {
                        candidates.push(path.to_path_buf());
                    }
                    false
                },
            );
            if moved.is_some() {
                break;
            }
        }
        if let Some(path) = moved {
            self.update_path(id, &path, None)?;
            return Ok(Resolution::Moved(path));
        }

        // 3. Replaced or copied: the old path itself first, then same-size candidates.
        let mut shortlist = Vec::new();
        if tracked.last_path.is_file() {
            shortlist.push(tracked.last_path.clone());
        }
        shortlist.extend(candidates);
        for path in shortlist {
            if Fingerprint::of_path(&path).ok() == Some(tracked.fingerprint) {
                let key = FileKey::of_path(&path)?;
                self.update_path(id, &path, Some(key))?;
                return Ok(Resolution::Replaced(path));
            }
        }
        Ok(Resolution::Missing)
    }

    fn update_path(&self, id: &str, path: &Path, new_key: Option<FileKey>) -> Result<()> {
        self.locked(|ledger| {
            if let Some(entry) = ledger.entries.get_mut(id) {
                entry.last_path = path.to_path_buf();
                if let Some(key) = new_key {
                    entry.key = key;
                }
            }
            Ok(((), true))
        })
    }
}

fn walk(
    dir: &Path,
    depth: usize,
    max_depth: usize,
    budget: &mut usize,
    visit: &mut dyn FnMut(&Path, &std::fs::Metadata) -> bool,
) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        if *budget == 0 {
            return true;
        }
        *budget -= 1;
        // `DirEntry::metadata` does not traverse symlinks.
        let Ok(meta) = entry.metadata() else { continue };
        let path = entry.path();
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_file() {
            if visit(&path, &meta) {
                return true;
            }
        } else if meta.is_dir()
            && depth < max_depth
            && walk(&path, depth + 1, max_depth, budget, visit)
        {
            return true;
        }
    }
    false
}

fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()
            .map_err(|e| FsError::io(path, e))?
            .join(path))
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn new_id() -> Result<StableId> {
    let mut bytes = [0u8; 16];
    random_bytes(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn random_bytes(buf: &mut [u8]) -> Result<()> {
    #[cfg(unix)]
    {
        File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(buf))
            .map_err(|e| FsError::io("/dev/urandom", e))
    }
    #[cfg(windows)]
    {
        #[link(name = "bcrypt")]
        unsafe extern "system" {
            fn BCryptGenRandom(
                algorithm: *mut std::ffi::c_void,
                buffer: *mut u8,
                length: u32,
                flags: u32,
            ) -> i32;
        }
        // SAFETY: buffer is valid for its length; flag 2 = system-preferred RNG.
        let status =
            unsafe { BCryptGenRandom(std::ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32, 2) };
        if status >= 0 {
            Ok(())
        } else {
            Err(FsError::io(
                "BCryptGenRandom",
                std::io::Error::other("RNG failure"),
            ))
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = buf;
        Err(FsError::Unavailable("random bytes"))
    }
}
