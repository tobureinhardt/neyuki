use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    threaded_interpreter_cfg();

    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let lib_dir = manifest_dir.join("lib");
    println!("cargo:rerun-if-changed={}", lib_dir.display());

    let mut libraries = fs::read_dir(&lib_dir)
        .unwrap_or_else(|err| panic!("failed to read {}: {}", lib_dir.display(), err))
        .map(|entry| entry.expect("failed to read library entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "nyk"))
        .collect::<Vec<_>>();
    libraries.sort();

    let mut generated = String::from("&[\n");
    for path in libraries {
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_else(|| panic!("invalid library filename: {}", path.display()));
        generated.push_str(&format!(
            "    (\"@neyuki/{name}\", include_str!({:?})),\n",
            path.to_string_lossy()
        ));
    }
    generated.push_str("]\n");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let output = out_dir.join("bundled_libraries.rs");
    fs::write(&output, generated)
        .unwrap_or_else(|err| panic!("failed to write {}: {}", output.display(), err));

    // The library reference pages (all but the README), which the language
    // server answers completion and hover from.
    let reference_dir = manifest_dir.join("docs").join("reference");
    println!("cargo:rerun-if-changed={}", reference_dir.display());

    let mut pages = fs::read_dir(&reference_dir)
        .unwrap_or_else(|err| panic!("failed to read {}: {}", reference_dir.display(), err))
        .map(|entry| entry.expect("failed to read reference entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect::<Vec<_>>();
    pages.sort();

    let mut generated = String::from("&[\n");
    for path in pages {
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_else(|| panic!("invalid reference filename: {}", path.display()));
        if name == "README" {
            continue;
        }
        generated.push_str(&format!(
            "    ({name:?}, include_str!({:?})),\n",
            path.to_string_lossy()
        ));
    }
    generated.push_str("]\n");

    let output = out_dir.join("reference_pages.rs");
    fs::write(&output, generated)
        .unwrap_or_else(|err| panic!("failed to write {}: {}", output.display(), err));
}

/// Enables the threaded interpreter (`src/vm/threaded.rs`) only where its
/// handler-to-handler tail calls are known to become jumps. Rust does not
/// guarantee tail calls, so it stays behind the opt-in `threaded` feature and
/// is further limited to optimized builds without debug assertions on 64-bit
/// x86 and ARM, where LLVM performs sibling-call optimisation. Windows is
/// left out: the Win64 convention passes only four arguments in registers,
/// and a handler hands the next one a new sixth argument on the stack, so
/// no dispatch can be a jump there. Everywhere else the loop interpreter in
/// `interp.rs` runs.
fn threaded_interpreter_cfg() {
    println!("cargo::rustc-check-cfg=cfg(neyuki_threaded)");
    let feature = env::var_os("CARGO_FEATURE_THREADED").is_some();
    let optimized = matches!(env::var("OPT_LEVEL").as_deref(), Ok("2" | "3"));
    let debug_assertions = env::var_os("CARGO_CFG_DEBUG_ASSERTIONS").is_some();
    let arch_ok = matches!(
        env::var("CARGO_CFG_TARGET_ARCH").as_deref(),
        Ok("x86_64" | "aarch64")
    );
    let width_ok = env::var("CARGO_CFG_TARGET_POINTER_WIDTH").as_deref() == Ok("64");
    let windows = env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    if feature && optimized && !debug_assertions && arch_ok && width_ok && !windows {
        println!("cargo::rustc-cfg=neyuki_threaded");
    }
}
