//! Compile assets/opendisplay.rc (icons + version info) into both executables.
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = std::env::var("OUT_DIR").unwrap();
    let obj = format!("{out}/resources.o");
    let ver = std::env::var("CARGO_PKG_VERSION").unwrap();
    let mut parts = ver.split('.').map(|p| p.split('-').next().unwrap_or("0").to_string());
    let (major, minor, patch) = (parts.next().unwrap(), parts.next().unwrap(), parts.next().unwrap());
    let windres = std::env::var("WINDRES").unwrap_or_else(|_| "x86_64-w64-mingw32-windres".into());
    let status = Command::new(&windres)
        .args(["-O", "coff", "-I", "assets"])
        .args([
            format!("-DVER_MAJOR={major}"),
            format!("-DVER_MINOR={minor}"),
            format!("-DVER_PATCH={patch}"),
            format!("-DVER_STRING=\\\"{ver}\\\""),
        ])
        .args(["assets/opendisplay.rc", "-o", &obj])
        .status()
        .unwrap_or_else(|e| panic!("running {windres}: {e} (llvm-mingw on PATH?)"));
    assert!(status.success(), "{windres} failed");
    println!("cargo:rustc-link-arg-bins={obj}");
}
