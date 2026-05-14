// Bake an rpath to the Xcode Swift runtime into the final binary.
//
// The `screencapturekit` crate's own build.rs emits the same rpath flags,
// but `cargo:rustc-link-arg` is scoped to the emitting package only — so a
// lib crate's flags never reach a downstream binary's link command. We have
// to re-emit them here, where the package being linked IS the binary.
//
// Symptom if you drop this: `dyld: Library not loaded: @rpath/libswift_Concurrency.dylib`.

use std::process::Command;

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    // /usr/lib/swift holds most of Swift's stdlib on modern macOS — cheap to
    // include even though libswift_Concurrency.dylib lives only in the Xcode
    // toolchain on this version of macOS.
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");

    let xcode_dev_dir = match Command::new("xcode-select").arg("-p").output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => {
            println!(
                "cargo:warning=xcode-select -p failed; macrdp may not load at runtime. \
                 Install full Xcode (not just Command Line Tools)."
            );
            return;
        }
    };

    // Both the old swift-5.5 layout and the unversioned `swift/` layout — Xcode
    // versions differ on which exists. Adding both rpaths is harmless if one
    // is absent; the dynamic linker just skips missing entries.
    for slice in ["swift-5.5", "swift"] {
        let path =
            format!("{xcode_dev_dir}/Toolchains/XcodeDefault.xctoolchain/usr/lib/{slice}/macosx");
        println!("cargo:rustc-link-arg=-Wl,-rpath,{path}");
    }
}
