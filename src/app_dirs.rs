//! One resolver for an app's per-user folders (`data`, `local`, `cache`, `home`) with a dev
//! namespace and test/QA isolation.
//!
//! This replaces the hand-rolled `app_paths.rs` / `*_DATA_DIR` copies in each app. The rules:
//!
//! * **Per-OS bases.** Windows: the Roaming AppData known folder (data) and the Local AppData known
//!   folder (local, cache), home from the Profile known folder, each asked of `SHGetKnownFolderPath`
//!   by [`AppDirs::resolve`] (environment redirection of `%APPDATA%`, `%LOCALAPPDATA%` and
//!   `%USERPROFILE%` is ignored, as the `dirs` crate does), with the variable as fallback only when
//!   the call fails. [`AppDirs::resolve_with`] uses supplied environment for ordinary mappings;
//!   canonical QA overrides also validate disk state & trusted protected locations. macOS: `~/Library/Application Support` (data and local) and
//!   `~/Library/Caches` (cache). Linux: `$XDG_DATA_HOME` (else `~/.local/share`) for data and
//!   local, `$XDG_CACHE_HOME` (else `~/.cache`) for cache. Every base must be absolute; an unset or
//!   relative base is an error, never the working directory.
//! * **Config folder.** [`AppDirs::config`] is where the platform keeps per-user settings, apart
//!   from data where it has a separate place: Linux `$XDG_CONFIG_HOME` (else `~/.config`); on
//!   Windows (Roaming AppData) and macOS (`~/Library/Application Support`) the platform has no
//!   separate base, so `config` is the same folder as `data`. A Linux process with no `HOME` and no
//!   `XDG_CONFIG_HOME` gets `config` next to `data` rather than a new failure. Android: the
//!   injected files folder. Under a test root or a QA override `config` is `<root>/config`.
//! * **Legacy names.** [`AppId::with_legacy_names`] lists earlier app names whose folders must not
//!   be abandoned. [`AppDirs::resolve`] (and [`AppDirs::resolve_with_legacy`], which takes the
//!   existence test as a parameter) picks, for each of `data`, `local` and `config` separately,
//!   the current name's folder if it exists, else the first legacy name's folder that exists, else
//!   the current name. `cache` always uses the current name (it is re-creatable). Nothing is moved
//!   or created. Test roots, QA overrides and injected folders ignore legacy names.
//! * **App folder.** `<base>/<org>/<namespace>`, org defaulting to `Orthic Labs`
//!   ([`AppId::without_org`] keeps an existing flat `<base>/<name>` install). Windows cache is
//!   `<LOCALAPPDATA>/<org>/<namespace>/Cache` so it never shares a folder with state.
//! * **Dev namespace.** A dev build (`cfg!(debug_assertions)` of this crate's build, see
//!   [`Options::current`]) uses `<name>Dev` (for example `ViewRightDev`), so a dev run never touches
//!   the installed app's data.
//! * **Test root.** Under this crate's own `cfg(test)`, and from [`AppDirs::for_test`] in any build,
//!   every folder is under one per-process temp root and the OS is never asked.
//! * **QA override (feature `test-mode`).** Only a build that compiles `test-mode` reads any
//!   override variable (the same feature name as `rightkit-daemon`, so one switch isolates both).
//!   `RIGHTKIT_QA_DATA_DIR` names the bound QA home itself (the root carrying
//!   `.rightkit-app.json`); the data folder is `<root>/data`, with `local`, `cache`, `config`
//!   under that data folder. The default `home` is the validated root itself; `RIGHTKIT_QA_HOME`
//!   may name another independently validated bound home. The canonical root requires a truthy `RIGHTKIT_UNDER_TEST` alongside it, else
//!   [`AppDirsError::UnpairedTestRoot`]. An app may name its own historical variables with
//!   [`AppId::with_legacy_env`]; a legacy data variable keeps direct data-folder semantics.
//!   The override is **refused** (an error, not a fallback) when it is
//!   relative, contains `..`, or points at or inside a real per-user app folder of this app.
//!   A release build (without `test-mode`) contains none of these variable names and reads none.
//!
//! * **Android.** There is no environment-based resolution on Android: the app gets its folders
//!   from the `Context` (`getFilesDir()`, `getCacheDir()`) and passes them in with
//!   [`AppDirs::injected`]. [`AppDirs::resolve`] (and `resolve_with` for [`Os::Android`]) return
//!   [`AppDirsError::AndroidNeedsInjectedRoots`] instead of guessing from `HOME`. Those folders are
//!   already private to the package and already separate per `applicationId` (a dev build uses its
//!   own id), so no org or `Dev` namespace is added. The injected constructor is compiled on every
//!   target, so host tests cover it.
//!
//! Normal mappings are pure over an injected environment; canonical QA roots additionally use
//! filesystem validation with trusted native locations or [`Options::protected_paths`].
//! Native fallback uses account/known-folder locations, ignoring redirected HOME/XDG/APPDATA.
//! Hosts with custom real XDG locations must supply those through [`Options::protected_paths`].
//! Nothing here creates a directory except [`AppDirs::create_all`].
//!
//! Historical selectors retain lexical checks against supplied bases. Canonical roots use the
//! portable validation contract and trusted locations independent of redirected HOME/APPDATA.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

/// The organisation folder every Right Suite app nests under.
pub const DEFAULT_ORG: &str = "Orthic Labs";

/// The data-folder override variable, read only with feature `test-mode`.
#[cfg(feature = "test-mode")]
pub const QA_DATA_DIR_VAR: &str = "RIGHTKIT_QA_DATA_DIR";
/// The home-folder override variable, read only with feature `test-mode`.
#[cfg(feature = "test-mode")]
pub const QA_HOME_VAR: &str = "RIGHTKIT_QA_HOME";
/// Activation variable that must accompany [`QA_DATA_DIR_VAR`], read only with `test-mode`.
#[cfg(feature = "test-mode")]
const UNDER_TEST_VAR: &str = "RIGHTKIT_UNDER_TEST";

/// Why folders could not be resolved.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AppDirsError {
    #[cfg(feature = "test-mode")]
    #[error("{0}")]
    TestMode(#[from] crate::test_mode::Refusal),
    /// A base folder the OS mapping needs is unset or empty.
    #[error("cannot resolve the {what} folder: {why}")]
    NoBase { what: &'static str, why: String },
    /// A base or override is relative or contains `..`.
    #[error("{var} must be an absolute path without `..`: {}", .path.display())]
    NotAbsolute { var: String, path: PathBuf },
    /// An override points at (or inside) the real per-user folder; it is refused, not honoured.
    #[error("{var} points at the real per-user folder {}; refusing", .path.display())]
    RealFolder { var: String, path: PathBuf },
    /// The canonical QA root is set without its activation pairing; refused, not honoured.
    #[error("{var} is set without test activation; refusing unpaired test root {}", .path.display())]
    UnpairedTestRoot { var: String, path: PathBuf },
    /// Android has no environment-based mapping; the folders come from the `Context`.
    #[error("Android has no environment-based folder resolution: pass the Context's filesDir and cacheDir to AppDirs::injected")]
    AndroidNeedsInjectedRoots,
    /// An injected root is unusable (relative, `..`, empty, or the two roots overlap).
    #[error("injected {what} folder is unusable: {why}")]
    BadInjectedRoot { what: &'static str, why: String },
    #[error("{0}")]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, AppDirsError>;

/// The operating system whose folder mapping is used. A parameter so each mapping is testable on
/// any host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Windows,
    MacOs,
    Linux,
    /// No environment mapping exists; folders are injected ([`AppDirs::injected`]).
    Android,
}

impl Os {
    /// The OS this build targets (anything that is neither Windows, Android nor macOS maps as
    /// Linux/XDG).
    pub fn current() -> Os {
        if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "android") {
            Os::Android
        } else if cfg!(target_os = "macos") {
            Os::MacOs
        } else {
            Os::Linux
        }
    }
}

/// Which app. Build with [`AppId::new`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct AppId {
    /// Runner binding identity when it differs from the installed folder name. Matching is exact.
    pub qa_app_id: Option<&'static str>,
    /// The installed folder name, for example `ViewRight`.
    pub name: &'static str,
    /// The organisation folder; `None` keeps a flat `<base>/<name>` layout.
    pub org: Option<&'static str>,
    /// Historical per-app data override variable (for example `HR_APP_DATA_DIR`), read only with
    /// feature `test-mode`, after `RIGHTKIT_QA_DATA_DIR`.
    pub legacy_data_env: Option<&'static str>,
    /// Historical per-app home override variable, read only with feature `test-mode`.
    pub legacy_home_env: Option<&'static str>,
    /// Earlier app names whose existing folders are still used, in order of preference (for
    /// example `&["LightCraft"]` for an app renamed to `Ember`). See [`AppId::with_legacy_names`].
    pub legacy_names: &'static [&'static str],
}

impl AppId {
    /// `name` under [`DEFAULT_ORG`].
    pub const fn new(name: &'static str) -> AppId {
        AppId {
            qa_app_id: None,
            name,
            org: Some(DEFAULT_ORG),
            legacy_data_env: None,
            legacy_home_env: None,
            legacy_names: &[],
        }
    }

    /// Use the runner's canonical app key for strict QA binding validation.
    pub const fn with_qa_app_id(mut self, id: &'static str) -> AppId {
        self.qa_app_id = Some(id);
        self
    }

    /// Earlier names of this app, most preferred first. A folder under one of them that already
    /// exists is used instead of a missing folder under the current name, so a renamed app keeps
    /// its users' settings and data where they are. Applied to `data`, `local` and `config` (each
    /// on its own) by [`AppDirs::resolve`] and [`AppDirs::resolve_with_legacy`]; the dev namespace
    /// is applied to each legacy name too.
    pub const fn with_legacy_names(mut self, names: &'static [&'static str]) -> AppId {
        self.legacy_names = names;
        self
    }

    /// A different organisation folder.
    pub const fn with_org(mut self, org: &'static str) -> AppId {
        self.org = Some(org);
        self
    }

    /// No organisation folder: `<base>/<name>`, for an app whose installed data predates the org
    /// layout.
    pub const fn without_org(mut self) -> AppId {
        self.org = None;
        self
    }

    /// Also honour these historical override variables (test-mode builds only).
    pub const fn with_legacy_env(
        mut self,
        data: Option<&'static str>,
        home: Option<&'static str>,
    ) -> AppId {
        self.legacy_data_env = data;
        self.legacy_home_env = home;
        self
    }

    /// The folder-name component: `name`, or `<name>Dev` for a dev build.
    pub fn namespace(&self, dev: bool) -> String {
        if dev {
            format!("{}Dev", self.name)
        } else {
            self.name.to_string()
        }
    }
}

/// The inputs of a resolution other than the app and the environment.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Options {
    /// Trusted real locations for QA root validation. `None` uses native user locations,
    /// ignoring QA-overridden environment variables. Hosts may supply their own trusted context.
    pub protected_paths: Option<Vec<PathBuf>>,
    pub os: Os,
    /// Use the `<name>Dev` namespace.
    pub dev: bool,
    /// Read the QA override variables. Has an effect only when feature `test-mode` is compiled.
    pub qa_overrides: bool,
    /// When set, every folder is under this root and the OS and environment are not consulted.
    pub test_root: Option<PathBuf>,
}

impl Options {
    /// A release-shaped resolution for `os`: not dev, no overrides, no test root.
    pub fn new(os: Os) -> Options {
        Options {
            protected_paths: None,
            os,
            dev: false,
            qa_overrides: false,
            test_root: None,
        }
    }

    /// This build: the target OS, dev when `debug_assertions` are on, overrides read only when
    /// feature `test-mode` is compiled, and the per-process test root under `cfg(test)`.
    pub fn current() -> Options {
        Options {
            protected_paths: None,
            os: Os::current(),
            dev: cfg!(debug_assertions),
            qa_overrides: cfg!(feature = "test-mode"),
            test_root: if cfg!(test) {
                Some(process_test_root())
            } else {
                None
            },
        }
    }

    pub fn dev(mut self, dev: bool) -> Options {
        self.dev = dev;
        self
    }

    pub fn protected_paths(mut self, paths: Vec<PathBuf>) -> Options {
        self.protected_paths = Some(paths);
        self
    }

    pub fn qa_overrides(mut self, on: bool) -> Options {
        self.qa_overrides = on;
        self
    }

    pub fn test_root(mut self, root: Option<PathBuf>) -> Options {
        self.test_root = root;
        self
    }
}

/// The per-process temp root every test resolution uses: `<temp>/rightkit-test-<pid>`.
pub fn process_test_root() -> PathBuf {
    std::env::temp_dir().join(format!("rightkit-test-{}", std::process::id()))
}

/// An app's resolved folders.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct AppDirs {
    /// Roaming data (databases, settings).
    pub data: PathBuf,
    /// Per-user configuration. A separate folder from `data` where the platform has one (Linux
    /// `$XDG_CONFIG_HOME`); the same folder as `data` on Windows and macOS, and on Android.
    pub config: PathBuf,
    /// Machine-local data (logs, WebView profile, large artifacts).
    pub local: PathBuf,
    /// Re-creatable cache.
    pub cache: PathBuf,
    /// The home folder to show or search (the user's real home unless isolated).
    pub home: PathBuf,
    /// True when any folder came from the test root or a QA override, so nothing is the real
    /// per-user folder.
    pub isolated: bool,
}

impl AppDirs {
    /// Resolve for this build and this process's environment.
    pub fn resolve(app: AppId) -> Result<AppDirs> {
        AppDirs::resolve_with_legacy(&app, &Options::current(), &process_env, &|p| p.is_dir())
    }

    /// [`AppDirs::resolve`] with explicit [`Options`] instead of [`Options::current`]: `dev(false)`
    /// gives the release namespace from a dev build, and the namespace follows `opts.dev` alone.
    /// The base folders are read as in [`AppDirs::resolve`] (on Windows the known-folder API first,
    /// then the environment variable), and legacy names apply when their folder already exists.
    /// On Windows the known-folder lookup is the build's own, so `opts.os` should name the build's
    /// OS for the real per-user folders; `opts.test_root` and the QA overrides behave as documented
    /// on [`Options`].
    pub fn resolve_with_options(app: AppId, opts: &Options) -> Result<AppDirs> {
        AppDirs::resolve_with_legacy(&app, opts, &process_env, &|p| p.is_dir())
    }

    /// Per-process temp folders in any build (consumer tests): never asks the OS.
    pub fn for_test(app: AppId) -> Result<AppDirs> {
        let opts = Options::current().test_root(Some(process_test_root()));
        AppDirs::resolve_with(&app, &opts, &|_| None)
    }

    /// `env` answers variable lookups. Canonical QA overrides require filesystem validation;
    /// legacy folder discovery is disabled here; use [`AppDirs::resolve_with_legacy`] for that.
    pub fn resolve_with(
        app: &AppId,
        opts: &Options,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> Result<AppDirs> {
        AppDirs::resolve_with_legacy(app, opts, env, &|_| false)
    }

    /// [`AppDirs::resolve_with`] that also applies [`AppId::legacy_names`]: `exists` answers
    /// whether a candidate folder is already there ([`AppDirs::resolve`] passes `Path::is_dir`).
    /// Pure when `exists` is.
    pub fn resolve_with_legacy(
        app: &AppId,
        opts: &Options,
        env: &dyn Fn(&str) -> Option<OsString>,
        exists: &dyn Fn(&Path) -> bool,
    ) -> Result<AppDirs> {
        let ns = app.namespace(opts.dev);
        if let Some(root) = &opts.test_root {
            check_absolute(opts.os, "test root", root)?;
            let r = root.join(&ns);
            return Ok(AppDirs {
                data: r.join("data"),
                config: r.join("config"),
                local: r.join("local"),
                cache: r.join("cache"),
                home: r.join("home"),
                isolated: true,
            });
        }

        if opts.os == Os::Android {
            return Err(AppDirsError::AndroidNeedsInjectedRoots);
        }

        let bases = Bases::read(opts.os, env);
        let (qa_data, qa_home) = if opts.qa_overrides {
            read_qa(app, opts, env, &bases)?
        } else {
            (None, None)
        };

        if let Some(root) = qa_data {
            let home = qa_home.unwrap_or_else(|| root.join("home"));
            return Ok(AppDirs {
                local: root.join("local"),
                cache: root.join("cache"),
                config: root.join("config"),
                data: root,
                home,
                isolated: true,
            });
        }

        let dirs = real_dirs(app, opts, &ns, env, exists)?;
        Ok(match qa_home {
            Some(home) => AppDirs {
                home,
                isolated: true,
                ..dirs
            },
            None => dirs,
        })
    }

    /// Folders handed in by the host app (Android: `Context.getFilesDir()` and
    /// `Context.getCacheDir()`), for targets with no environment mapping. `data` and `local` are
    /// `files_dir` (as on macOS, where the two are one folder), `cache` is `cache_dir`, and `home`
    /// is `files_dir` too: Android has no user home to show. Nothing is appended (no org, no `Dev`
    /// namespace): the OS already gives each package its own private folders. `isolated` is false.
    ///
    /// Both must be absolute and free of `..`, and neither may be the other or inside it (the OS
    /// may clear the cache folder, which must never be able to take state with it). Pure: touches
    /// no file and reads no environment, and it is compiled on every target.
    pub fn injected(
        files_dir: impl Into<PathBuf>,
        cache_dir: impl Into<PathBuf>,
    ) -> Result<AppDirs> {
        AppDirs::injected_for(Os::current(), files_dir.into(), cache_dir.into())
    }

    /// [`AppDirs::injected`] with the path syntax of `os` named, so each syntax is testable on any
    /// host.
    pub fn injected_for(os: Os, files_dir: PathBuf, cache_dir: PathBuf) -> Result<AppDirs> {
        let bad = |what: &'static str, why: &str| AppDirsError::BadInjectedRoot {
            what,
            why: why.to_string(),
        };
        for (what, path) in [("files", &files_dir), ("cache", &cache_dir)] {
            if path.as_os_str().is_empty() {
                return Err(bad(what, "empty"));
            }
            if !looks_absolute(os, path) {
                return Err(bad(what, "must be an absolute path without `..`"));
            }
        }
        let win = os == Os::Windows;
        if inside_or_equal(&files_dir, &cache_dir, win)
            || inside_or_equal(&cache_dir, &files_dir, win)
        {
            return Err(bad("cache", "must not equal or nest with the files folder"));
        }
        Ok(AppDirs {
            data: files_dir.clone(),
            config: files_dir.clone(),
            local: files_dir.clone(),
            cache: cache_dir,
            home: files_dir,
            isolated: false,
        })
    }

    /// Move the directory `from` to `to` (for example an old data folder to the current one) so
    /// that `to` is never visible half-migrated: one rename on the same volume, otherwise a copy to
    /// a hidden sibling of `to`, a rename of the sibling to `to`, then removal of `from`. `to` must
    /// be absent or an empty directory; a populated one is refused with
    /// [`FsError::AlreadyExists`](crate::FsError::AlreadyExists) and nothing is written. See
    /// [`crate::migrate`] for the full rules.
    pub fn migrate_dir(from: &Path, to: &Path) -> crate::error::Result<crate::migrate::Migrated> {
        crate::migrate::migrate_dir(from, to)
    }

    /// Create the folders (plain `create_dir_all`; use `ensure_private_dir` for private state).
    pub fn create_all(&self) -> io::Result<()> {
        for d in [
            &self.data,
            &self.config,
            &self.local,
            &self.cache,
            &self.home,
        ] {
            std::fs::create_dir_all(d)?;
        }
        Ok(())
    }
}

/// The OS base folders, each `None` when unset, empty or not absolute.
struct Bases {
    data: Option<PathBuf>,
    config: Option<PathBuf>,
    local: Option<PathBuf>,
    cache: Option<PathBuf>,
    home: Option<PathBuf>,
}

/// The environment [`AppDirs::resolve`] reads. On Windows the three base variables come from the
/// known-folder API first, so a redirected `%APPDATA%`, `%LOCALAPPDATA%` or `%USERPROFILE%` does not
/// move the real folders; the variable answers only when the API fails. Elsewhere it is the process
/// environment.
fn process_env(key: &str) -> Option<OsString> {
    #[cfg(windows)]
    {
        let id = match key {
            "APPDATA" => Some(&known::ROAMING_APP_DATA),
            "LOCALAPPDATA" => Some(&known::LOCAL_APP_DATA),
            "USERPROFILE" => Some(&known::PROFILE),
            _ => None,
        };
        if let Some(path) = id.and_then(known::known_folder) {
            return Some(path.into_os_string());
        }
    }
    std::env::var_os(key)
}

#[cfg(windows)]
mod known {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;

    #[repr(C)]
    pub(super) struct Guid {
        data1: u32,
        data2: u16,
        data3: u16,
        data4: [u8; 8],
    }

    /// `FOLDERID_RoamingAppData`.
    pub(super) static ROAMING_APP_DATA: Guid = Guid {
        data1: 0x3EB6_85DB,
        data2: 0x65F9,
        data3: 0x4CF6,
        data4: [0xA0, 0x3A, 0xE3, 0xEF, 0x65, 0x72, 0x9F, 0x3D],
    };
    /// `FOLDERID_LocalAppData`.
    pub(super) static LOCAL_APP_DATA: Guid = Guid {
        data1: 0xF1B3_2785,
        data2: 0x6FBA,
        data3: 0x4FCF,
        data4: [0x9D, 0x55, 0x7B, 0x8E, 0x7F, 0x15, 0x70, 0x91],
    };
    /// `FOLDERID_Profile`.
    pub(super) static PROFILE: Guid = Guid {
        data1: 0x5E6C_858F,
        data2: 0x0E22,
        data3: 0x4760,
        data4: [0x9A, 0xFE, 0xEA, 0x33, 0x17, 0xB6, 0x71, 0x73],
    };

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn SHGetKnownFolderPath(
            rfid: *const Guid,
            flags: u32,
            token: *mut c_void,
            path: *mut *mut u16,
        ) -> i32;
    }

    #[link(name = "ole32")]
    unsafe extern "system" {
        fn CoTaskMemFree(pv: *mut c_void);
    }

    /// `KF_FLAG_DEFAULT`: no creation, no verification, no redirection override.
    const KF_FLAG_DEFAULT: u32 = 0;

    /// The known folder `id` of the current user, or `None` when the call fails or returns an
    /// empty string. The returned buffer is always freed with `CoTaskMemFree`, also on failure
    /// (the API may set it even then).
    pub(super) fn known_folder(id: &Guid) -> Option<PathBuf> {
        let mut raw: *mut u16 = std::ptr::null_mut();
        // SAFETY: `id` is a valid GUID, a null token means the current user, and `raw` is a valid
        // out pointer. On return `raw` is null or a NUL-terminated UTF-16 string we own.
        let hr =
            unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, std::ptr::null_mut(), &mut raw) };
        if raw.is_null() {
            return None;
        }
        let path = if hr >= 0 {
            let mut len = 0usize;
            // SAFETY: `raw` is NUL-terminated, so every offset up to the terminator is readable.
            while unsafe { *raw.add(len) } != 0 {
                len += 1;
            }
            // SAFETY: `len` elements were just read from `raw`.
            let wide = unsafe { std::slice::from_raw_parts(raw, len) };
            Some(PathBuf::from(std::ffi::OsString::from_wide(wide)))
        } else {
            None
        };
        // SAFETY: `raw` came from SHGetKnownFolderPath and is freed exactly once.
        unsafe { CoTaskMemFree(raw.cast()) };
        path.filter(|p| !p.as_os_str().is_empty())
    }
}

fn nonempty(env: &dyn Fn(&str) -> Option<OsString>, key: &str) -> Option<PathBuf> {
    env(key)
        .filter(|v| !v.to_string_lossy().trim().is_empty())
        .map(PathBuf::from)
}

fn absolute_env(os: Os, env: &dyn Fn(&str) -> Option<OsString>, key: &str) -> Option<PathBuf> {
    nonempty(env, key).filter(|p| looks_absolute(os, p))
}

impl Bases {
    fn read(os: Os, env: &dyn Fn(&str) -> Option<OsString>) -> Bases {
        match os {
            Os::Windows => Bases {
                data: absolute_env(os, env, "APPDATA"),
                config: absolute_env(os, env, "APPDATA"),
                local: absolute_env(os, env, "LOCALAPPDATA"),
                cache: absolute_env(os, env, "LOCALAPPDATA"),
                home: absolute_env(os, env, "USERPROFILE"),
            },
            Os::MacOs => {
                let home = absolute_env(os, env, "HOME");
                let support = home.as_ref().map(|h| h.join("Library/Application Support"));
                Bases {
                    data: support.clone(),
                    config: support.clone(),
                    local: support,
                    cache: home.as_ref().map(|h| h.join("Library/Caches")),
                    home,
                }
            }
            Os::Android => Bases {
                data: None,
                config: None,
                local: None,
                cache: None,
                home: None,
            },
            Os::Linux => {
                let home = absolute_env(os, env, "HOME");
                let data = absolute_env(os, env, "XDG_DATA_HOME")
                    .or_else(|| home.as_ref().map(|h| h.join(".local/share")));
                let cache = absolute_env(os, env, "XDG_CACHE_HOME")
                    .or_else(|| home.as_ref().map(|h| h.join(".cache")));
                let config = absolute_env(os, env, "XDG_CONFIG_HOME")
                    .or_else(|| home.as_ref().map(|h| h.join(".config")))
                    .or_else(|| data.clone());
                Bases {
                    local: data.clone(),
                    config,
                    data,
                    cache,
                    home,
                }
            }
        }
    }
}

fn app_dir(base: &Path, app: &AppId, ns: &str) -> PathBuf {
    match app.org {
        Some(org) => base.join(org).join(ns),
        None => base.join(ns),
    }
}

fn missing(os: Os, what: &'static str) -> AppDirsError {
    let why = match os {
        Os::Windows => match what {
            "data" | "config" => "APPDATA is not set to an absolute path",
            "home" => "USERPROFILE is not set to an absolute path",
            _ => "LOCALAPPDATA is not set to an absolute path",
        },
        _ => "HOME is not set to an absolute path",
    };
    AppDirsError::NoBase {
        what,
        why: why.to_string(),
    }
}

/// `<base>/<org>/<ns>` for the current name, unless that folder is missing and a legacy name's
/// folder (same base, org and dev rule) exists: then the first such legacy folder.
fn pick_app_dir(
    base: &Path,
    app: &AppId,
    opts: &Options,
    ns: &str,
    exists: &dyn Fn(&Path) -> bool,
) -> PathBuf {
    let current = app_dir(base, app, ns);
    if exists(&current) {
        return current;
    }
    for legacy in app.legacy_names {
        let name = if opts.dev {
            format!("{legacy}Dev")
        } else {
            (*legacy).to_string()
        };
        let candidate = app_dir(base, app, &name);
        if exists(&candidate) {
            return candidate;
        }
    }
    current
}

fn real_dirs(
    app: &AppId,
    opts: &Options,
    ns: &str,
    env: &dyn Fn(&str) -> Option<OsString>,
    exists: &dyn Fn(&Path) -> bool,
) -> Result<AppDirs> {
    let os = opts.os;
    let b = Bases::read(os, env);
    let data = b.data.ok_or_else(|| missing(os, "data"))?;
    let config = b.config.ok_or_else(|| missing(os, "config"))?;
    let local = b.local.ok_or_else(|| missing(os, "local"))?;
    let cache = b.cache.ok_or_else(|| missing(os, "cache"))?;
    let home = b.home.ok_or_else(|| missing(os, "home"))?;
    let cache = if os == Os::Windows {
        app_dir(&cache, app, ns).join("Cache")
    } else {
        app_dir(&cache, app, ns)
    };
    Ok(AppDirs {
        data: pick_app_dir(&data, app, opts, ns, exists),
        config: pick_app_dir(&config, app, opts, ns, exists),
        local: pick_app_dir(&local, app, opts, ns, exists),
        cache,
        home,
        isolated: false,
    })
}

/// The QA data and home overrides, validated. Compiled to `(None, None)` without `test-mode`, so
/// the variable names are absent from a release binary.
#[cfg(not(feature = "test-mode"))]
fn read_qa(
    _app: &AppId,
    _opts: &Options,
    _env: &dyn Fn(&str) -> Option<OsString>,
    _bases: &Bases,
) -> Result<(Option<PathBuf>, Option<PathBuf>)> {
    Ok((None, None))
}

#[cfg(feature = "test-mode")]
fn read_qa(
    app: &AppId,
    opts: &Options,
    env: &dyn Fn(&str) -> Option<OsString>,
    bases: &Bases,
) -> Result<(Option<PathBuf>, Option<PathBuf>)> {
    let pick = |names: [Option<&'static str>; 2]| -> Option<(&'static str, PathBuf)> {
        names
            .into_iter()
            .flatten()
            .find_map(|n| nonempty(env, n).map(|p| (n, p)))
    };
    let canonical = nonempty(env, QA_DATA_DIR_VAR).map(|p| (QA_DATA_DIR_VAR, p));
    let legacy_data = app
        .legacy_data_env
        .and_then(|n| nonempty(env, n).map(|p| (n, p)));
    let home = pick([Some(QA_HOME_VAR), app.legacy_home_env]);
    let activated = env(UNDER_TEST_VAR).is_some_and(|v| {
        let text = v.to_string_lossy();
        let text = text.trim();
        !(text.is_empty() || text == "0" || text.eq_ignore_ascii_case("false"))
    });
    if activated && canonical.is_none() {
        return Err(crate::test_mode::Refusal::ActivationWithoutRoot.into());
    }
    if let Some((var, root)) = &canonical {
        if !activated {
            return Err(AppDirsError::UnpairedTestRoot {
                var: (*var).to_string(),
                path: root.clone(),
            });
        }
        check_absolute(opts.os, var, root)?;
        let protected = match &opts.protected_paths {
            Some(paths) => paths.clone(),
            None => native_protected_paths(app)?,
        };
        let id = app.qa_app_id.unwrap_or(app.name);
        let decision = crate::test_mode::decide(&crate::test_mode::Request {
            app_id: id,
            activated,
            root: Some(root),
            protected_paths: &protected,
        })?;
        let validated = decision
            .test_root()
            .expect("activated paired request yields test root");
        let data = validated.child(Path::new("data"))?;
        for child in ["data/local", "data/cache", "data/config"] {
            validated.child(Path::new(child))?;
        }
        let home = match home {
            Some((_, home)) => crate::test_mode::validate_root(id, &home, &protected)?
                .path()
                .to_path_buf(),
            None => validated.path().to_path_buf(),
        };
        return Ok((Some(data), Some(home)));
    }
    if legacy_data.is_none() && home.is_none() {
        return Ok((None, None));
    }
    // Legacy selectors deliberately retain their historical lexical mapping; only the canonical
    // selector denotes a bound runner home and participates in activation pairing.
    // The real folders of this app, in both namespaces and with or without the org folder, and the
    // real home. An override at or inside the first group, or equal to the home, is refused.
    let mut inside: Vec<PathBuf> = Vec::new();
    for base in [&bases.data, &bases.config, &bases.local, &bases.cache]
        .into_iter()
        .flatten()
    {
        for ns in [app.name.to_string(), format!("{}Dev", app.name)] {
            for org in [app.org, None] {
                let probe = AppId { org, ..app.clone() };
                inside.push(app_dir(base, &probe, &ns));
            }
        }
    }
    let win = opts.os == Os::Windows;
    let check = |var: &str, path: PathBuf, home_var: bool| -> Result<PathBuf> {
        check_absolute(opts.os, var, &path)?;
        let hit = inside.iter().any(|real| inside_or_equal(&path, real, win))
            || (home_var
                && bases.home.as_ref().is_some_and(|h| {
                    inside_or_equal(&path, h, win) && inside_or_equal(h, &path, win)
                }));
        if hit {
            return Err(AppDirsError::RealFolder {
                var: var.to_string(),
                path,
            });
        }
        Ok(path)
    };
    let data = legacy_data.map(|(v, p)| check(v, p, false)).transpose()?;
    let home = home.map(|(v, p)| check(v, p, true)).transpose()?;
    Ok((data, home))
}

/// Resolve the real user independently of HOME/XDG/APPDATA supplied by a QA launcher.
#[cfg(feature = "test-mode")]
fn native_protected_paths(app: &AppId) -> Result<Vec<PathBuf>> {
    #[cfg(windows)]
    let bases = Bases {
        data: known::known_folder(&known::ROAMING_APP_DATA),
        config: known::known_folder(&known::ROAMING_APP_DATA),
        local: known::known_folder(&known::LOCAL_APP_DATA),
        cache: known::known_folder(&known::LOCAL_APP_DATA),
        home: known::known_folder(&known::PROFILE),
    };
    #[cfg(unix)]
    let bases = {
        let mut size = 16 * 1024;
        let home = loop {
            let mut buffer = vec![0u8; size];
            let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
            let mut found = std::ptr::null_mut();
            // SAFETY: record and buffer are writable and remain alive while pw_dir is copied.
            let status = unsafe {
                libc::getpwuid_r(
                    libc::geteuid(),
                    record.as_mut_ptr(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    &mut found,
                )
            };
            if status == libc::ERANGE && size < 1024 * 1024 {
                size *= 2;
                continue;
            }
            if status != 0 || found.is_null() {
                return Err(crate::test_mode::Refusal::UserRootUnknown.into());
            }
            // SAFETY: successful lookup initialized record; pw_dir points into buffer.
            let raw = unsafe { record.assume_init().pw_dir };
            if raw.is_null() {
                return Err(crate::test_mode::Refusal::UserRootUnknown.into());
            }
            use std::os::unix::ffi::OsStrExt;
            let bytes = unsafe { std::ffi::CStr::from_ptr(raw) }.to_bytes();
            break PathBuf::from(std::ffi::OsStr::from_bytes(bytes));
        };
        Bases::read(Os::current(), &|key| {
            (key == "HOME").then(|| home.clone().into_os_string())
        })
    };
    #[cfg(not(any(unix, windows)))]
    return Err(crate::test_mode::Refusal::UserRootUnknown.into());
    #[cfg(any(unix, windows))]
    {
        if [&bases.data, &bases.config, &bases.local, &bases.cache]
            .into_iter()
            .any(|base| base.as_ref().is_none_or(|path| !path.is_absolute()))
        {
            return Err(crate::test_mode::Refusal::UserRootUnknown.into());
        }
        let home = bases
            .home
            .as_ref()
            .filter(|p| p.is_absolute())
            .ok_or(crate::test_mode::Refusal::UserRootUnknown)?;
        let mut protected = Vec::new();
        for base in [&bases.data, &bases.config, &bases.local, &bases.cache]
            .into_iter()
            .flatten()
        {
            protected.push(base.join(DEFAULT_ORG));
            for name in std::iter::once(app.name).chain(app.legacy_names.iter().copied()) {
                for ns in [name.to_string(), format!("{name}Dev")] {
                    protected.push(app_dir(base, app, &ns));
                    protected.push(base.join(ns));
                }
            }
        }
        for content in [
            "Documents",
            "Pictures",
            "Desktop",
            "Downloads",
            "Movies",
            "Music",
            "Videos",
        ] {
            protected.push(home.join(content));
        }
        Ok(protected)
    }
}

fn check_absolute(os: Os, var: &str, path: &Path) -> Result<()> {
    if looks_absolute(os, path) {
        Ok(())
    } else {
        Err(AppDirsError::NotAbsolute {
            var: var.to_string(),
            path: path.to_path_buf(),
        })
    }
}

/// Absolute for `os` and free of `..`, decided from the text so every OS mapping is testable on
/// any host.
fn looks_absolute(os: Os, path: &Path) -> bool {
    let s = path.to_string_lossy();
    let rooted = match os {
        Os::Windows => {
            let b = s.as_bytes();
            s.starts_with("\\\\")
                || (b.len() >= 3
                    && b[0].is_ascii_alphabetic()
                    && b[1] == b':'
                    && (b[2] == b'\\' || b[2] == b'/'))
        }
        _ => s.starts_with('/'),
    };
    rooted
        && !components(&s, os == Os::Windows, false)
            .iter()
            .any(|c| c == "..")
}

fn components(s: &str, windows: bool, lower: bool) -> Vec<String> {
    s.split(|c: char| c == '/' || (windows && c == '\\'))
        .filter(|c| !c.is_empty())
        .map(|c| {
            if lower {
                c.to_lowercase()
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// Whether `path` is `base` or below it (component-wise; case-insensitive on Windows).
fn inside_or_equal(path: &Path, base: &Path, windows: bool) -> bool {
    // On a Windows host `Path::join` writes `\`, so a path built for another OS (tests) mixes
    // separators; treat `\` as one there too.
    let seps = windows || cfg!(windows);
    let p = components(&path.to_string_lossy(), seps, windows);
    let b = components(&base.to_string_lossy(), seps, windows);
    p.len() >= b.len() && p[..b.len()] == b[..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| {
            pairs
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| OsString::from(v))
        }
    }

    const WIN: &[(&str, &str)] = &[
        ("APPDATA", r"C:\Users\a\AppData\Roaming"),
        ("LOCALAPPDATA", r"C:\Users\a\AppData\Local"),
        ("USERPROFILE", r"C:\Users\a"),
    ];

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// Path equality that ignores the host's separator, so every OS mapping is checked on any host.
    fn same(actual: &Path, expected: &str) {
        assert_eq!(
            components(&actual.to_string_lossy(), true, false),
            components(expected, true, false),
            "{} vs {expected}",
            actual.display()
        );
    }

    #[test]
    fn windows_mapping() {
        let d = AppDirs::resolve_with(
            &AppId::new("ViewRight"),
            &Options::new(Os::Windows),
            &env_of(WIN),
        )
        .unwrap();
        same(&d.data, r"C:\Users\a\AppData\Roaming\Orthic Labs\ViewRight");
        same(&d.local, r"C:\Users\a\AppData\Local\Orthic Labs\ViewRight");
        same(
            &d.cache,
            r"C:\Users\a\AppData\Local\Orthic Labs\ViewRight\Cache",
        );
        same(&d.home, r"C:\Users\a");
        assert!(!d.isolated);
    }

    #[test]
    fn macos_mapping() {
        let d = AppDirs::resolve_with(
            &AppId::new("MailRight"),
            &Options::new(Os::MacOs),
            &env_of(&[("HOME", "/Users/a")]),
        )
        .unwrap();
        same(
            &d.data,
            "/Users/a/Library/Application Support/Orthic Labs/MailRight",
        );
        assert_eq!(d.local, d.data);
        same(&d.cache, "/Users/a/Library/Caches/Orthic Labs/MailRight");
        same(&d.home, "/Users/a");
    }

    #[test]
    fn linux_mapping_uses_xdg_when_set_and_falls_back_to_home() {
        let app = AppId::new("CutRight");
        let o = Options::new(Os::Linux);
        let d = AppDirs::resolve_with(&app, &o, &env_of(&[("HOME", "/home/a")])).unwrap();
        same(&d.data, "/home/a/.local/share/Orthic Labs/CutRight");
        assert_eq!(d.local, d.data);
        same(&d.cache, "/home/a/.cache/Orthic Labs/CutRight");
        let d = AppDirs::resolve_with(
            &app,
            &o,
            &env_of(&[
                ("HOME", "/home/a"),
                ("XDG_DATA_HOME", "/x/data"),
                ("XDG_CACHE_HOME", "/x/cache"),
            ]),
        )
        .unwrap();
        same(&d.data, "/x/data/Orthic Labs/CutRight");
        same(&d.cache, "/x/cache/Orthic Labs/CutRight");
        // A relative XDG value is ignored, not trusted.
        let d = AppDirs::resolve_with(
            &app,
            &o,
            &env_of(&[("HOME", "/home/a"), ("XDG_DATA_HOME", "rel")]),
        )
        .unwrap();
        same(&d.data, "/home/a/.local/share/Orthic Labs/CutRight");
    }

    #[test]
    fn android_has_no_environment_mapping_and_points_at_the_injected_constructor() {
        let env = env_of(&[("HOME", "/data/user/0/x"), ("XDG_DATA_HOME", "/x")]);
        let err =
            AppDirs::resolve_with(&AppId::new("A"), &Options::new(Os::Android), &env).unwrap_err();
        assert!(matches!(err, AppDirsError::AndroidNeedsInjectedRoots));
        assert!(err.to_string().contains("AppDirs::injected"), "{err}");
        // The test root still applies (an instrumented test on a device).
        let o = Options::new(Os::Android).test_root(Some(p("/data/local/tmp/rightkit-test")));
        assert!(
            AppDirs::resolve_with(&AppId::new("A"), &o, &env_of(&[]))
                .unwrap()
                .isolated
        );
    }

    #[test]
    fn injected_roots_map_files_and_cache_without_a_namespace() {
        let d = AppDirs::injected_for(
            Os::Android,
            p("/data/user/0/com.orthiclabs.heardright/files"),
            p("/data/user/0/com.orthiclabs.heardright/cache"),
        )
        .unwrap();
        same(&d.data, "/data/user/0/com.orthiclabs.heardright/files");
        assert_eq!(d.local, d.data);
        assert_eq!(d.home, d.data);
        same(&d.cache, "/data/user/0/com.orthiclabs.heardright/cache");
        assert!(!d.isolated);
        // The host-OS constructor agrees on a path in the host's own syntax.
        let (f, c) = if cfg!(windows) {
            (r"C:\app\files", r"C:\app\cache")
        } else {
            ("/app/files", "/app/cache")
        };
        let d = AppDirs::injected(f, c).unwrap();
        assert_eq!((d.data, d.cache), (p(f), p(c)));
    }

    #[test]
    fn injected_roots_refuse_unusable_input() {
        let ok = |f: &str, c: &str| AppDirs::injected_for(Os::Android, p(f), p(c));
        for (f, c) in [
            ("", "/c"),
            ("/f", ""),
            ("files", "/c"),
            ("/f", "cache"),
            ("/f/../g", "/c"),
            ("/f", "/c/.."),
            ("/same", "/same"),
            ("/f", "/f/cache"),
            ("/f/files", "/f"),
        ] {
            assert!(
                matches!(ok(f, c), Err(AppDirsError::BadInjectedRoot { .. })),
                "{f:?} {c:?}"
            );
        }
        // A sibling that merely shares a prefix is fine.
        assert!(ok("/data/files", "/data/files-cache").is_ok());
        // Windows syntax is checked as Windows text, case-insensitively.
        assert!(
            AppDirs::injected_for(Os::Windows, p(r"C:\a\Files"), p(r"c:\a\files\cache")).is_err()
        );
    }

    #[test]
    fn injected_roots_work_with_create_all() {
        let root = process_test_root().join("InjectedCreateAll");
        let d = AppDirs::injected(root.join("files"), root.join("cache")).unwrap();
        d.create_all().unwrap();
        assert!(d.data.is_dir() && d.cache.is_dir());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn no_org_keeps_a_flat_layout() {
        let d = AppDirs::resolve_with(
            &AppId::new("ViewRight").without_org(),
            &Options::new(Os::Linux),
            &env_of(&[("HOME", "/home/a")]),
        )
        .unwrap();
        same(&d.data, "/home/a/.local/share/ViewRight");
    }

    #[test]
    fn dev_build_uses_the_dev_namespace() {
        let d = AppDirs::resolve_with(
            &AppId::new("ViewRight"),
            &Options::new(Os::Linux).dev(true),
            &env_of(&[("HOME", "/home/a")]),
        )
        .unwrap();
        same(&d.data, "/home/a/.local/share/Orthic Labs/ViewRightDev");
        same(&d.cache, "/home/a/.cache/Orthic Labs/ViewRightDev");
        assert_eq!(AppId::new("X").namespace(false), "X");
        assert_eq!(AppId::new("X").namespace(true), "XDev");
    }

    #[test]
    fn missing_or_relative_base_is_an_error_never_the_cwd() {
        let app = AppId::new("A");
        let none = env_of(&[]);
        for os in [Os::Windows, Os::MacOs, Os::Linux] {
            assert!(matches!(
                AppDirs::resolve_with(&app, &Options::new(os), &none),
                Err(AppDirsError::NoBase { .. })
            ));
        }
        let rel = env_of(&[
            ("APPDATA", "rel"),
            ("LOCALAPPDATA", "rel"),
            ("USERPROFILE", "rel"),
        ]);
        assert!(AppDirs::resolve_with(&app, &Options::new(Os::Windows), &rel).is_err());
        let blank = env_of(&[("HOME", "   ")]);
        assert!(AppDirs::resolve_with(&app, &Options::new(Os::Linux), &blank).is_err());
    }

    #[test]
    fn test_root_is_used_without_consulting_the_environment() {
        let root = process_test_root();
        let o = Options::new(Os::current()).test_root(Some(root.clone()));
        // An empty environment would error on the real mapping; the test root needs none.
        let d = AppDirs::resolve_with(&AppId::new("ViewRight"), &o, &env_of(&[])).unwrap();
        let r = root.join("ViewRight");
        assert_eq!(d.data, r.join("data"));
        assert_eq!(d.local, r.join("local"));
        assert_eq!(d.cache, r.join("cache"));
        assert_eq!(d.home, r.join("home"));
        assert!(d.isolated);
        for dir in [&d.data, &d.local, &d.cache, &d.home] {
            assert!(dir.starts_with(std::env::temp_dir()));
        }
        // The dev suffix applies inside the test root too.
        let d = AppDirs::resolve_with(&AppId::new("ViewRight"), &o.clone().dev(true), &env_of(&[]))
            .unwrap();
        assert_eq!(d.data, root.join("ViewRightDev").join("data"));
        // A relative test root is refused.
        let bad = Options::new(Os::Linux).test_root(Some(p("rel")));
        assert!(AppDirs::resolve_with(&AppId::new("A"), &bad, &env_of(&[])).is_err());
    }

    #[test]
    fn resolve_with_options_takes_the_namespace_from_the_options_not_the_build() {
        let root = process_test_root();
        // A dev build asking for the release shape gets the release namespace.
        let release = Options::new(Os::current())
            .dev(false)
            .test_root(Some(root.clone()));
        let d = AppDirs::resolve_with_options(AppId::new("ViewRight"), &release).unwrap();
        assert_eq!(d.data, root.join("ViewRight").join("data"));
        let dev = release.clone().dev(true);
        let d = AppDirs::resolve_with_options(AppId::new("ViewRight"), &dev).unwrap();
        assert_eq!(d.data, root.join("ViewRightDev").join("data"));
    }

    #[test]
    fn config_is_its_own_folder_on_linux_and_data_elsewhere() {
        let app = AppId::new("Ember");
        let linux = |pairs: &[(&'static str, &'static str)]| {
            AppDirs::resolve_with(&app, &Options::new(Os::Linux), &env_of(pairs)).unwrap()
        };
        let d = linux(&[("HOME", "/home/a")]);
        same(&d.config, "/home/a/.config/Orthic Labs/Ember");
        same(&d.data, "/home/a/.local/share/Orthic Labs/Ember");
        let d = linux(&[("HOME", "/home/a"), ("XDG_CONFIG_HOME", "/x/conf")]);
        same(&d.config, "/x/conf/Orthic Labs/Ember");
        // A relative XDG_CONFIG_HOME is ignored, like the other XDG bases.
        let d = linux(&[("HOME", "/home/a"), ("XDG_CONFIG_HOME", "rel")]);
        same(&d.config, "/home/a/.config/Orthic Labs/Ember");
        // No HOME and no XDG_CONFIG_HOME: the config base falls back to the data base instead
        // of being a new failure. A whole resolution still needs HOME (for `home` and, without
        // XDG_CACHE_HOME, `cache`), so this is checked on the bases, and the resolution itself
        // fails on HOME exactly as before.
        let only_data = env_of(&[("XDG_DATA_HOME", "/x/data")]);
        let bases = Bases::read(Os::Linux, &only_data);
        assert_eq!(bases.config, bases.data);
        same(bases.config.as_deref().unwrap(), "/x/data");
        assert!(AppDirs::resolve_with(&app, &Options::new(Os::Linux), &only_data).is_err());
        // With HOME restored the same environment resolves, and config sits next to data.
        let d = linux(&[("HOME", "/home/a"), ("XDG_DATA_HOME", "/x/data")]);
        same(&d.config, "/home/a/.config/Orthic Labs/Ember");

        let d = AppDirs::resolve_with(&app, &Options::new(Os::Windows), &env_of(WIN)).unwrap();
        assert_eq!(d.config, d.data);
        let d = AppDirs::resolve_with(
            &app,
            &Options::new(Os::MacOs),
            &env_of(&[("HOME", "/Users/a")]),
        )
        .unwrap();
        assert_eq!(d.config, d.data);
        same(
            &d.config,
            "/Users/a/Library/Application Support/Orthic Labs/Ember",
        );
    }

    #[test]
    fn config_under_a_test_root_and_injected_folders() {
        let d = AppDirs::for_test(AppId::new("Ember")).unwrap();
        assert_eq!(d.config, d.data.parent().unwrap().join("config"));
        let d = AppDirs::injected_for(Os::Linux, p("/files"), p("/cache")).unwrap();
        assert_eq!(d.config, d.data);
    }

    #[test]
    fn legacy_names_keep_an_existing_folder_per_directory() {
        let app = AppId::new("Ember").with_legacy_names(&["LightCraft", "OldCraft"]);
        let o = Options::new(Os::Linux);
        let env = env_of(&[("HOME", "/home/a")]);
        let resolve = |present: &'static [&'static str]| {
            AppDirs::resolve_with_legacy(&app, &o, &env, &|path: &Path| {
                let got = components(&path.to_string_lossy(), true, false);
                present
                    .iter()
                    .any(|want| components(want, true, false) == got)
            })
            .unwrap()
        };
        // Nothing exists: the current name, everywhere.
        let d = resolve(&[]);
        same(&d.data, "/home/a/.local/share/Orthic Labs/Ember");
        same(&d.config, "/home/a/.config/Orthic Labs/Ember");
        // Only the old config exists: config stays on it, data starts under the new name.
        let d = resolve(&["/home/a/.config/Orthic Labs/LightCraft"]);
        same(&d.config, "/home/a/.config/Orthic Labs/LightCraft");
        same(&d.data, "/home/a/.local/share/Orthic Labs/Ember");
        // The current name wins when it exists, even if a legacy folder does too.
        let d = resolve(&[
            "/home/a/.config/Orthic Labs/Ember",
            "/home/a/.config/Orthic Labs/LightCraft",
        ]);
        same(&d.config, "/home/a/.config/Orthic Labs/Ember");
        // Legacy names are tried in order.
        let d = resolve(&[
            "/home/a/.config/Orthic Labs/OldCraft",
            "/home/a/.config/Orthic Labs/LightCraft",
        ]);
        same(&d.config, "/home/a/.config/Orthic Labs/LightCraft");
        // Cache is never moved to a legacy name.
        let d = resolve(&["/home/a/.cache/Orthic Labs/LightCraft"]);
        same(&d.cache, "/home/a/.cache/Orthic Labs/Ember");
        // The pure resolver ignores legacy names; a dev build looks for `<legacy>Dev`.
        let d = AppDirs::resolve_with(&app, &o, &env).unwrap();
        same(&d.config, "/home/a/.config/Orthic Labs/Ember");
        let dev = Options::new(Os::Linux).dev(true);
        let d = AppDirs::resolve_with_legacy(&app, &dev, &env, &|path: &Path| {
            path.to_string_lossy().ends_with("LightCraftDev")
        })
        .unwrap();
        same(&d.data, "/home/a/.local/share/Orthic Labs/LightCraftDev");
    }

    #[test]
    fn resolve_under_this_crates_own_tests_stays_in_the_temp_root() {
        let d = AppDirs::resolve(AppId::new("ViewRight")).unwrap();
        let root = process_test_root();
        for dir in [&d.data, &d.local, &d.cache, &d.home] {
            assert!(
                dir.starts_with(&root),
                "{} outside {}",
                dir.display(),
                root.display()
            );
        }
        let t = AppDirs::for_test(AppId::new("ViewRight")).unwrap();
        assert!(t.data.starts_with(&root));
    }

    #[test]
    fn create_all_makes_the_folders() {
        let d = AppDirs::for_test(AppId::new("AppDirsCreateAll")).unwrap();
        d.create_all().unwrap();
        assert!(d.data.is_dir() && d.local.is_dir() && d.cache.is_dir() && d.home.is_dir());
        let _ = std::fs::remove_dir_all(process_test_root().join("AppDirsCreateAll"));
    }

    /// Without `test-mode` nothing reads an override variable, even when asked to.
    #[cfg(not(feature = "test-mode"))]
    #[test]
    fn overrides_are_ignored_without_test_mode() {
        let env = |key: &str| match key {
            "RIGHTKIT_UNDER_TEST" | "RIGHTKIT_QA_DATA_DIR" | "RIGHTKIT_QA_HOME" => {
                panic!("feature-off resolver read selector {key}")
            }
            "HOME" => Some(OsString::from("/home/a")),
            _ => None,
        };
        let d = AppDirs::resolve_with(
            &AppId::new("ViewRight"),
            &Options::new(Os::Linux).qa_overrides(true),
            &env,
        )
        .unwrap();
        same(&d.data, "/home/a/.local/share/Orthic Labs/ViewRight");
        assert!(!d.isolated);
    }

    #[cfg(feature = "test-mode")]
    mod qa {
        use super::*;

        fn fixture() -> (tempfile::TempDir, tempfile::TempDir, AppId, Options) {
            let root = tempfile::tempdir().unwrap();
            let real = tempfile::tempdir().unwrap();
            crate::private::ensure_private_dir(root.path()).unwrap();
            std::fs::write(
                root.path().join(crate::test_mode::BINDING_FILE),
                r#"{"app":"viewright"}"#,
            )
            .unwrap();
            let app = AppId::new("ViewRight").with_qa_app_id("viewright");
            let opts = Options::new(Os::current())
                .qa_overrides(true)
                .protected_paths(vec![real.path().to_path_buf()]);
            (root, real, app, opts)
        }

        #[test]
        fn canonical_bound_home_uses_shared_validation_and_child_derivation() {
            let (root, _real, app, opts) = fixture();
            let env = |key: &str| match key {
                UNDER_TEST_VAR => Some(OsString::from("1")),
                QA_DATA_DIR_VAR | "HOME" | "USERPROFILE" => {
                    Some(root.path().as_os_str().to_owned())
                }
                _ => None,
            };
            let d = AppDirs::resolve_with(&app, &opts, &env).unwrap();
            let canonical = std::fs::canonicalize(root.path()).unwrap();
            assert_eq!(d.data, canonical.join("data"));
            assert_eq!(d.home, canonical);
            assert_eq!(d.local, d.data.join("local"));
            assert!(d.isolated);
        }

        #[test]
        fn both_partial_selector_pairs_refuse() {
            let (root, _real, app, opts) = fixture();
            for activated in [false, true] {
                let env = |key: &str| match key {
                    UNDER_TEST_VAR if activated => Some(OsString::from("1")),
                    QA_DATA_DIR_VAR if !activated => Some(root.path().as_os_str().to_owned()),
                    _ => None,
                };
                assert!(AppDirs::resolve_with(&app, &opts, &env).is_err());
            }
            let env = |key: &str| (key == UNDER_TEST_VAR).then(|| OsString::from("1"));
            assert!(matches!(
                AppDirs::resolve_with(&app, &opts, &env),
                Err(AppDirsError::TestMode(
                    crate::test_mode::Refusal::ActivationWithoutRoot
                ))
            ));
        }

        #[cfg(unix)]
        #[test]
        fn canonical_override_refuses_data_and_nested_child_escapes() {
            let (root, real, app, opts) = fixture();
            let env = |key: &str| match key {
                UNDER_TEST_VAR => Some(OsString::from("1")),
                QA_DATA_DIR_VAR => Some(root.path().as_os_str().to_owned()),
                _ => None,
            };
            let data = root.path().join("data");
            std::os::unix::fs::symlink(real.path(), &data).unwrap();
            assert!(matches!(
                AppDirs::resolve_with(&app, &opts, &env),
                Err(AppDirsError::TestMode(
                    crate::test_mode::Refusal::ChildEscapesRoot
                ))
            ));
            std::fs::remove_file(&data).unwrap();
            std::fs::create_dir(&data).unwrap();
            for name in ["local", "config", "cache"] {
                let child = data.join(name);
                std::os::unix::fs::symlink(real.path(), &child).unwrap();
                assert!(matches!(
                    AppDirs::resolve_with(&app, &opts, &env),
                    Err(AppDirsError::TestMode(
                        crate::test_mode::Refusal::ChildEscapesRoot
                    ))
                ));
                std::fs::remove_file(child).unwrap();
            }
        }

        #[test]
        fn canonical_binding_privacy_and_overlap_are_not_lexical_only() {
            let (root, real, app, opts) = fixture();
            let env = |key: &str| match key {
                UNDER_TEST_VAR => Some(OsString::from("1")),
                QA_DATA_DIR_VAR => Some(root.path().as_os_str().to_owned()),
                _ => None,
            };
            for bad in [
                r#"{"nested":{"app":"viewright"}}"#,
                r#"{"app":"viewright""#,
                r#"{"app":"other"}"#,
            ] {
                std::fs::write(root.path().join(crate::test_mode::BINDING_FILE), bad).unwrap();
                assert!(AppDirs::resolve_with(&app, &opts, &env).is_err());
            }
            std::fs::write(
                root.path().join(crate::test_mode::BINDING_FILE),
                r#"{"app":"viewright"}"#,
            )
            .unwrap();
            let overlapping = opts
                .clone()
                .protected_paths(vec![root.path().join("future")]);
            assert!(matches!(
                AppDirs::resolve_with(&app, &overlapping, &env),
                Err(AppDirsError::TestMode(
                    crate::test_mode::Refusal::RootOverlapsUserData
                ))
            ));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755))
                    .unwrap();
                assert!(matches!(
                    AppDirs::resolve_with(&app, &opts, &env),
                    Err(AppDirsError::TestMode(
                        crate::test_mode::Refusal::RootNotPrivate
                    ))
                ));
                crate::private::ensure_private_dir(root.path()).unwrap();
                let alias = real.path().join("alias");
                std::os::unix::fs::symlink(root.path(), &alias).unwrap();
                let aliased = opts.clone().protected_paths(vec![alias.join("future")]);
                assert!(matches!(
                    AppDirs::resolve_with(&app, &aliased, &env),
                    Err(AppDirsError::TestMode(
                        crate::test_mode::Refusal::RootOverlapsUserData
                    ))
                ));
            }
            #[cfg(windows)]
            {
                // A normal inherited temp DACL is not the private DACL required for QA roots.
                let unprivate = real.path().join("public-home");
                std::fs::create_dir(&unprivate).unwrap();
                use std::os::windows::process::CommandExt;
                assert!(std::process::Command::new("icacls")
                    .arg(&unprivate)
                    .args(["/grant", "*S-1-1-0:(OI)(CI)R"])
                    .creation_flags(0x0800_0000)
                    .output()
                    .unwrap()
                    .status
                    .success());
                std::fs::write(
                    unprivate.join(crate::test_mode::BINDING_FILE),
                    r#"{"app":"viewright"}"#,
                )
                .unwrap();
                let inherited_env = |key: &str| match key {
                    UNDER_TEST_VAR => Some(OsString::from("1")),
                    QA_DATA_DIR_VAR => Some(unprivate.as_os_str().to_owned()),
                    _ => None,
                };
                let isolated = opts
                    .clone()
                    .protected_paths(vec![root.path().to_path_buf()]);
                assert!(matches!(
                    AppDirs::resolve_with(&app, &isolated, &inherited_env),
                    Err(AppDirsError::TestMode(
                        crate::test_mode::Refusal::RootNotPrivate
                    ))
                ));
            }
        }

        #[test]
        fn legacy_mapping_remains_direct_data_and_independent_home() {
            let app = AppId::new("HeardRight").with_legacy_env(Some("HR_DATA"), Some("HR_HOME"));
            let env = env_of(&[("HR_DATA", "/tmp/hr"), ("HR_HOME", "/tmp/hrhome")]);
            let d = AppDirs::resolve_with(&app, &Options::new(Os::Linux).qa_overrides(true), &env)
                .unwrap();
            assert_eq!((d.data, d.home), (p("/tmp/hr"), p("/tmp/hrhome")));
        }
    }
    #[cfg(windows)]
    #[test]
    fn windows_bases_come_from_known_folders() {
        let roaming = known::known_folder(&known::ROAMING_APP_DATA).expect("RoamingAppData");
        let local = known::known_folder(&known::LOCAL_APP_DATA).expect("LocalAppData");
        let profile = known::known_folder(&known::PROFILE).expect("Profile");
        assert!(roaming.is_absolute() && local.is_absolute() && profile.is_absolute());
        // The environment `AppDirs::resolve` reads yields the same folders, whatever the
        // variables say.
        let b = Bases::read(Os::Windows, &process_env);
        assert_eq!(b.data.as_ref(), Some(&roaming));
        assert_eq!(b.local.as_ref(), Some(&local));
        assert_eq!(b.cache.as_ref(), Some(&local));
        assert_eq!(b.home.as_ref(), Some(&profile));
    }
}
