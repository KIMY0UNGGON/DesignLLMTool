//! Embeds the app icon (`assets/app.ico`, regenerated from `design/icons/app/`) as the Windows icon
//! resource of the executables that link this crate. The Windows SDK's `rc.exe` compiles it, so no
//! build dependency is added. When the icon or `rc.exe` is missing the build still succeeds and says
//! so as a warning; the executable then has no icon, and nothing else changes.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/app.ico");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc")
    {
        return;
    }
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let out = PathBuf::from(env::var("OUT_DIR").unwrap_or_default());
    let icon = manifest.join("assets").join("app.ico");
    if !icon.is_file() {
        println!("cargo:warning=assets/app.ico is missing; the executable gets no icon");
        return;
    }
    let script = out.join("app-icon.rc");
    let resource = out.join("app-icon.res");
    // rc.exe reads forward slashes in quoted paths without escaping.
    let icon_path = icon.display().to_string().replace('\\', "/");
    if fs::write(&script, format!("1 ICON \"{icon_path}\"\n")).is_err() {
        println!("cargo:warning=could not write the icon script; the executable gets no icon");
        return;
    }
    let Some(rc) = find_rc() else {
        println!("cargo:warning=rc.exe was not found in the Windows SDK; the executable gets no icon");
        return;
    };
    match Command::new(&rc)
        .arg("/nologo")
        .arg(format!("/fo{}", resource.display()))
        .arg(&script)
        .status()
    {
        Ok(status) if status.success() => {
            // The executable crate links this file (its build script reads the metadata below).
            println!("cargo:icon-res={}", resource.display());
        }
        _ => println!("cargo:warning=rc.exe failed on the app icon; the executable gets no icon"),
    }
}

/// The newest x64 `rc.exe` of the Windows 10/11 SDK in its default folder.
fn find_rc() -> Option<PathBuf> {
    let root = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut versions: Vec<PathBuf> = fs::read_dir(root)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    versions.sort();
    versions
        .into_iter()
        .rev()
        .map(|dir| dir.join("x64").join("rc.exe"))
        .find(|path| path.is_file())
}
