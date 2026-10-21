//! File names that come from somewhere else: sanitising a download name and picking a free one.
//!
//! A name taken from a URL, a `Content-Disposition` header or a mail attachment is attacker
//! controlled text. [`sanitize_file_name`] turns it into one safe path component that is valid on
//! Windows, macOS and Linux alike (the rules are the strictest of the three, so a file saved on one
//! platform can be copied to another): no directory part, no control or bidirectional-override
//! characters, no reserved Windows device names, no trailing dot or space, no leading dot, bounded
//! length with the extension kept. It never returns an empty name.
//!
//! [`numbered_candidates`] lists `name.ext`, `name (1).ext`, `name (2).ext`, ...; with feature `fs`,
//! [`create_unique_file`] creates the first free one exclusively (`create_new`, so two processes
//! never get the same file) and [`create_download_file`] sanitises first.
//!
//! The pure functions here need no feature and behave the same on every platform, so a name
//! sanitised on macOS is identical to the one Windows would produce.

/// How [`sanitize_file_name_with`] shapes a name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NameOptions {
    /// Longest result in UTF-8 bytes, clamped to `8..=240`. The default, 200, leaves room for a
    /// ` (9999)` collision suffix inside the 255-byte component limit of the common filesystems.
    pub max_bytes: usize,
    /// Returned when nothing usable is left. Must itself be a safe name; it is sanitised too, and
    /// `"download"` stands in if it sanitises to nothing.
    pub fallback: &'static str,
}

impl Default for NameOptions {
    fn default() -> Self {
        Self {
            max_bytes: 200,
            fallback: "download",
        }
    }
}

/// True for a name Windows treats as a device, whatever its extension or letter case: `CON`, `PRN`,
/// `AUX`, `NUL`, `COM1`-`COM9`, `LPT1`-`LPT9` (also with the superscript digits `¹ ² ³` that
/// Windows maps to the same devices), `CONIN$` and `CONOUT$`. The part before the first `.` decides:
/// `nul.txt` and `NUL.tar.gz` are reserved, and so is `con ` (trailing spaces are dropped).
pub fn is_windows_reserved_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    let upper = stem.to_uppercase();
    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || device_with_number(&upper, "COM")
        || device_with_number(&upper, "LPT")
}

fn device_with_number(upper: &str, prefix: &str) -> bool {
    let Some(rest) = upper.strip_prefix(prefix) else {
        return false;
    };
    let mut chars = rest.chars();
    matches!(
        (chars.next(), chars.next()),
        (Some('1'..='9' | '\u{b9}' | '\u{b2}' | '\u{b3}'), None)
    )
}

/// True when `name` cannot be used as one path component on Windows: empty, `.` or `..`, a reserved
/// device name ([`is_windows_reserved_name`]), a trailing dot or space (Windows drops it, so the name
/// would address a different entry), or any of `\ / : * ? " < > |` or a control character (a `:`
/// names an NTFS alternate data stream). Used by the containment checks for names on Windows.
pub fn is_unsafe_windows_component(name: &str) -> bool {
    name.is_empty()
        || name == "."
        || name == ".."
        || name.ends_with(['.', ' '])
        || name.chars().any(|c| {
            c < ' '
                || c == '\u{7f}'
                || matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        })
        || is_windows_reserved_name(name)
}

/// Characters removed outright: C0 and C1 controls, the Unicode line and paragraph separators,
/// zero-width and invisible formatting characters, and every bidirectional override, embedding and
/// isolate (the classic `invoice\u{202e}fdp.exe` spoof).
fn is_stripped(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{ad}'
                | '\u{61c}'
                | '\u{180e}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206f}'
                | '\u{feff}'
                | '\u{fff9}'..='\u{fffb}'
        )
}

/// Characters that cannot appear in a Windows file name, plus the path separators. Replaced by `_`.
fn is_replaced(c: char) -> bool {
    matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
}

/// [`sanitize_file_name_with`] with [`NameOptions::default`].
pub fn sanitize_file_name(raw: &str) -> String {
    sanitize_file_name_with(raw, &NameOptions::default())
}

/// Turn `raw` into one safe, portable file name.
///
/// In order: only the last non-empty segment between `/` and `\` is kept (a name that is only
/// separators has none); control, invisible and bidirectional-control characters are removed; `: * ? " < > |` become
/// `_`; leading dots and whitespace are dropped (no hidden file, no `.` or `..`); trailing dots and
/// whitespace are dropped (Windows would drop them silently); a reserved Windows device name
/// ([`is_windows_reserved_name`]) gets a `_` prefix; the result is cut to `options.max_bytes` at a
/// character boundary, keeping the extension (up to 24 bytes of it) when there is one; and an empty
/// result is the fallback name. The result is never empty, never `.` or `..`, and contains no
/// separator, so joining it to a directory cannot leave that directory.
pub fn sanitize_file_name_with(raw: &str, options: &NameOptions) -> String {
    let max = options.max_bytes.clamp(8, 240);
    match shape(raw, max) {
        Some(name) => name,
        None => shape(options.fallback, max).unwrap_or_else(|| "download".to_string()),
    }
}

fn shape(raw: &str, max: usize) -> Option<String> {
    let last = raw
        .rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or("");
    let cleaned: String = last
        .chars()
        .filter(|c| !is_stripped(*c))
        .map(|c| if is_replaced(c) { '_' } else { c })
        .collect();
    let trimmed = cleaned
        .trim_start_matches(|c: char| c == '.' || c.is_whitespace())
        .trim_end_matches(|c: char| c == '.' || c.is_whitespace());
    if trimmed.is_empty() {
        return None;
    }
    let settle = |name: String| -> String {
        // A cut can expose a trailing dot or space.
        name.trim_end_matches(|c: char| c == '.' || c.is_whitespace())
            .to_string()
    };
    let mut name = settle(truncate_keeping_extension(trimmed, max));
    if name.is_empty() {
        return None;
    }
    // After the cut, so a cut that leaves exactly `COM1.txt` is still caught. The prefix makes the
    // stem start with `_`, which no reserved name does, and a second cut keeps that prefix.
    if is_windows_reserved_name(&name) {
        name.insert(0, '_');
        name = settle(truncate_keeping_extension(&name, max));
    }
    Some(name)
}

/// Cut `name` to at most `max` bytes at a character boundary. When it has an extension of at most 24
/// bytes (the part after the last `.`, with the dot), the stem is shortened and the extension kept.
fn truncate_keeping_extension(name: &str, max: usize) -> String {
    if name.len() <= max {
        return name.to_string();
    }
    // An extension is kept only when it is short and leaves at least half the budget to the stem.
    let split = name
        .rfind('.')
        .filter(|i| *i > 0 && name.len() - *i <= 24usize.min(max / 2));
    let (stem, ext) = match split {
        Some(i) => name.split_at(i),
        None => (name, ""),
    };
    let budget = max - ext.len();
    let mut end = stem.len().min(budget);
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &stem[..end], ext)
}

/// `name`, then `stem (1).ext`, `stem (2).ext`, ... up to `count` names in all (the first is
/// `name` itself). The number goes before the last extension; a name with no extension, or a
/// leading-dot name, takes it at the end. `name` should already be safe ([`sanitize_file_name`]).
/// A `count` of zero gives an empty list.
pub fn numbered_candidates(name: &str, count: usize) -> Vec<String> {
    let split = name.rfind('.').filter(|i| *i > 0);
    let (stem, ext) = match split {
        Some(i) => name.split_at(i),
        None => (name, ""),
    };
    (0..count)
        .map(|n| {
            if n == 0 {
                name.to_string()
            } else {
                format!("{stem} ({n}){ext}")
            }
        })
        .collect()
}

/// The most names [`create_unique_file`] tries before giving up with `AlreadyExists`.
#[cfg(feature = "fs")]
pub const MAX_UNIQUE_TRIES: usize = 10_000;

/// Exclusively create the first free name of [`numbered_candidates`]`(name, MAX_UNIQUE_TRIES)` in
/// `dir` and return its path and the open file (write-only).
///
/// `create_new` is the atomic claim, so two processes or threads asking for the same name get
/// different files, and a name that exists as a file, directory or symlink (a link is never
/// followed) is skipped. The file is created with the platform's default permissions (for a download
/// the user will open; use [`crate::create_private_new`] for a secret). `name` must be a single safe
/// component: anything with a separator, `..`, NUL or an unsafe Windows name is refused as
/// [`crate::FsError::BadName`], so a caller that skipped [`sanitize_file_name`] cannot write outside
/// `dir`. Every other error ends the search. When every candidate exists the result is
/// [`crate::FsError::AlreadyExists`].
#[cfg(feature = "fs")]
pub fn create_unique_file(
    dir: &std::path::Path,
    name: &str,
) -> crate::error::Result<(std::path::PathBuf, std::fs::File)> {
    use crate::error::FsError;
    if name.contains('\0') || name.contains(['/', '\\']) || is_unsafe_windows_component(name) {
        return Err(FsError::BadName(name.to_string()));
    }
    crate::atomic::reject_reserved_name(name)?;
    let mut last = dir.join(name);
    for candidate in numbered_candidates(name, MAX_UNIQUE_TRIES) {
        let path = dir.join(&candidate);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = path,
            // Windows refuses `create_new` on an existing directory with access denied, not
            // "already exists". A name that is taken by anything (a directory, a link) is skipped.
            Err(_) if std::fs::symlink_metadata(&path).is_ok() => last = path,
            Err(e) => return Err(FsError::io(&path, e)),
        }
    }
    Err(FsError::AlreadyExists(last))
}

/// [`sanitize_file_name`] then [`create_unique_file`]: the one call for "save this download under
/// the name it asked for, or the next free one".
#[cfg(feature = "fs")]
pub fn create_download_file(
    dir: &std::path::Path,
    raw_name: &str,
) -> crate::error::Result<(std::path::PathBuf, std::fs::File)> {
    create_unique_file(dir, &sanitize_file_name(raw_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_parts_are_dropped_for_both_separators() {
        assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
        assert_eq!(
            sanitize_file_name(r"C:\Windows\System32\cmd.exe"),
            "cmd.exe"
        );
        assert_eq!(sanitize_file_name("a/b\\c/d.txt"), "d.txt");
        assert_eq!(sanitize_file_name("/"), "download");
        assert_eq!(sanitize_file_name("dir/"), "dir");
        assert_eq!(sanitize_file_name(r"\\server\share\f.bin"), "f.bin");
    }

    #[test]
    fn dot_names_and_hidden_files_are_not_produced() {
        for raw in [".", "..", "...", " . ", ".. ..", ""] {
            assert_eq!(sanitize_file_name(raw), "download", "{raw:?}");
        }
        assert_eq!(sanitize_file_name(".bashrc"), "bashrc");
        assert_eq!(sanitize_file_name("..hidden.txt"), "hidden.txt");
    }

    #[test]
    fn trailing_dots_and_spaces_are_dropped() {
        assert_eq!(sanitize_file_name("report.pdf   "), "report.pdf");
        assert_eq!(sanitize_file_name("report.pdf. . "), "report.pdf");
        assert_eq!(sanitize_file_name("a b.txt"), "a b.txt");
    }

    #[test]
    fn invalid_characters_become_underscores() {
        assert_eq!(
            sanitize_file_name("a:b*c?d\"e<f>g|h.txt"),
            "a_b_c_d_e_f_g_h.txt"
        );
        // An NTFS alternate data stream name cannot be smuggled in.
        assert_eq!(sanitize_file_name("file.txt:evil.exe"), "file.txt_evil.exe");
    }

    #[test]
    fn controls_nul_and_bidi_overrides_are_removed() {
        assert_eq!(sanitize_file_name("a\0b.txt"), "ab.txt");
        assert_eq!(sanitize_file_name("a\u{1}\u{1f}\u{7f}\u{85}b"), "ab");
        // `invoice` + RLO + `fdp.exe` would render as `invoiceexe.pdf`.
        assert_eq!(
            sanitize_file_name("invoice\u{202e}fdp.exe"),
            "invoicefdp.exe"
        );
        for c in [
            '\u{200e}', '\u{200f}', '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}', '\u{2066}',
            '\u{2067}', '\u{2068}', '\u{2069}', '\u{061c}', '\u{200b}', '\u{feff}', '\u{2028}',
            '\u{2029}',
        ] {
            assert_eq!(
                sanitize_file_name(&format!("a{c}b")),
                "ab",
                "U+{:04X}",
                c as u32
            );
        }
        assert_eq!(sanitize_file_name("\u{202e}\u{200b}"), "download");
    }

    #[test]
    fn reserved_windows_names_get_a_prefix() {
        for (raw, want) in [
            ("CON", "_CON"),
            ("nul.txt", "_nul.txt"),
            ("Aux.tar.gz", "_Aux.tar.gz"),
            ("com1", "_com1"),
            ("COM9.log", "_COM9.log"),
            ("lpt3.txt", "_lpt3.txt"),
            ("con .txt", "_con .txt"),
            ("CONIN$", "_CONIN$"),
            ("COM\u{b9}", "_COM\u{b9}"),
        ] {
            assert_eq!(sanitize_file_name(raw), want, "{raw:?}");
        }
        // Names that merely start like a device are untouched.
        for raw in [
            "console.txt",
            "com10",
            "com0",
            "com",
            "lpt",
            "auxiliary",
            "null",
        ] {
            assert_eq!(sanitize_file_name(raw), raw, "{raw:?}");
        }
    }

    #[test]
    fn predicate_for_unsafe_windows_components() {
        for bad in [
            "", ".", "..", "a.", "a ", "a:b", "a\\b", "a/b", "a*", "a?", "a\"", "a<", "a>", "a|",
            "con", "NUL.txt", "a\u{1}",
        ] {
            assert!(is_unsafe_windows_component(bad), "{bad:?}");
        }
        for ok in ["a", "a.txt", "a b", ".hidden", "console", "com10", "é"] {
            assert!(!is_unsafe_windows_component(ok), "{ok:?}");
        }
    }

    #[test]
    fn long_names_are_cut_on_a_boundary_and_keep_the_extension() {
        let long = format!("{}.pdf", "a".repeat(500));
        let got = sanitize_file_name(&long);
        assert_eq!(got.len(), 200);
        assert!(got.ends_with(".pdf"));
        // A multi-byte character straddling the cut is dropped whole.
        let wide = format!("{}.txt", "\u{e9}".repeat(300));
        let got = sanitize_file_name(&wide);
        assert!(got.len() <= 200 && got.ends_with(".txt"));
        assert!(got.is_char_boundary(got.len() - 4));
        // No extension: a plain cut. A huge "extension" is not preserved.
        let bare = "b".repeat(300);
        assert_eq!(sanitize_file_name(&bare).len(), 200);
        let long_ext = format!("x.{}", "e".repeat(300));
        assert!(sanitize_file_name(&long_ext).len() <= 200);
        let options = NameOptions {
            max_bytes: 12,
            fallback: "download",
        };
        assert_eq!(
            sanitize_file_name_with("abcdefghijklmnop.txt", &options),
            "abcdefgh.txt"
        );
    }

    #[test]
    fn a_cut_that_leaves_a_reserved_name_is_caught() {
        let options = NameOptions {
            max_bytes: 8,
            fallback: "download",
        };
        // `COM1abcd.txt` cut to 8 bytes keeping `.txt` is `COM1.txt`: a device name.
        let got = sanitize_file_name_with("COM1abcd.txt", &options);
        assert!(!is_windows_reserved_name(&got), "{got:?}");
        assert!(
            got.len() <= 8 && got.ends_with(".txt") || got.starts_with('_'),
            "{got:?}"
        );
    }

    #[test]
    fn a_cut_that_exposes_a_trailing_dot_is_settled() {
        let options = NameOptions {
            max_bytes: 8,
            fallback: "download",
        };
        let got = sanitize_file_name_with("abcdefg.hijklmnopqrstuvwxyzabcdefgh", &options);
        assert!(!got.ends_with('.') && !got.is_empty(), "{got:?}");
    }

    #[test]
    fn the_fallback_is_used_and_is_itself_sanitised() {
        let options = NameOptions {
            max_bytes: 200,
            fallback: "../x/CON",
        };
        assert_eq!(sanitize_file_name_with("..", &options), "_CON");
        let bad = NameOptions {
            max_bytes: 200,
            fallback: "",
        };
        assert_eq!(sanitize_file_name_with("..", &bad), "download");
    }

    #[test]
    fn no_result_ever_holds_a_separator_or_dots_only() {
        for raw in [
            "a/../b",
            "..\\..\\x",
            "%2e%2e/x",
            "....//",
            "x\u{2215}y",
            "a\u{ff0f}b",
        ] {
            let got = sanitize_file_name(raw);
            assert!(!got.contains(['/', '\\', '\0']), "{raw:?} -> {got:?}");
            assert!(got != "." && got != "..", "{raw:?} -> {got:?}");
            assert!(!is_unsafe_windows_component(&got), "{raw:?} -> {got:?}");
        }
    }

    #[test]
    fn candidates_number_before_the_extension() {
        assert_eq!(
            numbered_candidates("report.pdf", 3),
            ["report.pdf", "report (1).pdf", "report (2).pdf"]
        );
        assert_eq!(numbered_candidates("noext", 2), ["noext", "noext (1)"]);
        assert_eq!(
            numbered_candidates("a.tar.gz", 2),
            ["a.tar.gz", "a.tar (1).gz"]
        );
        assert_eq!(numbered_candidates("x", 0), Vec::<String>::new());
        assert_eq!(numbered_candidates(".rc", 2), [".rc", ".rc (1)"]);
    }

    #[cfg(feature = "fs")]
    mod fs_tests {
        use super::*;
        use crate::error::FsError;

        #[test]
        fn first_free_name_is_taken_exclusively() {
            let dir = tempfile::tempdir().unwrap();
            let (a, _fa) = create_unique_file(dir.path(), "f.txt").unwrap();
            let (b, _fb) = create_unique_file(dir.path(), "f.txt").unwrap();
            let (c, _fc) = create_unique_file(dir.path(), "f.txt").unwrap();
            assert_eq!(a, dir.path().join("f.txt"));
            assert_eq!(b, dir.path().join("f (1).txt"));
            assert_eq!(c, dir.path().join("f (2).txt"));
        }

        #[test]
        fn an_existing_directory_or_file_is_skipped_and_never_written() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir(dir.path().join("d")).unwrap();
            std::fs::write(dir.path().join("d (1)"), b"keep").unwrap();
            let (p, _f) = create_unique_file(dir.path(), "d").unwrap();
            assert_eq!(p, dir.path().join("d (2)"));
            assert_eq!(std::fs::read(dir.path().join("d (1)")).unwrap(), b"keep");
        }

        #[cfg(unix)]
        #[test]
        fn a_symlink_at_the_name_is_skipped_not_followed() {
            let dir = tempfile::tempdir().unwrap();
            let outside = dir.path().join("outside");
            std::fs::write(&outside, b"secret").unwrap();
            std::os::unix::fs::symlink(&outside, dir.path().join("f")).unwrap();
            let (p, _f) = create_unique_file(dir.path(), "f").unwrap();
            assert_eq!(p, dir.path().join("f (1)"));
            assert_eq!(std::fs::read(&outside).unwrap(), b"secret");
        }

        #[test]
        fn an_unsafe_name_is_refused_before_anything_is_created() {
            let dir = tempfile::tempdir().unwrap();
            for bad in [
                "../x", "a/b", "a\\b", "..", "", "NUL", "a\0b", "x:y", "trail.",
            ] {
                let err = create_unique_file(dir.path(), bad).unwrap_err();
                assert!(matches!(err, FsError::BadName(_)), "{bad:?}: {err:?}");
            }
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        }

        #[test]
        fn a_temp_shaped_name_is_refused() {
            let dir = tempfile::tempdir().unwrap();
            assert!(create_unique_file(dir.path(), ".x.rktmp-1-1-0").is_err());
        }

        #[test]
        fn create_download_file_sanitises_first() {
            let dir = tempfile::tempdir().unwrap();
            let (p, _f) = create_download_file(dir.path(), "../../etc/CON.txt").unwrap();
            assert_eq!(p, dir.path().join("_CON.txt"));
            assert_eq!(p.parent().unwrap(), dir.path());
        }
    }
}
