//! `rightkit-clonefile SRC DST`: clones SRC to DST with `clonefile(2)` (CLONE_NOFOLLOW) or fails.
//!
//! Used by the build-control broker for class-1 protected build roots. It never copies bytes:
//! on failure it writes one JSON line to stderr and exits with a status from `CloneError::exit_code`.
//! Exit: 0 cloned, 1 I/O, 2 usage or invalid path, 3 clone unsupported, 4 cross-volume,
//! 5 symlink source refused, 6 source not a regular file.

use rightkit_fs::clone_file_nofollow;
use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.len() != 2 {
        eprintln!(
            "{}",
            serde_json::json!({ "ok": false, "code": "CLONE_USAGE", "message": "usage: rightkit-clonefile SRC DST" })
        );
        return ExitCode::from(2);
    }
    match clone_file_nofollow(Path::new(&args[0]), Path::new(&args[1])) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!(
                "{}",
                serde_json::json!({ "ok": false, "code": error.code(), "message": error.to_string() })
            );
            ExitCode::from(error.exit_code())
        }
    }
}
