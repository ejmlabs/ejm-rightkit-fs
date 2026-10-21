//! `rightkit-swap A B`: atomically exchanges two paths with `renamex_np(RENAME_SWAP)` or fails.
//!
//! Used by `right-release install-app` so an installed app bundle is never absent, even under
//! SIGKILL. On failure it writes one line to stderr and exits nonzero.
//! Exit: 0 swapped, 1 I/O (missing path, permissions, ...), 2 usage or invalid path,
//! 3 swap unsupported here (non-APFS volume or non-macOS; callers fall back to two renames).

use rightkit_fs::swap::is_swap_unsupported;
use rightkit_fs::swap_paths;
use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.len() != 2 {
        eprintln!("rightkit-swap: usage: rightkit-swap A B");
        return ExitCode::from(2);
    }
    match swap_paths(Path::new(&args[0]), Path::new(&args[1])) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if error.kind() == std::io::ErrorKind::InvalidInput && error.raw_os_error().is_none() {
                eprintln!("rightkit-swap: SWAP_INVALID_PATH: {error}");
                return ExitCode::from(2);
            }
            if is_swap_unsupported(&error) {
                eprintln!("rightkit-swap: SWAP_UNSUPPORTED: {error}");
                return ExitCode::from(3);
            }
            eprintln!("rightkit-swap: SWAP_IO: {error}");
            ExitCode::from(1)
        }
    }
}
