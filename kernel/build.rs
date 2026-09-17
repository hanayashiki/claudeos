//! Hand the linker this machine's linker script, by absolute path.
//!
//! A `-T` in the rustflags of .cargo/config.toml is resolved against the
//! directory rustc runs in, which is the workspace root for a build from there
//! and kernel/ for a build from here. The path out of CARGO_MANIFEST_DIR is the
//! same from anywhere.

use std::env;

fn main() {
    let dir = env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rerun-if-changed=linker-aarch64.ld");

    // Only the kernel's own targets link with a script. A check of the crate
    // for another target, as an editor runs, links nothing.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("none") {
        return;
    }
    let script = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => "linker.ld",
        Ok("aarch64") => "linker-aarch64.ld",
        other => panic!("no linker script for target_arch {other:?}"),
    };
    println!("cargo:rustc-link-arg=-T{dir}/{script}");
}
