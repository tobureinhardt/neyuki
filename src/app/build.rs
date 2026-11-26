//! `neyuki build`: packages a project as a standalone desktop executable.
//!
//! The output is a copy of a Neyuki runtime (this executable, or the one
//! given with `--runtime`) with the bundle appended. Its format decides the
//! rest: a Windows runtime gets the app's icon, version resource and the GUI
//! subsystem; a macOS runtime goes into a `.app` bundle with the payload as
//! a separate file; any other runtime is written as a single executable.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use super::bundle::{self, normalize};
use super::icon;
use crate::pkg::{PackageManifest, Version};

const USAGE: &str = "Usage: neyuki build [project] [options]

Builds the project in the given directory (default: the current one) or the
given entry script into a standalone executable. Settings come from the
[app] section of neyuki.toml; options override them.

Options:
  -o, --out <path>      Output file or .app directory (default: dist/<name>)
  --name <name>         Product name (default: [app] name or package name)
  --version <x.y.z>     App version (default: [package] version)
  --icon <file>         .ico or .png for Windows, .icns or .png for macOS
  --asset <path>        Bundle a file or directory too (repeatable)
  --console             Keep the console window on Windows
  --runtime <file>      Build on this Neyuki executable instead of the
                        running one, e.g. to build for another OS";

/// What to build. Relative paths are taken from the working directory,
/// except `assets`, which are inside the project like their bundle paths.
#[derive(Clone, Debug, Default)]
pub struct BuildOptions {
    pub project: Option<PathBuf>,
    pub out: Option<PathBuf>,
    pub name: Option<String>,
    pub version: Option<String>,
    pub icon: Option<PathBuf>,
    pub assets: Vec<String>,
    pub console: bool,
    pub runtime: Option<PathBuf>,
}

/// The kinds of runtime executable `neyuki build` knows how to package.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Windows,
    MacOs,
    /// ELF and anything else: the bundle is appended and nothing else changes.
    Other,
}

impl Platform {
    /// Recognizes an executable by its first bytes.
    pub fn detect(bytes: &[u8]) -> Self {
        match bytes.get(..4) {
            Some([b'M', b'Z', _, _]) => Self::Windows,
            // Mach-O 32/64-bit in either byte order, and universal binaries.
            Some([0xfe, 0xed, 0xfa, 0xce | 0xcf])
            | Some([0xce | 0xcf, 0xfa, 0xed, 0xfe])
            | Some([0xca, 0xfe, 0xba, 0xbe]) => Self::MacOs,
            _ => Self::Other,
        }
    }
}

/// What `build` wrote.
#[derive(Debug)]
pub struct BuildReport {
    pub output: PathBuf,
    pub platform: Platform,
    pub files: usize,
    pub bytes: u64,
    /// Whether a Windows build keeps its console window.
    pub console: bool,
}

pub fn run_cli(args: &[String]) -> Result<(), String> {
    let mut options = BuildOptions::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let mut value = |flag: &str| {
            iter.next()
                .cloned()
                .ok_or_else(|| format!("{} expects a value\n\n{}", flag, USAGE))
        };
        match arg.as_str() {
            "-h" | "--help" | "help" => {
                println!("{}", USAGE);
                return Ok(());
            }
            "-o" | "--out" => options.out = Some(value(arg)?.into()),
            "--name" => options.name = Some(value(arg)?),
            "--version" => options.version = Some(value(arg)?),
            "--icon" => options.icon = Some(value(arg)?.into()),
            "--asset" => options.assets.push(value(arg)?),
            "--console" => options.console = true,
            "--runtime" => options.runtime = Some(value(arg)?.into()),
            flag if flag.starts_with('-') => {
                return Err(format!("unknown option '{}'\n\n{}", flag, USAGE));
            }
            project if options.project.is_none() => options.project = Some(project.into()),
            extra => return Err(format!("unexpected argument '{}'\n\n{}", extra, USAGE)),
        }
    }
    let report = build(&options)?;
    let kind = match report.platform {
        Platform::Windows if report.console => "Windows console app",
        Platform::Windows => "Windows app",
        Platform::MacOs => "macOS app bundle",
        Platform::Other => "executable",
    };
    println!(
        "built {} ({}, {} bundled files, {:.1} MiB)",
        report.output.display(),
        kind,
        report.files,
        report.bytes as f64 / (1024.0 * 1024.0)
    );
    Ok(())
}

/// The project being built: its root, its entry inside the root and its
/// manifest (a default one when there is no `neyuki.toml`).
struct Project {
    root: PathBuf,
    entry: String,
    manifest: PackageManifest,
    has_manifest: bool,
}

fn load_project(options: &BuildOptions) -> Result<Project, String> {
    let target = options
        .project
        .clone()
        .unwrap_or_else(|| PathBuf::from("."));
    let (root, entry_override) = if target.is_file() {
        let root = match target.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let name = target
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("invalid entry path {}", target.display()))?;
        (root, Some(name.to_string()))
    } else if target.is_dir() {
        (target, None)
    } else {
        return Err(format!("{} does not exist", target.display()));
    };

    let manifest_path = root.join("neyuki.toml");
    let has_manifest = manifest_path.is_file();
    let manifest = if has_manifest {
        let text = fs::read_to_string(&manifest_path)
            .map_err(|err| format!("cannot read {}: {}", manifest_path.display(), err))?;
        PackageManifest::parse(&text)
            .map_err(|err| format!("{}: {}", manifest_path.display(), err))?
    } else {
        PackageManifest::default()
    };
    let entry_raw = entry_override.unwrap_or_else(|| manifest.entry.clone());
    let entry = normalize(&entry_raw).ok_or_else(|| {
        format!(
            "the entry script '{}' must be a path inside the project",
            entry_raw
        )
    })?;
    if !root.join(&entry).is_file() {
        return Err(format!(
            "entry script {} not found",
            root.join(&entry).display()
        ));
    }
    Ok(Project {
        root,
        entry,
        manifest,
        has_manifest,
    })
}

/// Builds the app and returns where it went.
pub fn build(options: &BuildOptions) -> Result<BuildReport, String> {
    let project = load_project(options)?;
    let app = &project.manifest.app;

    let name = options
        .name
        .clone()
        .or_else(|| app.name.clone())
        .or_else(|| project.has_manifest.then(|| project.manifest.name.clone()))
        .unwrap_or_else(|| {
            Path::new(&project.entry)
                .file_stem()
                .map_or("app".to_string(), |s| s.to_string_lossy().into_owned())
        });
    let version_text = match &options.version {
        Some(version) => version.clone(),
        None => project.manifest.version.to_string(),
    };
    let version = Version::from_str(&version_text).map_err(|err| format!("--version: {}", err))?;
    let identifier = app
        .identifier
        .clone()
        .unwrap_or_else(|| format!("app.neyuki.{}", slug(&name)));
    let console = options.console || app.console;
    let icon_path = options
        .icon
        .clone()
        .or_else(|| app.icon.as_ref().map(|icon| project.root.join(icon)));

    check_entry(&project)?;

    let runtime_path = match &options.runtime {
        Some(path) => path.clone(),
        None => std::env::current_exe()
            .map_err(|err| format!("cannot find the Neyuki runtime: {}", err))?,
    };
    let mut runtime = fs::read(&runtime_path)
        .map_err(|err| format!("cannot read runtime {}: {}", runtime_path.display(), err))?;
    runtime.truncate(bundle::strip_trailer(&runtime));
    let platform = Platform::detect(&runtime);
    // The running executable is the runtime unless --runtime names another,
    // whose header says what it is.
    let targets = match &options.runtime {
        None => Some(vec![crate::pkg::native::current_target()]),
        Some(_) => super::target::runtime_targets(&runtime),
    };

    let files = collect_files(
        &project,
        &options.assets,
        options.out.as_deref(),
        targets.as_deref(),
    )?;

    let mut meta = BTreeMap::new();
    meta.insert("name".to_string(), name.clone());
    meta.insert("version".to_string(), version.to_string());
    meta.insert("identifier".to_string(), identifier.clone());
    meta.insert("entry".to_string(), project.entry.clone());
    meta.insert("console".to_string(), console.to_string());
    meta.insert("neyuki".to_string(), env!("CARGO_PKG_VERSION").to_string());
    let mut contents = Vec::with_capacity(files.len());
    for (bundle_path, disk_path) in &files {
        let data = fs::read(disk_path)
            .map_err(|err| format!("cannot read {}: {}", disk_path.display(), err))?;
        contents.push((bundle_path.clone(), data));
    }
    let payload = bundle::pack(&meta, &contents)?;

    let icon = match &icon_path {
        Some(path) => Some(
            fs::read(path)
                .map_err(|err| format!("cannot read icon {}: {}", path.display(), err))?,
        ),
        None => None,
    };

    let info = AppInfo {
        name: &name,
        version: &version,
        identifier: &identifier,
        console,
        publisher: app.publisher.as_deref(),
        copyright: app.copyright.as_deref(),
        icon: icon.as_deref().zip(icon_path.as_deref()),
    };
    let output = match platform {
        Platform::Windows => {
            let out = options
                .out
                .clone()
                .unwrap_or_else(|| Path::new("dist").join(format!("{}.exe", file_name(&name))));
            let image = windows_image(&runtime, &info, &out)?;
            let trailer = bundle::trailer(payload.len() as u64);
            write_executable(&out, &[&image[..], &payload[..], &trailer[..]])?;
            out
        }
        Platform::Other => {
            let out = options
                .out
                .clone()
                .unwrap_or_else(|| Path::new("dist").join(slug(&name)));
            let trailer = bundle::trailer(payload.len() as u64);
            write_executable(&out, &[&runtime[..], &payload[..], &trailer[..]])?;
            out
        }
        Platform::MacOs => {
            let out = options
                .out
                .clone()
                .unwrap_or_else(|| Path::new("dist").join(format!("{}.app", file_name(&name))));
            write_macos_bundle(&out, &runtime, &payload, &info)?;
            out
        }
    };
    let bytes = match platform {
        Platform::MacOs => (runtime.len() + payload.len()) as u64,
        _ => fs::metadata(&output).map(|m| m.len()).unwrap_or_default(),
    };
    Ok(BuildReport {
        output,
        platform,
        files: files.len(),
        bytes,
        console,
    })
}

/// Compiles the entry so a syntax or type error fails the build rather than
/// the app.
fn check_entry(project: &Project) -> Result<(), String> {
    let path = project.root.join(&project.entry);
    let bytes =
        fs::read(&path).map_err(|err| format!("cannot read {}: {}", path.display(), err))?;
    if bytes.starts_with(crate::bytecode::MAGIC) {
        let proto = crate::bytecode::deserialize(&bytes)?;
        return crate::bytecode::verify_proto(&proto)
            .map_err(|err| format!("{}: bytecode verification failed: {}", path.display(), err));
    }
    let source =
        String::from_utf8(bytes).map_err(|_| format!("{} is not valid UTF-8", path.display()))?;
    crate::compiler::compile_program(&source, &project.entry, crate::compiler::Options::RUN)
        .map(drop)
}

/// Bundle path → file on disk: every `.nyk` and `.nykb` file in the project
/// (so `require` finds local modules), the manifests of installed registry
/// packages and the native libraries the runtime loads (see
/// `add_native_libraries`), plus the assets. Hidden entries, `target`,
/// `dist` and the output itself are skipped.
fn collect_files(
    project: &Project,
    extra_assets: &[String],
    out: Option<&Path>,
    targets: Option<&[String]>,
) -> Result<BTreeMap<String, PathBuf>, String> {
    let root = &project.root;
    let skip = out.and_then(|out| fs::canonicalize(out).ok());
    let mut files = BTreeMap::new();

    let mut scripts = Vec::new();
    walk(root, root, &mut scripts, true)?;
    for (bundle_path, disk_path) in scripts {
        // A registry package's neyuki.toml names its entry file, which
        // `require("@pkg/user/name")` reads inside the app too.
        let package_manifest =
            bundle_path.starts_with("neyuki_packages/") && bundle_path.ends_with("/neyuki.toml");
        if package_manifest {
            add_native_libraries(&bundle_path, &disk_path, targets, &mut files);
        }
        if bundle_path.ends_with(".nyk") || bundle_path.ends_with(".nykb") || package_manifest {
            files.insert(bundle_path, disk_path);
        }
    }

    let mut assets: Vec<String> = match &project.manifest.app.assets {
        Some(list) => list.clone(),
        None if root.join("assets").is_dir() => vec!["assets".to_string()],
        None => Vec::new(),
    };
    assets.extend(extra_assets.iter().cloned());
    for asset in assets {
        let normalized = normalize(&asset).ok_or_else(|| {
            format!(
                "asset '{}' must be a relative path inside the project",
                asset
            )
        })?;
        let disk = root.join(&normalized);
        if disk.is_dir() {
            let mut found = Vec::new();
            walk(root, &disk, &mut found, false)?;
            files.extend(found);
        } else if disk.is_file() {
            files.insert(normalized, disk);
        } else {
            return Err(format!("asset {} not found", disk.display()));
        }
    }

    if let Some(skip) = skip {
        files.retain(|_, disk| fs::canonicalize(disk).ok().as_ref() != Some(&skip));
    }
    Ok(files)
}

/// Bundles the native libraries from a package's `[native]` section that
/// the runtime loads: for each of `targets`, the one library it would pick.
/// Libraries for other platforms would only make the app bigger. With no
/// `targets` (a runtime whose header is not recognized) every library goes
/// in, and the app picks its own when it runs.
fn add_native_libraries(
    manifest_bundle_path: &str,
    manifest_disk_path: &Path,
    targets: Option<&[String]>,
    files: &mut BTreeMap<String, PathBuf>,
) {
    let Some(native) = fs::read_to_string(manifest_disk_path)
        .ok()
        .and_then(|text| crate::pkg::PackageManifest::parse(&text).ok())
        .and_then(|manifest| manifest.native)
    else {
        return;
    };
    let (Some(bundle_dir), Some(disk_dir)) = (
        manifest_bundle_path.rsplit_once('/').map(|(dir, _)| dir),
        manifest_disk_path.parent(),
    ) else {
        return;
    };
    let libraries: Vec<&str> = match targets {
        Some(targets) => targets
            .iter()
            .filter_map(|target| native.library_for(target))
            .collect(),
        None => native.targets.values().map(String::as_str).collect(),
    };
    for library in libraries {
        let disk = crate::pkg::native::library_in(disk_dir, library);
        if disk.is_file() {
            files.insert(format!("{}/{}", bundle_dir, library), disk);
        }
    }
}

/// Collects the files under `dir` as (path relative to `root`, disk path).
/// `top_level_filter` skips `target` and `dist`, which only matters for the
/// project-wide walk.
fn walk(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(String, PathBuf)>,
    top_level_filter: bool,
) -> Result<(), String> {
    let entries =
        fs::read_dir(dir).map_err(|err| format!("cannot list {}: {}", dir.display(), err))?;
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            if top_level_filter && dir == root && matches!(&*name, "target" | "dist") {
                continue;
            }
            walk(root, &path, out, top_level_filter)?;
        } else if path.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|_| format!("{} is outside the project", path.display()))?;
            let relative = relative.to_string_lossy();
            let bundle_path = normalize(&relative)
                .ok_or_else(|| format!("{} cannot be bundled", path.display()))?;
            out.push((bundle_path, path));
        }
    }
    Ok(())
}

/// The metadata the platform writers need.
struct AppInfo<'a> {
    name: &'a str,
    version: &'a Version,
    identifier: &'a str,
    console: bool,
    publisher: Option<&'a str>,
    copyright: Option<&'a str>,
    /// The icon's bytes and where they came from (for its extension).
    icon: Option<(&'a [u8], &'a Path)>,
}

fn icon_extension(path: &Path) -> String {
    path.extension()
        .map(|ext| ext.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// The runtime with the app's icon, version resource and subsystem.
fn windows_image(runtime: &[u8], info: &AppInfo, out: &Path) -> Result<Vec<u8>, String> {
    use editpe::constants::{IMAGE_SUBSYSTEM_WINDOWS_CUI, IMAGE_SUBSYSTEM_WINDOWS_GUI};
    use editpe::types::FixedFileInfo;
    use editpe::types::{VersionU16, VersionU32};
    use editpe::{Image, VersionInfo, VersionStringTable};

    let pe_err = |err: &dyn std::fmt::Display| format!("cannot edit the Windows runtime: {}", err);
    let mut image = Image::parse(runtime.to_vec()).map_err(|e| pe_err(&e))?;
    let mut resources = image.resource_directory().cloned().unwrap_or_default();

    if let Some((bytes, path)) = info.icon {
        let ico = match icon_extension(path).as_str() {
            "ico" => bytes.to_vec(),
            "png" => icon::png_to_ico(bytes)?,
            _ => {
                return Err(format!(
                    "{}: a Windows icon must be an .ico or .png file",
                    path.display()
                ));
            }
        };
        resources.set_main_icon(ico).map_err(|e| pe_err(&e))?;
    }

    let v = info.version;
    let clamp = |n: u64| n.min(u16::MAX as u64) as u32;
    let packed = VersionU32 {
        major: (clamp(v.major) << 16) | clamp(v.minor),
        minor: clamp(v.patch) << 16,
    };
    let file_name = out
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let mut table = VersionStringTable {
        // US English, Unicode.
        key: "040904B0".to_string(),
        ..Default::default()
    };
    let mut add = |key: &str, value: &str| {
        table.strings.insert(key.to_string(), value.to_string());
    };
    if let Some(publisher) = info.publisher {
        add("CompanyName", publisher);
    }
    add("FileDescription", info.name);
    add("FileVersion", &v.to_string());
    add("InternalName", info.name);
    if let Some(copyright) = info.copyright {
        add("LegalCopyright", copyright);
    }
    add("OriginalFilename", &file_name);
    add("ProductName", info.name);
    add("ProductVersion", &v.to_string());
    let version_info = VersionInfo {
        info: FixedFileInfo {
            file_version: packed,
            product_version: packed,
            ..Default::default()
        },
        strings: vec![table],
        vars: vec![VersionU16 {
            major: 0x0409,
            minor: 0x04B0,
        }],
    };
    resources
        .set_version_info(&version_info)
        .map_err(|e| pe_err(&e))?;
    image
        .set_resource_directory(resources)
        .map_err(|e| pe_err(&e))?;
    image.set_subsystem(if info.console {
        IMAGE_SUBSYSTEM_WINDOWS_CUI
    } else {
        IMAGE_SUBSYSTEM_WINDOWS_GUI
    });
    Ok(image.data().to_vec())
}

/// Writes `parts` back to back to `out` and marks it executable.
fn write_executable(out: &Path, parts: &[&[u8]]) -> Result<(), String> {
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|err| format!("cannot create {}: {}", parent.display(), err))?;
    }
    fs::write(out, parts.concat())
        .map_err(|err| format!("cannot write {}: {}", out.display(), err))?;
    make_executable(out)
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .map_err(|err| format!("cannot make {} executable: {}", path.display(), err))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// Writes `Name.app`: the unmodified runtime in `Contents/MacOS`, the
/// payload and icon in `Contents/Resources` and an `Info.plist`.
fn write_macos_bundle(
    out: &Path,
    runtime: &[u8],
    payload: &[u8],
    info: &AppInfo,
) -> Result<(), String> {
    let contents = out.join("Contents");
    let executable = slug(info.name);
    let icon_file = match info.icon {
        Some((bytes, path)) => {
            let icns = match icon_extension(path).as_str() {
                "icns" => bytes.to_vec(),
                "png" => icon::png_to_icns(bytes)?,
                "ico" => icon::png_from_ico(bytes)
                    .ok_or_else(|| {
                        format!(
                            "{}: the .ico holds no PNG image to use on macOS; give an .icns or .png icon",
                            path.display()
                        )
                    })
                    .and_then(|png| icon::png_to_icns(&png))?,
                _ => {
                    return Err(format!(
                        "{}: a macOS icon must be an .icns, .png or .ico file",
                        path.display()
                    ));
                }
            };
            Some(icns)
        }
        None => None,
    };

    let create = |dir: &Path| {
        fs::create_dir_all(dir).map_err(|err| format!("cannot create {}: {}", dir.display(), err))
    };
    create(&contents.join("MacOS"))?;
    create(&contents.join("Resources"))?;
    write_executable(&contents.join("MacOS").join(&executable), &[runtime])?;
    let write = |path: PathBuf, data: &[u8]| {
        fs::write(&path, data).map_err(|err| format!("cannot write {}: {}", path.display(), err))
    };
    write(
        contents.join("Resources").join(super::MACOS_PAYLOAD),
        payload,
    )?;
    if let Some(icns) = &icon_file {
        write(contents.join("Resources").join("AppIcon.icns"), icns)?;
    }
    write(
        contents.join("Info.plist"),
        info_plist(info, &executable, icon_file.is_some()).as_bytes(),
    )
}

fn info_plist(info: &AppInfo, executable: &str, has_icon: bool) -> String {
    let mut keys = vec![
        ("CFBundleDevelopmentRegion", "en".to_string()),
        ("CFBundleDisplayName", info.name.to_string()),
        ("CFBundleExecutable", executable.to_string()),
        ("CFBundleIdentifier", info.identifier.to_string()),
        ("CFBundleInfoDictionaryVersion", "6.0".to_string()),
        ("CFBundleName", info.name.to_string()),
        ("CFBundlePackageType", "APPL".to_string()),
        (
            "CFBundleShortVersionString",
            format!(
                "{}.{}.{}",
                info.version.major, info.version.minor, info.version.patch
            ),
        ),
        ("CFBundleVersion", info.version.to_string()),
    ];
    if has_icon {
        keys.push(("CFBundleIconFile", "AppIcon".to_string()));
    }
    if let Some(copyright) = info.copyright {
        keys.push(("NSHumanReadableCopyright", copyright.to_string()));
    }
    let mut plist = String::from(concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" ",
        "\"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
        "<plist version=\"1.0\">\n<dict>\n"
    ));
    for (key, value) in keys {
        plist.push_str(&format!(
            "\t<key>{}</key>\n\t<string>{}</string>\n",
            key,
            xml_escape(&value)
        ));
    }
    plist.push_str("\t<key>NSHighResolutionCapable</key>\n\t<true/>\n</dict>\n</plist>\n");
    plist
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Lower-case letters, digits and hyphens: `My App 2` → `my-app-2`.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() {
        "app".to_string()
    } else {
        out
    }
}

/// `name` without characters that Windows or macOS refuse in file names.
fn file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c: &char| {
            !matches!(*c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') && !c.is_control()
        })
        .collect();
    let cleaned = cleaned.trim().trim_end_matches('.').to_string();
    if cleaned.is_empty() {
        "app".to_string()
    } else {
        cleaned
    }
}
