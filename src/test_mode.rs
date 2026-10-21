//! Portable test-mode root validation (M21): the filesystem half of the release-vs-test decision,
//! with no daemon, suite or IPC dependency.
//!
//! Code placement only: this module is the recommended portable owner of the validation rules,
//! not a ratified ownership change (D06). A host (for example `rightkit-daemon`) supplies the real
//! per-user locations to protect, reads its own activation/root selectors, and constructs its own
//! suite/app roots from the [`ValidatedRoot`] returned here. Nothing in this module reads the
//! environment, so a caller decides when (and whether) selector reads happen at all.
//!
//! Rules, mirroring the established daemon decision: activation and a root must arrive together
//! (either half alone is a [`Refusal`]); the root must be absolute without `..`, an existing
//! directory that is not itself a symlink, canonically disjoint from every protected real
//! location (equal, ancestor, descendant, or reached through a symlinked ancestor), outside
//! cloud-sync folders, private to the current user (Unix owner + mode, Windows SID/DACL through
//! [`crate::private::verify_owner_only`]), and bound to the asking app by the runner's binding
//! file (`.rightkit-app.json`, `{"app": "<id>"}`).

use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::error::FsError;

/// Binding file a QA runner writes inside every home it creates.
pub const BINDING_FILE: &str = ".rightkit-app.json";

/// Why a test run was refused. [`Refusal::rule`] is the stable kebab-case vocabulary shared with
/// the daemon decision and the QA guard.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refusal {
    /// Activation was supplied but no root was given.
    ActivationWithoutRoot,
    /// A root was given but activation was not supplied.
    RootWithoutActivation,
    RootNotAbsolute,
    /// Missing, or not a directory.
    RootNotDirectory,
    /// The root itself is a symlink.
    RootHasSymlink,
    /// Equal to, an ancestor of, a descendant of, or resolving into a protected real location.
    RootOverlapsUserData,
    /// Inside a cloud-synchronised folder (name heuristic, as in the daemon decision).
    RootOnRemovableOrCloud,
    /// Not private to the current user (Unix: wrong owner or group/other bits; Windows: DACL).
    RootNotPrivate,
    /// No readable binding file: not a home a runner created.
    RootNotCreatedByRunner,
    /// The binding names a different app.
    BindingAppMismatch,
    /// No protected real location was supplied, so disjointness cannot be proven.
    UserRootUnknown,
    /// A child path asked to leave the validated root.
    ChildEscapesRoot,
    /// A filesystem error while verifying.
    Io(String),
}

impl Refusal {
    /// A stable kebab-case rule id.
    pub fn rule(&self) -> &'static str {
        match self {
            Refusal::ActivationWithoutRoot => "activation-without-root",
            Refusal::RootWithoutActivation => "root-without-activation",
            Refusal::RootNotAbsolute => "root-not-absolute",
            Refusal::RootNotDirectory => "root-not-directory",
            Refusal::RootHasSymlink => "root-has-symlink",
            Refusal::RootOverlapsUserData => "root-overlaps-user-data",
            Refusal::RootOnRemovableOrCloud => "root-on-removable-or-cloud",
            Refusal::RootNotPrivate => "root-not-private",
            Refusal::RootNotCreatedByRunner => "root-not-created-by-runner",
            Refusal::BindingAppMismatch => "binding-app-mismatch",
            Refusal::UserRootUnknown => "user-root-unknown",
            Refusal::ChildEscapesRoot => "child-escapes-root",
            Refusal::Io(_) => "io",
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Io(e) => write!(f, "test mode refused ({}): {e}", self.rule()),
            _ => write!(f, "test mode refused ({})", self.rule()),
        }
    }
}

impl std::error::Error for Refusal {}

impl From<io::Error> for Refusal {
    fn from(e: io::Error) -> Self {
        Refusal::Io(e.to_string())
    }
}

impl From<FsError> for Refusal {
    fn from(e: FsError) -> Self {
        match e {
            FsError::NotOwned(_) | FsError::TooPermissive(_) | FsError::UnsafeAcl(_) => {
                Refusal::RootNotPrivate
            }
            FsError::Symlink(_) => Refusal::RootHasSymlink,
            other => Refusal::Io(other.to_string()),
        }
    }
}

/// A verified test root: canonical, not a symlink at the leaf, disjoint from the protected real
/// locations, private to the current user, and bound to one app by the runner. Opaque: the only
/// way to obtain one is [`decide`] / [`validate_root`], and children are derived through
/// [`ValidatedRoot::child`] so a caller cannot re-derive an unvalidated path by string joins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedRoot {
    path: PathBuf,
    app_id: String,
}

impl ValidatedRoot {
    /// The canonical root directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The app id the runner's binding names.
    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    /// A child of the root. `relative` must be relative, non-empty, and contain no `..`, root or
    /// prefix component; anything else is [`Refusal::ChildEscapesRoot`]. Every existing ancestor
    /// must resolve inside this root. Missing suffixes are allowed; lookup failures fail closed.
    pub fn child(&self, relative: &Path) -> Result<PathBuf, Refusal> {
        if relative.as_os_str().is_empty() || relative.is_absolute() {
            return Err(Refusal::ChildEscapesRoot);
        }
        if fs::canonicalize(&self.path)? != self.path {
            return Err(Refusal::ChildEscapesRoot);
        }
        let mut out = self.path.clone();
        let mut any = false;
        for component in relative.components() {
            match component {
                Component::Normal(name) => {
                    out.push(name);
                    match fs::symlink_metadata(&out) {
                        Ok(_) => {
                            if !fs::canonicalize(&out)?.starts_with(&self.path) {
                                return Err(Refusal::ChildEscapesRoot);
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                    any = true;
                }
                Component::CurDir => {}
                _ => return Err(Refusal::ChildEscapesRoot),
            }
        }
        if any {
            Ok(out)
        } else {
            Err(Refusal::ChildEscapesRoot)
        }
    }
}

/// How a run was decided. `Release` means neither activation nor a root was supplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Release,
    Test(ValidatedRoot),
}

impl Decision {
    pub fn is_test(&self) -> bool {
        matches!(self, Decision::Test(_))
    }

    pub fn test_root(&self) -> Option<&ValidatedRoot> {
        match self {
            Decision::Test(root) => Some(root),
            Decision::Release => None,
        }
    }
}

/// Everything [`decide`] needs, supplied by the host. `protected_paths` are the real per-user
/// locations the root may never overlap (at minimum the real suite/data roots); an empty list
/// cannot prove disjointness and refuses with [`Refusal::UserRootUnknown`].
#[derive(Debug, Clone)]
pub struct Request<'a> {
    pub app_id: &'a str,
    pub activated: bool,
    pub root: Option<&'a Path>,
    pub protected_paths: &'a [PathBuf],
}

/// Decide a run from an already-read activation/root pair. Reads no environment itself.
pub fn decide(request: &Request<'_>) -> Result<Decision, Refusal> {
    match (request.activated, request.root) {
        (false, None) => Ok(Decision::Release),
        (false, Some(_)) => Err(Refusal::RootWithoutActivation),
        (true, None) => Err(Refusal::ActivationWithoutRoot),
        (true, Some(root)) => {
            validate_root(request.app_id, root, request.protected_paths).map(Decision::Test)
        }
    }
}

/// Validate `root` as the bound test home for `app_id`.
pub fn validate_root(
    app_id: &str,
    root: &Path,
    protected_paths: &[PathBuf],
) -> Result<ValidatedRoot, Refusal> {
    if app_id.trim().is_empty() {
        return Err(Refusal::RootNotCreatedByRunner);
    }
    // Verbatim Windows paths (`\\?\`) keep `..` as a literal component, so match the text too.
    let parent_dir = |c: &Component<'_>| matches!(c, Component::ParentDir) || c.as_os_str() == "..";
    if !root.is_absolute() || root.components().any(|c| parent_dir(&c)) {
        return Err(Refusal::RootNotAbsolute);
    }
    let md = fs::symlink_metadata(root).map_err(|_| Refusal::RootNotDirectory)?;
    if md.file_type().is_symlink() {
        return Err(Refusal::RootHasSymlink);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if md.file_attributes() & 0x400 != 0 {
            return Err(Refusal::RootHasSymlink);
        }
    }
    if !md.is_dir() {
        return Err(Refusal::RootNotDirectory);
    }
    let canon = fs::canonicalize(root)?;

    if protected_paths.is_empty() {
        return Err(Refusal::UserRootUnknown);
    }
    for protected in protected_paths {
        if overlaps(&canon, &resolve(protected)) {
            return Err(Refusal::RootOverlapsUserData);
        }
    }
    if on_cloud_folder(&canon) {
        return Err(Refusal::RootOnRemovableOrCloud);
    }
    crate::private::verify_owner_only(&canon)?;
    check_binding(app_id, &canon)?;

    Ok(ValidatedRoot {
        path: canon,
        app_id: app_id.to_string(),
    })
}

/// Longest existing ancestor canonicalised, remainder appended lexically.
fn resolve(path: &Path) -> PathBuf {
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    loop {
        if let Ok(canon) = fs::canonicalize(&cur) {
            let mut out = canon;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (cur.file_name().map(|n| n.to_os_string()), cur.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                cur = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
}

fn overlaps(root: &Path, protected: &Path) -> bool {
    root.starts_with(protected) || protected.starts_with(root)
}

fn on_cloud_folder(path: &Path) -> bool {
    path.components().any(|c| match c {
        Component::Normal(name) => {
            let n = name.to_string_lossy().to_lowercase();
            n.starts_with("onedrive")
                || n == "dropbox"
                || n == "icloud drive"
                || n == "mobile documents"
                || n == "google drive"
                || n == "googledrive"
        }
        _ => false,
    })
}

fn check_binding(app_id: &str, canon: &Path) -> Result<(), Refusal> {
    let bytes = fs::read(canon.join(BINDING_FILE)).map_err(|_| Refusal::RootNotCreatedByRunner)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| Refusal::RootNotCreatedByRunner)?;
    match binding_app(text) {
        None => Err(Refusal::RootNotCreatedByRunner),
        Some(bound) if bound == app_id => Ok(()),
        Some(_) => Err(Refusal::BindingAppMismatch),
    }
}

/// Parse a complete top-level runner binding, including optional runner metadata. Duplicate
/// fields, nested identities and trailing bytes fail closed; extra metadata is allowed.
fn binding_app(text: &str) -> Option<String> {
    struct Binding(String);
    impl<'de> serde::Deserialize<'de> for Binding {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct Visitor;
            impl<'de> serde::de::Visitor<'de> for Visitor {
                type Value = Binding;
                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str("a top-level runner binding object")
                }
                fn visit_map<M: serde::de::MapAccess<'de>>(
                    self,
                    mut map: M,
                ) -> Result<Binding, M::Error> {
                    let mut seen = std::collections::BTreeSet::new();
                    let mut app: Option<String> = None;
                    while let Some(key) = map.next_key::<String>()? {
                        if !seen.insert(key.clone()) {
                            return Err(serde::de::Error::custom("duplicate binding field"));
                        }
                        match key.as_str() {
                            "app" => app = Some(map.next_value()?),
                            "runRoot" | "runId" => {
                                let _: String = map.next_value()?;
                            }
                            _ => {
                                let _: serde::de::IgnoredAny = map.next_value()?;
                            }
                        }
                    }
                    match app {
                        Some(app) if !app.trim().is_empty() => Ok(Binding(app)),
                        _ => Err(serde::de::Error::custom("missing binding app")),
                    }
                }
            }
            deserializer.deserialize_map(Visitor)
        }
    }
    serde_json::from_str::<Binding>(text)
        .ok()
        .map(|binding| binding.0)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn tmp(tag: &str) -> PathBuf {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("rkfs-test-mode-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        make_private(&dir);
        dir
    }

    fn make_private(dir: &Path) {
        crate::private::ensure_private_dir(dir).unwrap();
    }

    fn bound(dir: &Path, app: &str) {
        fs::write(dir.join(BINDING_FILE), format!("{{\"app\": \"{app}\"}}")).unwrap();
    }

    #[test]
    fn partial_pairing_refuses_and_neither_is_release() {
        let root = tmp("pairing");
        let protected = vec![tmp("pairing-real")];
        let base = Request {
            app_id: "viewright",
            activated: false,
            root: None,
            protected_paths: &protected,
        };
        assert_eq!(decide(&base).unwrap(), Decision::Release);
        assert_eq!(
            decide(&Request {
                activated: true,
                ..base.clone()
            }),
            Err(Refusal::ActivationWithoutRoot)
        );
        assert_eq!(
            decide(&Request {
                root: Some(&root),
                ..base
            }),
            Err(Refusal::RootWithoutActivation)
        );
    }

    #[test]
    fn binding_requires_complete_strict_top_level_json() {
        for invalid in [
            r#"junk "app":"viewright""#,
            r#"{"nested":{"app":"viewright"}}"#,
            r#"{"app":"viewright""#,
            r#"{"app":"viewright"} trailing"#,
            r#"{"app":"viewright","app":"viewright"}"#,
            r#"{"app":"viewright","extra":1,"extra":2}"#,
            r#"[{"app":"viewright"}]"#,
            r#"["viewright", "", ""]"#,
            r#"{"app":"viewright","runRoot":{}}"#,
        ] {
            assert_eq!(binding_app(invalid), None, "{invalid}");
        }
        assert_eq!(
            binding_app(r#"{"app":"viewright"}"#).as_deref(),
            Some("viewright")
        );
        assert_eq!(
            binding_app(r#"{"app":"viewright","runRoot":"/run","runId":"r"}"#).as_deref(),
            Some("viewright")
        );
    }

    #[cfg(unix)]
    #[test]
    fn protected_alias_is_canonicalized_before_overlap_check() {
        let root = tmp("alias");
        bound(&root, "viewright");
        let parent = tmp("alias-parent");
        let link = parent.join("alias");
        std::os::unix::fs::symlink(&root, &link).unwrap();
        assert_eq!(
            validate_root("viewright", &root, &[link.join("future")]),
            Err(Refusal::RootOverlapsUserData)
        );
    }

    #[test]
    fn bound_private_disjoint_root_validates_and_derives_children() {
        let root = tmp("ok");
        bound(&root, "viewright");
        let protected = vec![tmp("ok-real")];
        let got = validate_root("viewright", &root, &protected).unwrap();
        assert_eq!(got.path(), fs::canonicalize(&root).unwrap().as_path());
        assert_eq!(got.app_id(), "viewright");
        assert_eq!(
            got.child(Path::new("shared")).unwrap(),
            got.path().join("shared")
        );
        assert_eq!(
            got.child(Path::new("../outside")),
            Err(Refusal::ChildEscapesRoot)
        );
        assert_eq!(
            got.child(&root),
            Err(Refusal::ChildEscapesRoot),
            "an absolute child is not a derivation"
        );
    }

    #[cfg(unix)]
    #[test]
    fn child_refuses_external_symlink_and_missing_suffix_but_accepts_internal_alias() {
        let root = tmp("child-links");
        let outside = tmp("child-outside");
        bound(&root, "viewright");
        let validated = validate_root("viewright", &root, std::slice::from_ref(&outside)).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("data")).unwrap();
        for child in ["data", "data/missing/file"] {
            assert_eq!(
                validated.child(Path::new(child)),
                Err(Refusal::ChildEscapesRoot)
            );
        }
        fs::remove_file(root.join("data")).unwrap();
        fs::create_dir(root.join("inside")).unwrap();
        std::os::unix::fs::symlink(root.join("inside"), root.join("data")).unwrap();
        assert!(validated.child(Path::new("data/missing/file")).is_ok());
        fs::write(root.join("regular"), b"file").unwrap();
        assert!(validated.child(Path::new("regular/child")).is_err());
        std::os::unix::fs::symlink(outside.join("missing"), root.join("dangling")).unwrap();
        assert!(validated.child(Path::new("dangling/file")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn child_refuses_external_junction_and_missing_suffix() {
        use std::ffi::c_void;
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn DeviceIoControl(
                handle: *mut c_void,
                code: u32,
                input: *const c_void,
                input_len: u32,
                output: *mut c_void,
                output_len: u32,
                returned: *mut u32,
                overlapped: *mut c_void,
            ) -> i32;
        }
        let root = tmp("child-junction");
        let outside = tmp("child-junction-outside");
        bound(&root, "viewright");
        let validated = validate_root("viewright", &root, &[outside.clone()]).unwrap();
        let junction = root.join("data");
        fs::create_dir(&junction).unwrap();
        let target = fs::canonicalize(&outside)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let plain = target.strip_prefix(r"\\?\").unwrap_or(&target);
        let substitute: Vec<u16> = format!(r"\??\{plain}").encode_utf16().collect();
        let print: Vec<u16> = plain.encode_utf16().collect();
        let path_bytes = (substitute.len() + print.len() + 2) * 2;
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&0xA0000003u32.to_le_bytes()); // IO_REPARSE_TAG_MOUNT_POINT
        buffer.extend_from_slice(&((8 + path_bytes) as u16).to_le_bytes());
        buffer.extend_from_slice(&0u16.to_le_bytes());
        for field in [
            0,
            (substitute.len() * 2) as u16,
            ((substitute.len() + 1) * 2) as u16,
            (print.len() * 2) as u16,
        ] {
            buffer.extend_from_slice(&field.to_le_bytes());
        }
        for word in substitute.into_iter().chain([0]).chain(print).chain([0]) {
            buffer.extend_from_slice(&word.to_le_bytes());
        }
        let handle = fs::OpenOptions::new()
            .access_mode(0x40000000)
            .share_mode(7)
            .custom_flags(0x02200000)
            .open(&junction)
            .unwrap();
        let mut returned = 0;
        // SAFETY: live directory handle & initialized mount-point reparse buffer; no output.
        let ok = unsafe {
            DeviceIoControl(
                handle.as_raw_handle().cast(),
                0x000900A4,
                buffer.as_ptr().cast(),
                buffer.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(ok, 0, "{}", io::Error::last_os_error());
        drop(handle);
        for child in ["data", "data/missing/file"] {
            assert_eq!(
                validated.child(Path::new(child)),
                Err(Refusal::ChildEscapesRoot)
            );
        }
        fs::remove_dir(&junction).unwrap();
    }

    #[test]
    fn wrong_or_missing_binding_and_overlap_refuse() {
        let root = tmp("binding");
        let protected = vec![tmp("binding-real")];
        assert_eq!(
            validate_root("viewright", &root, &protected),
            Err(Refusal::RootNotCreatedByRunner)
        );
        bound(&root, "genright");
        assert_eq!(
            validate_root("viewright", &root, &protected),
            Err(Refusal::BindingAppMismatch)
        );
        bound(&root, "viewright");
        let ancestor = vec![root.join("nested-real")];
        assert_eq!(
            validate_root("viewright", &root, &ancestor),
            Err(Refusal::RootOverlapsUserData),
            "a root that is an ancestor of a protected path overlaps it"
        );
        assert_eq!(
            validate_root("viewright", &root, &[]),
            Err(Refusal::UserRootUnknown)
        );
        assert_eq!(
            validate_root("viewright", Path::new("relative/home"), &protected),
            Err(Refusal::RootNotAbsolute)
        );
    }

    #[test]
    fn cloud_folder_refuses() {
        let root = tmp("cloud").join("OneDrive").join("home");
        fs::create_dir_all(&root).unwrap();
        make_private(&root);
        bound(&root, "viewright");
        let protected = vec![tmp("cloud-real")];
        assert_eq!(
            validate_root("viewright", &root, &protected),
            Err(Refusal::RootOnRemovableOrCloud)
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_root_and_group_readable_root_refuse() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp("privacy");
        bound(&root, "viewright");
        let protected = vec![tmp("privacy-real")];
        let link = root.with_file_name(format!("rkfs-test-mode-link-{}", std::process::id()));
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(&root, &link).unwrap();
        assert_eq!(
            validate_root("viewright", &link, &protected),
            Err(Refusal::RootHasSymlink)
        );
        fs::set_permissions(&root, fs::Permissions::from_mode(0o750)).unwrap();
        assert_eq!(
            validate_root("viewright", &root, &protected),
            Err(Refusal::RootNotPrivate)
        );
    }
}
