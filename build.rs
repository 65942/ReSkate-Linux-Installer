// Builds nativepicker.dll (install --native-picker) with MinGW; main.rs embeds it.
use std::{env, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=nativepicker");
    let out = format!("{}/nativepicker.dll", env::var("OUT_DIR").unwrap());
    let status = Command::new("x86_64-w64-mingw32-g++")
        // Older MinGW (Ubuntu 22.04's) targets XP, which hides IFileOpenDialog and friends.
        .args(["-D_WIN32_WINNT=0x0601", "-std=c++17", "-O2", "-s", "-shared", "-static", "-o", &out,
               "nativepicker/nativepicker.cpp", "nativepicker/nativepicker.def", "-lole32", "-lshell32", "-luuid"])
        .status()
        .expect("x86_64-w64-mingw32-g++ not found: install MinGW-w64 (Arch: mingw-w64-gcc, Debian/Ubuntu: g++-mingw-w64-x86-64)");
    assert!(status.success(), "building nativepicker.dll failed");
}
