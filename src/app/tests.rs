use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::build::{BuildOptions, Platform, build, slug};
use super::bundle::{self, Bundle, Entry, normalize, pack};
use super::icon;

/// Tests that install a bundle share the process-wide slot, so they take
/// turns.
static INSTALL_LOCK: Mutex<()> = Mutex::new(());

fn meta() -> BTreeMap<String, String> {
    let mut meta = BTreeMap::new();
    meta.insert("name".to_string(), "Test App".to_string());
    meta.insert("version".to_string(), "1.2.3".to_string());
    meta.insert("entry".to_string(), "main.nyk".to_string());
    meta
}

fn sample_files() -> Vec<(String, Vec<u8>)> {
    vec![
        ("main.nyk".to_string(), b"print(1)".to_vec()),
        ("assets/fonts/Inter.ttf".to_string(), vec![0, 1, 2, 3, 255]),
        ("assets/logo.png".to_string(), b"not really a png".to_vec()),
        ("assets/empty.txt".to_string(), Vec::new()),
    ]
}

/// A directory of its own under the system temp directory.
fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("neyuki_app_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The 24-byte start of a PNG of the given size: enough for the icon code,
/// which only reads the header.
fn png_header(width: u32, height: u32) -> Vec<u8> {
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(&13u32.to_be_bytes());
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&width.to_be_bytes());
    png.extend_from_slice(&height.to_be_bytes());
    png.extend_from_slice(&[8, 6, 0, 0, 0]);
    png
}

#[test]
fn test_normalize_paths() {
    assert_eq!(
        normalize("assets/logo.png").as_deref(),
        Some("assets/logo.png")
    );
    assert_eq!(
        normalize("./assets//logo.png").as_deref(),
        Some("assets/logo.png")
    );
    assert_eq!(
        normalize("assets\\fonts\\Inter.ttf").as_deref(),
        Some("assets/fonts/Inter.ttf")
    );
    assert_eq!(normalize("assets/./x/").as_deref(), Some("assets/x"));
    for rejected in [
        "",
        ".",
        "./",
        "..",
        "../secret.txt",
        "assets/../../etc/passwd",
        "assets\\..\\x",
        "/etc/passwd",
        "\\\\server\\share",
        "C:\\Windows\\x",
        "C:x",
        "a\0b",
    ] {
        assert_eq!(
            normalize(rejected),
            None,
            "{:?} should be rejected",
            rejected
        );
    }
}

#[test]
fn test_pack_and_unpack_in_memory() {
    let payload = pack(&meta(), &sample_files()).unwrap();
    let bundle = Bundle::from_payload(payload).unwrap();
    assert_eq!(bundle.meta("name"), Some("Test App"));
    assert_eq!(bundle.meta("version"), Some("1.2.3"));
    assert_eq!(bundle.meta("missing"), None);
    for (path, data) in sample_files() {
        assert_eq!(bundle.read(&path).unwrap().unwrap(), data, "{}", path);
    }
    // Lookups normalize the same way paths were packed.
    assert_eq!(
        bundle.read("./assets\\logo.png").unwrap().unwrap(),
        b"not really a png"
    );
    assert!(bundle.read("assets/missing.png").is_none());
    assert!(bundle.read("../main.nyk").is_none());

    assert_eq!(
        bundle.entry("assets/fonts/Inter.ttf"),
        Some(Entry::File { size: 5 })
    );
    assert_eq!(bundle.entry("assets"), Some(Entry::Dir));
    assert_eq!(bundle.entry("assets/fonts/"), Some(Entry::Dir));
    assert_eq!(bundle.entry("assets/nothing"), None);
    assert_eq!(
        bundle.entry("."),
        None,
        "the bundle root is not a directory"
    );

    assert_eq!(
        bundle.list("assets").unwrap(),
        vec!["empty.txt", "fonts", "logo.png"]
    );
    assert_eq!(bundle.list("assets/fonts").unwrap(), vec!["Inter.ttf"]);
    assert!(bundle.list("main.nyk").is_none());
    assert_eq!(bundle.paths().count(), 4);
}

#[test]
fn test_pack_rejects_bad_paths() {
    for bad in ["../x", "/abs", "a/../b", "./a", "a//b", "a\\b", ""] {
        let files = vec![(bad.to_string(), vec![1])];
        assert!(
            pack(&meta(), &files).is_err(),
            "{:?} should be rejected",
            bad
        );
    }
    let twice = vec![("a".to_string(), vec![1]), ("a".to_string(), vec![2])];
    assert!(pack(&meta(), &twice).unwrap_err().contains("twice"));
}

#[test]
fn test_damaged_payloads_are_errors() {
    let payload = pack(&meta(), &sample_files()).unwrap();
    // Every truncation fails cleanly rather than panicking.
    for len in 0..payload.len() {
        assert!(
            Bundle::from_payload(payload[..len].to_vec()).is_err(),
            "length {}",
            len
        );
    }
    let mut wrong_magic = payload.clone();
    wrong_magic[7] = 99;
    assert!(Bundle::from_payload(wrong_magic).is_err());

    // An index naming a path outside the bundle is refused when read back.
    let mut forged = Vec::new();
    forged.extend_from_slice(bundle::PAYLOAD_MAGIC);
    forged.extend_from_slice(&0u32.to_le_bytes());
    forged.extend_from_slice(&1u32.to_le_bytes());
    forged.extend_from_slice(&5u32.to_le_bytes());
    forged.extend_from_slice(b"../x2");
    forged.extend_from_slice(&2u64.to_le_bytes());
    forged.extend_from_slice(b"hi");
    let err = Bundle::from_payload(forged).unwrap_err();
    assert!(err.contains("invalid path"), "{}", err);

    // A size larger than the payload.
    let mut oversized = Vec::new();
    oversized.extend_from_slice(bundle::PAYLOAD_MAGIC);
    oversized.extend_from_slice(&0u32.to_le_bytes());
    oversized.extend_from_slice(&1u32.to_le_bytes());
    oversized.extend_from_slice(&1u32.to_le_bytes());
    oversized.extend_from_slice(b"x");
    oversized.extend_from_slice(&u64::MAX.to_le_bytes());
    assert!(Bundle::from_payload(oversized).is_err());
}

#[test]
fn test_executable_trailer_roundtrip() {
    let dir = scratch_dir("trailer");
    let runtime = b"MZ pretend runtime".to_vec();
    let payload = pack(&meta(), &sample_files()).unwrap();
    let mut exe = runtime.clone();
    exe.extend_from_slice(&payload);
    exe.extend_from_slice(&bundle::trailer(payload.len() as u64));
    let path = dir.join("app.exe");
    std::fs::write(&path, &exe).unwrap();

    let bundle = Bundle::from_executable(&path).unwrap().expect("a bundle");
    assert_eq!(bundle.meta("entry"), Some("main.nyk"));
    for (file, data) in sample_files() {
        assert_eq!(bundle.read(&file).unwrap().unwrap(), data, "{}", file);
    }
    assert_eq!(bundle::strip_trailer(&exe), runtime.len());
    assert_eq!(bundle::strip_trailer(&runtime), runtime.len());

    let plain = dir.join("plain.exe");
    std::fs::write(&plain, &runtime).unwrap();
    assert!(Bundle::from_executable(&plain).unwrap().is_none());

    // A trailer claiming more payload than the file holds.
    let mut lying = runtime.clone();
    lying.extend_from_slice(&bundle::trailer(1 << 40));
    std::fs::write(&plain, &lying).unwrap();
    assert!(Bundle::from_executable(&plain).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_payload_file_and_macos_layout() {
    let dir = scratch_dir("payload_file");
    let path = dir.join(super::MACOS_PAYLOAD);
    std::fs::write(&path, pack(&meta(), &sample_files()).unwrap()).unwrap();
    let bundle = Bundle::from_payload_file(&path).unwrap();
    assert_eq!(
        bundle.read("assets/fonts/Inter.ttf").unwrap().unwrap(),
        vec![0, 1, 2, 3, 255]
    );
    let _ = std::fs::remove_dir_all(&dir);

    let exe = Path::new("/Applications/Test App.app/Contents/MacOS/test-app");
    assert_eq!(
        super::macos_payload_path(exe).unwrap(),
        Path::new("/Applications/Test App.app/Contents/Resources").join(super::MACOS_PAYLOAD)
    );
    assert!(super::macos_payload_path(Path::new("/usr/local/bin/neyuki")).is_none());
}

#[test]
fn test_icons() {
    let png = png_header(256, 256);
    assert_eq!(icon::png_size(&png), Some((256, 256)));
    assert_eq!(icon::png_size(b"GIF89a not a png at all"), None);

    let ico = icon::png_to_ico(&png).unwrap();
    assert_eq!(&ico[..6], &[0, 0, 1, 0, 1, 0]);
    assert_eq!(ico[6], 0, "256 is stored as 0");
    assert_eq!(&ico[22..], &png[..]);
    assert_eq!(icon::png_from_ico(&ico).unwrap(), png);
    assert!(icon::png_to_ico(&png_header(512, 512)).is_err());

    let icns = icon::png_to_icns(&png).unwrap();
    assert_eq!(&icns[..4], b"icns");
    assert_eq!(
        u32::from_be_bytes(icns[4..8].try_into().unwrap()) as usize,
        icns.len()
    );
    assert_eq!(&icns[8..12], b"ic08");
    assert!(icon::png_to_icns(&png_header(100, 100)).is_err());
}

#[test]
fn test_platform_detection_and_names() {
    assert_eq!(Platform::detect(b"MZ\x90\x00"), Platform::Windows);
    assert_eq!(Platform::detect(&[0xcf, 0xfa, 0xed, 0xfe]), Platform::MacOs);
    assert_eq!(Platform::detect(&[0xca, 0xfe, 0xba, 0xbe]), Platform::MacOs);
    assert_eq!(Platform::detect(b"\x7fELF"), Platform::Other);
    assert_eq!(slug("My App 2"), "my-app-2");
    assert_eq!(slug("  ***  "), "app");
}

#[test]
fn test_manifest_app_section() {
    let manifest = crate::pkg::PackageManifest::parse(
        r#"
[package]
name = "demo"
version = "2.1.0"
entry = "src/main.nyk"

[app]
name = "Demo App"
icon = "assets/icon.png"
assets = ["assets", "data/levels.json"]
console = true
"#,
    )
    .unwrap();
    assert_eq!(manifest.app.name.as_deref(), Some("Demo App"));
    assert_eq!(manifest.app.icon.as_deref(), Some("assets/icon.png"));
    assert_eq!(
        manifest.app.assets,
        Some(vec!["assets".to_string(), "data/levels.json".to_string()])
    );
    assert!(manifest.app.console);
    // `neyuki pkg add` rewrites the manifest; the section must survive.
    let again = crate::pkg::PackageManifest::parse(&manifest.serialize()).unwrap();
    assert_eq!(again.app, manifest.app);
}

#[test]
fn test_scripts_see_the_installed_bundle() {
    let _guard = INSTALL_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let files = vec![
        (
            "__app_test__/data/greeting.txt".to_string(),
            b"hello from the bundle".to_vec(),
        ),
        (
            "__app_test__/util.nyk".to_string(),
            b"return { answer = 42 }".to_vec(),
        ),
    ];
    let mut meta = meta();
    meta.insert("identifier".to_string(), "app.neyuki.test".to_string());
    super::install(Some(
        Bundle::from_payload(pack(&meta, &files).unwrap()).unwrap(),
    ));

    let script = r#"
local fs = require("@neyuki/fs")
assert(fs.read("__app_test__/data/greeting.txt") == "hello from the bundle")
assert(fs.read("./__app_test__\\data\\greeting.txt") == "hello from the bundle")
assert(fs.exists("__app_test__/data/greeting.txt"))
assert(fs.isDir("__app_test__/data"))
assert(fs.isFile("__app_test__/util.nyk"))
assert(fs.info("__app_test__/data/greeting.txt").size == 21)
assert(fs.info("__app_test__/data/greeting.txt").readonly)
assert(#fs.list("__app_test__") == 2)
assert(require("__app_test__/util").answer == 42)
assert(require("__app_test__/util") == require("__app_test__.util"))
local ok = try(fs.write, "__app_test__/data/greeting.txt", "changed")
assert(not ok, "bundled files are read-only")
ok = try(fs.remove, "__app_test__/util.nyk")
assert(not ok, "bundled files cannot be removed")
local app = os.app()
assert(app.bundled and app.name == "Test App" and app.version == "1.2.3")
assert(app.identifier == "app.neyuki.test")
assert(not fs.exists("__app_test__/missing.txt"))
"#;
    let result = crate::vm::run_bytes(
        "bundle_test.nyk",
        script.as_bytes().to_vec(),
        crate::compiler::Options::RUN,
    );
    // An entry script in a bundle folder finds modules next to it first,
    // and still finds them from the bundle root.
    let nested = crate::vm::run_bytes(
        "__app_test__/entry.nyk",
        br#"assert(require("util") == require("__app_test__/util"))"#.to_vec(),
        crate::compiler::Options::RUN,
    );
    super::install(None);
    result.unwrap();
    nested.unwrap();

    let after = crate::vm::run_bytes(
        "bundle_test.nyk",
        b"assert(not os.app().bundled)".to_vec(),
        crate::compiler::Options::RUN,
    );
    after.unwrap();
}

/// Builds a project on top of the test executable itself (a real runtime
/// for whatever platform runs the tests) and reads the result back.
#[test]
fn test_build_project() {
    let dir = scratch_dir("build");
    let project = dir.join("project");
    std::fs::create_dir_all(project.join("assets/fonts")).unwrap();
    std::fs::create_dir_all(project.join("lib")).unwrap();
    std::fs::create_dir_all(project.join(".git")).unwrap();
    std::fs::write(
        project.join("neyuki.toml"),
        "[package]\nname = \"build_test\"\nversion = \"3.4.5\"\nentry = \"main.nyk\"\n\n\
         [app]\nname = \"Build Test\"\nicon = \"icon.png\"\npublisher = \"Example\"\n",
    )
    .unwrap();
    std::fs::write(
        project.join("main.nyk"),
        "local util = require(\"lib/util\")\nprint(util)\n",
    )
    .unwrap();
    std::fs::write(project.join("lib/util.nyk"), "return 1\n").unwrap();
    std::fs::write(project.join("assets/fonts/x.ttf"), [1, 2, 3]).unwrap();
    std::fs::write(project.join(".git/config"), "ignored").unwrap();
    std::fs::write(project.join("icon.png"), png_header(64, 64)).unwrap();

    let out_name = if cfg!(windows) { "out.exe" } else { "out" };
    let options = BuildOptions {
        project: Some(project.clone()),
        out: Some(dir.join(out_name)),
        runtime: Some(std::env::current_exe().unwrap()),
        ..Default::default()
    };
    let report = build(&options).unwrap();
    assert_eq!(report.files, 3, "main.nyk, lib/util.nyk and the font");

    let bundle = match report.platform {
        Platform::MacOs => {
            let contents = report.output.join("Contents");
            let plist = std::fs::read_to_string(contents.join("Info.plist")).unwrap();
            assert!(plist.contains("<string>Build Test</string>"));
            assert!(plist.contains("<string>3.4.5</string>"));
            assert!(contents.join("Resources/AppIcon.icns").is_file());
            assert!(contents.join("MacOS/build-test").is_file());
            Bundle::from_payload_file(&contents.join("Resources").join(super::MACOS_PAYLOAD))
                .unwrap()
        }
        _ => Bundle::from_executable(&report.output)
            .unwrap()
            .expect("a bundle"),
    };
    assert_eq!(bundle.meta("name"), Some("Build Test"));
    assert_eq!(bundle.meta("version"), Some("3.4.5"));
    assert_eq!(bundle.meta("entry"), Some("main.nyk"));
    assert_eq!(bundle.meta("console"), Some("false"));
    assert_eq!(
        bundle.read("assets/fonts/x.ttf").unwrap().unwrap(),
        vec![1, 2, 3]
    );
    assert!(bundle.read("lib/util.nyk").is_some());
    assert!(bundle.read(".git/config").is_none());
    assert!(bundle.read("neyuki.toml").is_none());

    if report.platform == Platform::Windows {
        let data = std::fs::read(&report.output).unwrap();
        let image = editpe::Image::parse(data).unwrap();
        assert_eq!(
            image.subsystem(),
            editpe::constants::IMAGE_SUBSYSTEM_WINDOWS_GUI
        );
        let resources = image.resource_directory().unwrap();
        assert!(resources.get_main_icon().unwrap().is_some());
        let version = resources.get_version_info().unwrap().unwrap();
        let strings = &version.strings[0].strings;
        assert_eq!(
            strings.get("ProductName").map(String::as_str),
            Some("Build Test")
        );
        assert_eq!(
            strings.get("ProductVersion").map(String::as_str),
            Some("3.4.5")
        );
        assert_eq!(
            strings.get("CompanyName").map(String::as_str),
            Some("Example")
        );
    }

    // A syntax error in the entry fails the build.
    std::fs::write(project.join("main.nyk"), "local = = 1\n").unwrap();
    assert!(build(&options).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_build_rejects_assets_outside_the_project() {
    let dir = scratch_dir("outside");
    std::fs::write(dir.join("main.nyk"), "print(1)\n").unwrap();
    let options = BuildOptions {
        project: Some(dir.join("main.nyk")),
        out: Some(dir.join("out")),
        assets: vec!["../elsewhere".to_string()],
        runtime: Some(std::env::current_exe().unwrap()),
        ..Default::default()
    };
    let err = build(&options).unwrap_err();
    assert!(err.contains("inside the project"), "{}", err);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_unpack_extracts_files_and_decompiles_bytecode() {
    let dir = scratch_dir("unpack");
    let proto = crate::compiler::compile_program(
        "local function add(a, b) return a + b end\nreturn add(2, 3)\n",
        "lib.nyk",
        crate::compiler::Options::RUN,
    )
    .unwrap();
    let mut files = sample_files();
    files.push((
        "lib/math.nykb".to_string(),
        crate::bytecode::serialize(&proto),
    ));
    let payload = pack(&meta(), &files).unwrap();
    let mut exe = b"\x7fELF not really a runtime".to_vec();
    exe.extend_from_slice(&payload);
    exe.extend_from_slice(&bundle::trailer(payload.len() as u64));
    let exe_path = dir.join("app");
    std::fs::write(&exe_path, &exe).unwrap();

    let bundle = super::unpack::open(&exe_path).unwrap();
    let out = dir.join("out");
    let report = super::unpack::unpack(&bundle, &out, true).unwrap();
    assert_eq!(report.files, files.len());
    assert_eq!(report.decompiled, 1);
    assert!(report.meta);
    for (path, bytes) in &files {
        assert_eq!(&std::fs::read(out.join(path)).unwrap(), bytes, "{}", path);
    }
    let source = std::fs::read_to_string(out.join("lib/math.nyk")).unwrap();
    assert!(
        source.contains("function(a, b)") && source.contains("a + b"),
        "{}",
        source
    );
    let meta = std::fs::read_to_string(out.join(super::unpack::META_FILE)).unwrap();
    assert!(
        meta.contains("[app]") && meta.contains("name = \"Test App\""),
        "{}",
        meta
    );

    // The output directory now has files, so a second run refuses it.
    assert!(super::unpack::unpack(&bundle, &out, true).is_err());

    // Without decompiling, only the bundled files are written.
    let plain = dir.join("plain");
    let report = super::unpack::unpack(&bundle, &plain, false).unwrap();
    assert_eq!(report.decompiled, 0);
    assert!(!plain.join("lib/math.nyk").exists());

    // A file that is not an app is an error, not an empty directory.
    let not_app = dir.join("not_app");
    std::fs::write(&not_app, b"just text").unwrap();
    assert!(super::unpack::open(&not_app).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A package with a library for every target: a build keeps only the ones
/// its runtime loads.
#[test]
fn test_build_bundles_only_the_runtimes_native_libraries() {
    let dir = scratch_dir("native_targets");
    let project = dir.join("project");
    let package = project.join("neyuki_packages/acme/zip");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(project.join("main.nyk"), "print(1)\n").unwrap();
    let current = crate::pkg::native::current_target();
    let mut targets = vec![
        "aarch64-linux-gnu".to_string(),
        "aarch64-linux-musl".to_string(),
        "x86_64-windows".to_string(),
        "aarch64-macos".to_string(),
    ];
    if !targets.contains(&current) {
        targets.push(current.clone());
    }
    let mut manifest =
        String::from("[package]\nname = \"zip\"\nversion = \"1.0.0\"\n\n[native]\nabi = 2\n");
    for target in &targets {
        manifest.push_str(&format!("{0} = \"native/{0}/lib\"\n", target));
        std::fs::create_dir_all(package.join("native").join(target)).unwrap();
        std::fs::write(package.join("native").join(target).join("lib"), target).unwrap();
    }
    std::fs::write(package.join("neyuki.toml"), manifest).unwrap();

    // Each build gets its own output: on macOS the first one is a `.app`
    // directory, which a later single-file build could not overwrite.
    let bundled = |runtime: PathBuf| -> Vec<String> {
        let stem = runtime.file_stem().unwrap().to_string_lossy().into_owned();
        let out = dir.join(format!("out-{}{}", stem, std::env::consts::EXE_SUFFIX));
        let report = build(&BuildOptions {
            project: Some(project.clone()),
            out: Some(out),
            runtime: Some(runtime),
            ..Default::default()
        })
        .unwrap();
        let bundle = match report.platform {
            Platform::MacOs => Bundle::from_payload_file(
                &report
                    .output
                    .join("Contents/Resources")
                    .join(super::MACOS_PAYLOAD),
            )
            .unwrap(),
            _ => Bundle::from_executable(&report.output).unwrap().unwrap(),
        };
        targets
            .iter()
            .filter(|t| {
                bundle
                    .read(&format!("neyuki_packages/acme/zip/native/{}/lib", t))
                    .is_some()
            })
            .cloned()
            .collect()
    };

    // The test executable is a runtime for the running target.
    assert_eq!(bundled(std::env::current_exe().unwrap()), vec![current]);

    // A static aarch64 Linux runtime may be glibc (found through the
    // `-gnu` key) or musl.
    let mut elf = vec![0u8; 64];
    elf[..4].copy_from_slice(b"\x7fELF");
    elf[4] = 2;
    elf[5] = 1;
    elf[18..20].copy_from_slice(&0xb7u16.to_le_bytes());
    let elf_runtime = dir.join("runtime-aarch64");
    std::fs::write(&elf_runtime, &elf).unwrap();
    assert_eq!(
        bundled(elf_runtime),
        vec!["aarch64-linux-gnu", "aarch64-linux-musl"]
    );

    // A runtime this build cannot read keeps every library.
    let unknown = dir.join("runtime-unknown");
    std::fs::write(&unknown, b"#!/bin/sh\n").unwrap();
    assert_eq!(bundled(unknown), targets);
    let _ = std::fs::remove_dir_all(&dir);
}
