//! Canonical paths without the Windows `\\?\` verbatim prefix.
//!
//! `std::fs::canonicalize` returns `\\?\C:\dir` on Windows. That prefix leaks into webview asset
//! URLs, child-process arguments (`explorer`, `rundll32`, `ffmpeg`) and user-facing text.
//! [`canonicalize_plain`] resolves the path and drops the prefix when that is safe.
//!
//! The rule is deliberately narrow: only `\\?\<drive>:` (a verbatim drive path) is stripped.
//! `\\?\UNC\server\share`, device paths (`\\.\` and `\\?\Volume{...}`) and any path that is not
//! valid UTF-8 are returned unchanged, because stripping them changes what they name.

use std::path::{Path, PathBuf};

/// [`std::fs::canonicalize`], then [`plain_path`]. On non-Windows targets it is exactly
/// `std::fs::canonicalize`. Errors are the canonicalize errors unchanged.
pub fn canonicalize_plain(path: impl AsRef<Path>) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(path).map(plain_path)
}

/// Drop a leading `\\?\` from a verbatim drive path (`\\?\C:\x` becomes `C:\x`). Anything else,
/// including `\\?\UNC\...`, device paths and non-UTF-8 paths, is returned unchanged. A pure string
/// transform: it does not touch the filesystem and behaves the same on every platform, so a path
/// that does not start with the prefix is never altered.
pub fn plain_path(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(strip_verbatim_drive) {
        Some(stripped) => PathBuf::from(stripped),
        None => path,
    }
}

fn strip_verbatim_drive(s: &str) -> Option<&str> {
    let rest = s.strip_prefix(r"\\?\")?;
    let b = rest.as_bytes();
    let drive = b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
    // `\\?\C:` must be followed by a separator or end, so `\\?\C:foo` style oddities stay as is.
    let bounded = b.len() == 2 || b[2] == b'\\';
    (drive && bounded).then_some(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        plain_path(PathBuf::from(s))
    }

    #[test]
    fn strips_verbatim_drive() {
        assert_eq!(
            p(r"\\?\C:\Users\a\b.txt"),
            PathBuf::from(r"C:\Users\a\b.txt")
        );
        assert_eq!(p(r"\\?\d:\"), PathBuf::from(r"d:\"));
        assert_eq!(p(r"\\?\C:"), PathBuf::from("C:"));
    }

    #[test]
    fn leaves_unc_device_and_plain_paths() {
        for s in [
            r"\\?\UNC\server\share\x",
            r"\\?\Volume{1234}\x",
            r"\\.\COM1",
            r"\\?\C:foo",
            r"\\?\1:\x",
            r"C:\Users\a",
            "/tmp/x",
            "",
        ] {
            assert_eq!(p(s), PathBuf::from(s), "{s}");
        }
    }

    #[test]
    fn canonicalize_plain_resolves_existing_and_reports_missing() {
        let dir = std::env::temp_dir();
        let got = canonicalize_plain(&dir).expect("temp dir exists");
        assert!(got.is_absolute());
        assert!(
            !got.to_string_lossy().starts_with(r"\\?\")
                || got.to_string_lossy().starts_with(r"\\?\UNC")
        );
        assert!(canonicalize_plain(dir.join("rightkit-fs-no-such-path-272")).is_err());
    }
}
