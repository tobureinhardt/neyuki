//! `neyuki unpack`: extracts the bundle of a built app into a directory.
//!
//! Scripts are bundled as they were in the project, so `.nyk` files come
//! back as their source. Each `.nykb` bytecode file is also written as a
//! decompiled `.nyk` next to it, and the bundle's metadata goes to
//! `bundle.toml`.

use std::fs;
use std::path::{Path, PathBuf};

use super::bundle::Bundle;

const USAGE: &str = "Usage: neyuki unpack <app> [options]

Extracts the scripts and assets bundled into an executable built with
`neyuki build` (or a macOS .app, or the app.nykpak inside one) into a
directory. Each .nykb bytecode file is also written next to itself as
decompiled .nyk source, and the app's metadata goes to bundle.toml.

Options:
  -o, --out <dir>   Output directory (default: <app>-unpacked)
  --no-decompile    Leave .nykb files as they are";

/// The name of the metadata file.
pub const META_FILE: &str = "bundle.toml";

/// What `unpack` wrote.
#[derive(Debug, Default)]
pub struct UnpackReport {
    pub out: PathBuf,
    pub files: usize,
    pub decompiled: usize,
    /// Files that could not be decompiled, with the reason.
    pub skipped: Vec<(String, String)>,
    /// Whether the metadata was written (a bundled `bundle.toml` wins).
    pub meta: bool,
}

/// Opens the bundle of an app: an executable with a payload, a macOS
/// `.app` directory or the executable inside one, or a payload file.
pub fn open(path: &Path) -> Result<Bundle, String> {
    if path.is_dir() {
        let payload = path
            .join("Contents")
            .join("Resources")
            .join(super::MACOS_PAYLOAD);
        if payload.is_file() {
            return Bundle::from_payload_file(&payload);
        }
        return Err(format!(
            "{} is a directory without an app bundle",
            path.display()
        ));
    }
    if let Some(bundle) = Bundle::from_executable(path)? {
        return Ok(bundle);
    }
    if let Some(payload) = super::macos_payload_path(path)
        && payload.is_file()
    {
        return Bundle::from_payload_file(&payload);
    }
    Bundle::from_payload_file(path).map_err(|err| format!("{}: {}", path.display(), err))
}

/// Writes every file of `bundle` under `out`, which must not exist yet or
/// be empty. With `decompile`, `.nykb` files also get a `.nyk` beside them
/// unless the bundle has one already.
pub fn unpack(bundle: &Bundle, out: &Path, decompile: bool) -> Result<UnpackReport, String> {
    if let Ok(mut entries) = fs::read_dir(out)
        && entries.next().is_some()
    {
        return Err(format!("{} already exists and is not empty", out.display()));
    }
    fs::create_dir_all(out).map_err(|err| format!("cannot create {}: {}", out.display(), err))?;
    let mut report = UnpackReport {
        out: out.to_path_buf(),
        ..UnpackReport::default()
    };
    let paths: Vec<String> = bundle.paths().map(str::to_string).collect();
    for path in &paths {
        let bytes = bundle
            .read(path)
            .ok_or_else(|| format!("'{}' is listed but not bundled", path))?
            .map_err(|err| format!("cannot read '{}' from the bundle: {}", path, err))?;
        write(out, path, &bytes)?;
        report.files += 1;

        let Some(stem) = path.strip_suffix(".nykb") else {
            continue;
        };
        let source = format!("{}.nyk", stem);
        if !decompile || paths.contains(&source) {
            continue;
        }
        match decompile_bytes(&bytes) {
            Ok(text) => {
                write(out, &source, text.as_bytes())?;
                report.decompiled += 1;
            }
            Err(err) => report.skipped.push((path.clone(), err)),
        }
    }
    if !paths.iter().any(|p| p == META_FILE) {
        write(out, META_FILE, meta_toml(bundle).as_bytes())?;
        report.meta = true;
    }
    Ok(report)
}

/// Decompiles a `.nykb` file's contents.
pub fn decompile_bytes(bytes: &[u8]) -> Result<String, String> {
    let proto = crate::bytecode::deserialize(bytes)?;
    Ok(crate::bytecode::decompile(&proto))
}

fn write(out: &Path, path: &str, bytes: &[u8]) -> Result<(), String> {
    // Bundle paths are normalized, relative and free of `..`.
    let dest = out.join(path);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("cannot create {}: {}", parent.display(), err))?;
    }
    fs::write(&dest, bytes).map_err(|err| format!("cannot write {}: {}", dest.display(), err))
}

fn toml_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The bundle's metadata as a TOML `[app]` table.
pub fn meta_toml(bundle: &Bundle) -> String {
    let mut out = String::from("# The metadata `neyuki build` stored in the app.\n[app]\n");
    for (key, value) in bundle.meta_entries() {
        let bare = !key.is_empty()
            && key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        let key = if bare {
            key.to_string()
        } else {
            toml_string(key)
        };
        out.push_str(&format!("{} = {}\n", key, toml_string(value)));
    }
    out
}

/// The default output directory: `<app>-unpacked` in the working
/// directory.
fn default_out(input: &Path) -> PathBuf {
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("app");
    PathBuf::from(format!("{}-unpacked", stem))
}

pub fn run_cli(args: &[String]) -> Result<(), String> {
    let mut input: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut decompile = true;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                println!("{}", USAGE);
                return Ok(());
            }
            "-o" | "--out" => {
                i += 1;
                let dir = args
                    .get(i)
                    .ok_or_else(|| format!("{} needs a directory\n\n{}", args[i - 1], USAGE))?;
                out = Some(PathBuf::from(dir));
            }
            "--no-decompile" => decompile = false,
            arg if arg.starts_with('-') => {
                return Err(format!("unknown option '{}'\n\n{}", arg, USAGE));
            }
            arg if input.is_none() => input = Some(PathBuf::from(arg)),
            _ => return Err(USAGE.to_string()),
        }
        i += 1;
    }
    let input = input.ok_or_else(|| USAGE.to_string())?;
    let out = out.unwrap_or_else(|| default_out(&input));
    let bundle = open(&input)?;
    let report = unpack(&bundle, &out, decompile)?;
    println!(
        "unpacked {} file{} into {}",
        report.files,
        if report.files == 1 { "" } else { "s" },
        report.out.display()
    );
    if report.decompiled > 0 {
        println!(
            "decompiled {} bytecode file{} to .nyk",
            report.decompiled,
            if report.decompiled == 1 { "" } else { "s" }
        );
    }
    for (path, err) in &report.skipped {
        eprintln!("warning: could not decompile {}: {}", path, err);
    }
    if !report.meta {
        eprintln!(
            "warning: the bundle has its own {}, so the app metadata was not written",
            META_FILE
        );
    }
    Ok(())
}
