//! Readers for a file another process replaces.
//!
//! A replace (`atomic_write`, `publish_file`, `StateDir::write`) never shows a reader a truncated
//! file or a mix of two versions. On Windows it can, for a moment, refuse a reader's *open*: the
//! open fails with `ERROR_ACCESS_DENIED` (5) or `ERROR_SHARING_VIOLATION` (32) while the other
//! process is moving a new file over the name. The refusal passes by itself, so a reader that must
//! not mistake it for "no file" or "not allowed" tries again.
//!
//! - [`open_replaced`]: `File::open` that retries those two refusals on Windows.
//! - [`read_replaced`] / [`read_to_string_replaced`]: the whole file, through [`open_replaced`].
//!
//! **Windows:** up to [`MAX_OPEN_TRIES`] opens, 1 ms growing by 1 ms per try to a 20 ms ceiling
//! between them, so the worst case waits a little over a second. A refusal that is a real
//! permission failure (error 5 that never passes) is returned after the last try, unchanged.
//! Every other error, `NotFound` included, is returned at once. **Other platforms:** one plain
//! open; no retry exists because `rename(2)` never refuses an opening reader.
//!
//! Only the open is retried. Bytes read from the returned handle are one version of the file: the
//! one that was named when the open succeeded.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::time::Duration;

/// The most opens a Windows reader makes: the first, and up to 63 retries.
pub const MAX_OPEN_TRIES: u32 = 64;

/// Longest pause between two opens.
const MAX_PAUSE_MS: u64 = 20;

/// `ERROR_ACCESS_DENIED`.
const ERROR_ACCESS_DENIED: i32 = 5;
/// `ERROR_SHARING_VIOLATION`.
const ERROR_SHARING_VIOLATION: i32 = 32;

/// Whether a failed open carries a Win32 code that a concurrent replace produces: access denied
/// or a sharing violation. Pure; it looks only at the code.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn is_replace_refusal(code: Option<i32>) -> bool {
    matches!(
        code,
        Some(ERROR_ACCESS_DENIED) | Some(ERROR_SHARING_VIOLATION)
    )
}

/// The pause after failed try number `try_index` (0 for the first): 1 ms, plus 1 ms per earlier
/// failure, never more than 20 ms.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn pause_after(try_index: u32) -> Duration {
    Duration::from_millis((u64::from(try_index) + 1).min(MAX_PAUSE_MS))
}

/// Run `open` up to `tries` times, calling `pause` with the wait before each retry, and retry
/// only a refusal that [`is_replace_refusal`] accepts. Any other result ends the loop.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn open_with_retry<T>(
    tries: u32,
    mut open: impl FnMut() -> io::Result<T>,
    mut pause: impl FnMut(Duration),
) -> io::Result<T> {
    let mut attempt = 0u32;
    loop {
        match open() {
            Err(error)
                if attempt + 1 < tries.max(1) && is_replace_refusal(error.raw_os_error()) =>
            {
                pause(pause_after(attempt));
                attempt += 1;
            }
            other => return other,
        }
    }
}

/// Open `path` for reading, retrying on Windows an open that a concurrent replace refused (see the
/// module docs). Elsewhere this is `File::open`.
pub fn open_replaced(path: &Path) -> io::Result<File> {
    #[cfg(windows)]
    {
        open_with_retry(MAX_OPEN_TRIES, || File::open(path), std::thread::sleep)
    }
    #[cfg(not(windows))]
    {
        File::open(path)
    }
}

/// Read the whole file at `path` through [`open_replaced`].
pub fn read_replaced(path: &Path) -> io::Result<Vec<u8>> {
    let mut file = open_replaced(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Read the whole file at `path` as UTF-8 through [`open_replaced`]. Invalid UTF-8 is
/// `InvalidData`, as for `std::fs::read_to_string`.
pub fn read_to_string_replaced(path: &Path) -> io::Result<String> {
    let mut file = open_replaced(path)?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn os(code: i32) -> io::Error {
        io::Error::from_raw_os_error(code)
    }

    #[test]
    fn only_access_denied_and_sharing_violation_are_refusals() {
        assert!(is_replace_refusal(Some(5)));
        assert!(is_replace_refusal(Some(32)));
        for other in [2, 3, 33, 80, 123, 1224] {
            assert!(!is_replace_refusal(Some(other)), "{other}");
        }
        assert!(!is_replace_refusal(None));
    }

    #[test]
    fn the_pause_grows_by_a_millisecond_and_stops_at_twenty() {
        assert_eq!(pause_after(0), Duration::from_millis(1));
        assert_eq!(pause_after(1), Duration::from_millis(2));
        assert_eq!(pause_after(19), Duration::from_millis(20));
        assert_eq!(pause_after(63), Duration::from_millis(20));
        assert_eq!(pause_after(u32::MAX), Duration::from_millis(20));
    }

    #[test]
    fn the_worst_case_wait_is_bounded() {
        let total: Duration = (0..MAX_OPEN_TRIES - 1).map(pause_after).sum();
        assert!(total <= Duration::from_secs(2), "{total:?}");
    }

    #[test]
    fn a_refusal_is_retried_until_the_open_succeeds() {
        let calls = Cell::new(0u32);
        let pauses = RefCell::new(Vec::new());
        let got = open_with_retry(
            MAX_OPEN_TRIES,
            || {
                calls.set(calls.get() + 1);
                match calls.get() {
                    1 => Err(os(5)),
                    2 => Err(os(32)),
                    _ => Ok("opened"),
                }
            },
            |d| pauses.borrow_mut().push(d),
        );
        assert_eq!(got.unwrap(), "opened");
        assert_eq!(calls.get(), 3);
        assert_eq!(
            *pauses.borrow(),
            vec![Duration::from_millis(1), Duration::from_millis(2)]
        );
    }

    #[test]
    fn a_persistent_refusal_stops_after_the_last_try_and_is_returned() {
        let calls = Cell::new(0u32);
        let got: io::Result<()> = open_with_retry(
            MAX_OPEN_TRIES,
            || {
                calls.set(calls.get() + 1);
                Err(os(5))
            },
            |_| {},
        );
        assert_eq!(calls.get(), MAX_OPEN_TRIES);
        assert_eq!(got.unwrap_err().raw_os_error(), Some(5));
    }

    #[test]
    fn any_other_error_ends_the_loop_at_once() {
        let calls = Cell::new(0u32);
        let got: io::Result<()> = open_with_retry(
            MAX_OPEN_TRIES,
            || {
                calls.set(calls.get() + 1);
                Err(io::Error::new(io::ErrorKind::NotFound, "gone"))
            },
            |_| panic!("no pause for a non-refusal"),
        );
        assert_eq!(calls.get(), 1);
        assert_eq!(got.unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn zero_tries_still_opens_once() {
        let calls = Cell::new(0u32);
        let got: io::Result<()> = open_with_retry(
            0,
            || {
                calls.set(calls.get() + 1);
                Err(os(32))
            },
            |_| {},
        );
        assert_eq!(calls.get(), 1);
        assert!(got.is_err());
    }

    #[test]
    fn reads_an_ordinary_file_and_reports_a_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "hello").unwrap();
        assert_eq!(read_replaced(&path).unwrap(), b"hello");
        assert_eq!(read_to_string_replaced(&path).unwrap(), "hello");
        let missing = open_replaced(&dir.path().join("none")).unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
    }
}
