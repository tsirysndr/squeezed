//! On macOS, build the Core Audio HAL plug-in (the `squeezed-driver` cdylib in
//! driver/) and stash it in OUT_DIR so `src/driver.rs` can embed it in the
//! binary (`squeezed driver install` writes it back out to
//! /Library/Audio/Plug-Ins/HAL). Other platforms build nothing extra.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    println!("cargo:rerun-if-changed=driver/src/lib.rs");
    println!("cargo:rerun-if-changed=driver/build.rs");
    println!("cargo:rerun-if-changed=driver/Cargo.toml");
    println!("cargo:rerun-if-changed=driver/Info.plist");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let target = std::env::var("TARGET").unwrap();

    // Nested cargo build: a separate --target-dir avoids deadlocking on the
    // outer build's target directory lock.
    let driver_target = out_dir.join("driver-target");
    let status = Command::new(&cargo)
        // Don't inherit wrappers from the outer invocation (e.g. clippy-driver
        // when this runs under `cargo clippy`) — we always want a plain build.
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        .args(["build", "--release", "--target"])
        .arg(&target)
        .arg("--manifest-path")
        .arg(manifest_dir.join("driver/Cargo.toml"))
        .arg("--target-dir")
        .arg(&driver_target)
        .status()
        .expect("failed to run cargo for the squeezed-driver crate");
    assert!(
        status.success(),
        "building the squeezed-driver crate failed"
    );

    let dylib = driver_target
        .join(&target)
        .join("release/libsqueezed_driver.dylib");
    std::fs::copy(&dylib, out_dir.join("SqueezedAudio"))
        .unwrap_or_else(|e| panic!("copying {} to OUT_DIR: {e}", dylib.display()));
}
