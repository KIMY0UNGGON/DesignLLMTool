//! Links the app icon resource into the DesignLLMTool executable. designllm-desktop's build script
//! compiles `assets/app.ico` with rc.exe and publishes the .res path as build metadata
//! (`links = "designllm_desktop"` -> DEP_DESIGNLLM_DESKTOP_ICON_RES). Only with the `gui` feature,
//! the one that makes desktop a dependency; headless users of the library are unaffected.
use std::env;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=DEP_DESIGNLLM_DESKTOP_ICON_RES");
    if env::var_os("CARGO_FEATURE_GUI").is_none() || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    match env::var("DEP_DESIGNLLM_DESKTOP_ICON_RES") {
        Ok(resource) if !resource.is_empty() => println!("cargo:rustc-link-arg-bins={resource}"),
        // designllm-desktop already warned why (no icon file or no rc.exe).
        _ => println!("cargo:warning=designllm-desktop published no icon resource; DesignLLMTool.exe gets no icon"),
    }
}
