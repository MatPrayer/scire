//! `cargo dmg` — build an installable macOS .dmg for Scirè.
//!
//! Thin wrapper over the packaging scripts:
//!
//!   1. `cargo build --release`  (the .dmg needs the optimized binary)
//!   2. `packaging/macos/make-dmg.sh` (stages + compresses the image)
//!
//! Kept dependency-free so `cargo dmg` compiles in a second regardless of how
//! heavy the rest of the workspace is.
//!
//! Script args are passed through (e.g. `OUT_DIR=... cargo dmg`); there are no
//! CLI flags of our own.

use std::env;
use std::path::PathBuf;
use std::process::Command;

const SCRIPT_REL: &str = "packaging/macos/make-dmg.sh";

fn main() {
    if !cfg!(target_os = "macos") {
        eprintln!("error: a .dmg can only be built on macOS");
        std::process::exit(1);
    }

    let root = workspace_root();
    if !root.join(SCRIPT_REL).is_file() {
        eprintln!(
            "error: {} not found — expected a Cargo workspace checkout",
            SCRIPT_REL
        );
        std::process::exit(1);
    }

    // 1. Release build. Run through `cargo` (not the binary directly) so the
    //    user's configured toolchain/flags apply unchanged.
    let status = Command::new("cargo")
        .arg("build")
        .arg("--release")
        .current_dir(&root)
        .status()
        .expect("failed to run cargo build --release");
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    // 2. Package. Inherit stdin/stdout/stderr so hdiutil/osascript output and
    //    any prompts surface to the user.
    let script = root.join(SCRIPT_REL);
    let status = Command::new(&script)
        .args(env::args_os().skip(2))
        .current_dir(&root)
        .status()
        .expect("failed to run make-dmg.sh");
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
}

/// Walk up from CARGO_MANIFEST_DIR to the directory holding the `packaging/`
/// tree and the workspace `Cargo.toml`. `cargo run` sets CARGO_MANIFEST_DIR to
/// this crate's dir, which is `crates/dmg`, one level below the root.
fn workspace_root() -> PathBuf {
    if let Some(manifest) = env::var_os("CARGO_MANIFEST_DIR") {
        let dir = PathBuf::from(manifest);
        for ancestor in dir.ancestors() {
            if ancestor.join("Cargo.toml").is_file() && ancestor.join(SCRIPT_REL).is_file() {
                return ancestor.to_path_buf();
            }
        }
    }
    resolve_by_curdir().unwrap_or_else(|e| {
        eprintln!("error: could not locate workspace root: {e}");
        std::process::exit(1);
    })
}

/// Fallback when CARGO_MANIFEST_DIR is unset (e.g. running the raw binary):
/// climb from the current directory looking for the packaging tree.
fn resolve_by_curdir() -> Result<PathBuf, String> {
    let cwd = env::current_dir().map_err(|e| e.to_string())?;
    for ancestor in cwd.ancestors() {
        if ancestor.join(SCRIPT_REL).is_file() {
            return Ok(ancestor.to_path_buf());
        }
    }
    Err(format!(
        "no {} found in any parent of {}",
        SCRIPT_REL,
        cwd.display()
    ))
}
