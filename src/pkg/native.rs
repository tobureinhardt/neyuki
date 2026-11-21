//! Native packages: prebuilt extension libraries shipped inside a package.
//!
//! A package declares them in `neyuki.toml`:
//!
//! ```toml
//! [native]
//! abi = 1
//! x86_64-linux   = "native/x86_64-linux/libzlib.so"
//! aarch64-macos  = "native/aarch64-macos/libzlib.dylib"
//! x86_64-windows = "native/x86_64-windows/zlib.dll"
//! ```
//!
//! `abi` is the extension ABI the libraries were built against
//! (`NEYUKI_EXT_ABI_VERSION`); every other key is a target (`<arch>-<os>`,
//! with `-musl` for musl Linux) mapped to a file inside the package. One
//! archive carries every target, so the registry, lockfile checksums and
//! install path stay the same as for plain packages.
//!
//! `require("@pkg/<user>/<name>/native")` loads the library for the running
//! target, as does `require("@pkg/<user>/<name>")` when the package has no
//! Neyuki entry file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::vm::libs::ffi::ext::NEYUKI_EXT_ABI_VERSION;

/// The module name under a package that loads its native library.
pub const NATIVE_MODULE: &str = "native";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NativeManifest {
    /// The extension ABI the libraries were built for.
    pub abi: u32,
    /// Target name -> library path inside the package.
    pub targets: BTreeMap<String, String>,
}

/// The target this runtime was built for, as `[native]` keys name it:
/// `x86_64-linux`, `aarch64-macos`, `x86_64-windows`, `x86_64-linux-musl`.
pub fn current_target() -> String {
    let os = std::env::consts::OS;
    if cfg!(all(target_os = "linux", target_env = "musl")) {
        format!("{}-{}-musl", std::env::consts::ARCH, os)
    } else {
        format!("{}-{}", std::env::consts::ARCH, os)
    }
}

/// Keys accepted for the running target, preferred first. glibc Linux also
/// answers to `<arch>-linux-gnu`.
fn target_keys() -> Vec<String> {
    let main = current_target();
    let mut keys = vec![main.clone()];
    if cfg!(all(target_os = "linux", not(target_env = "musl"))) {
        keys.push(format!("{}-gnu", main));
    }
    keys
}

/// A path inside the package: relative, `/`-separated, no `..`.
pub fn valid_library_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.contains(':')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

impl NativeManifest {
    /// Reads one `key = value` line of the `[native]` section.
    pub fn parse_line(&mut self, key: &str, value: &str) -> Result<(), String> {
        if key == "abi" {
            self.abi = value
                .parse()
                .map_err(|_| format!("[native] abi must be a number, got '{}'", value))?;
            return Ok(());
        }
        if !valid_library_path(value) {
            return Err(format!(
                "[native] {} = \"{}\": the library must be a relative path inside the package",
                key, value
            ));
        }
        self.targets.insert(key.to_string(), value.to_string());
        Ok(())
    }

    pub fn serialize(&self, out: &mut String) {
        out.push_str("[native]\n");
        out.push_str(&format!("abi = {}\n", self.abi));
        for (target, path) in &self.targets {
            out.push_str(&format!("{} = \"{}\"\n", target, path));
        }
        out.push('\n');
    }

    /// The library path for the running target, or an error that names the
    /// targets the package does ship.
    pub fn library_for_current_target(&self, package: &str) -> Result<&str, String> {
        if self.abi != NEYUKI_EXT_ABI_VERSION {
            return Err(format!(
                "{} was built for native extension ABI {}, but this Neyuki speaks ABI {}; \
                 install a version of the package built for it",
                package, self.abi, NEYUKI_EXT_ABI_VERSION
            ));
        }
        target_keys()
            .iter()
            .find_map(|key| self.targets.get(key))
            .map(String::as_str)
            .ok_or_else(|| {
                let shipped: Vec<&str> = self.targets.keys().map(String::as_str).collect();
                format!(
                    "{} has no native library for {} (it ships: {})",
                    package,
                    current_target(),
                    if shipped.is_empty() {
                        "none".to_string()
                    } else {
                        shipped.join(", ")
                    }
                )
            })
    }

    /// A problem `neyuki pkg install` should warn about, if any.
    pub fn install_warning(&self, package: &str) -> Option<String> {
        self.library_for_current_target(package).err()
    }
}

/// Where a library from a built app's bundle is written so the OS loader
/// can open it: `<neyuki home>/native-cache/<sha256>/<file name>`. The hash
/// covers the bytes, so a changed library never reuses a stale copy and two
/// apps shipping the same library share it.
pub fn extract_bundled_library(file_name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let hash = super::sha256_hex(bytes);
    let dir = super::paths::neyuki_home()?
        .join("native-cache")
        .join(&hash[..32]);
    let path = dir.join(file_name);
    if std::fs::read(&path).is_ok_and(|existing| super::sha256_hex(&existing) == hash) {
        return Ok(path);
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {}", dir.display(), e))?;
    // Written under a temporary name and renamed, so a concurrent start never
    // loads a half-written library.
    let staging = dir.join(format!(".{}.{}.tmp", file_name, std::process::id()));
    std::fs::write(&staging, bytes)
        .map_err(|e| format!("cannot write {}: {}", staging.display(), e))?;
    std::fs::rename(&staging, &path).map_err(|e| {
        let _ = std::fs::remove_file(&staging);
        format!("cannot write {}: {}", path.display(), e)
    })?;
    Ok(path)
}

/// The file name part of a `/`-separated package path.
pub fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Checks a package's `[native]` section against the files being published.
pub fn check_publish(native: &NativeManifest, files: &[(String, Vec<u8>)]) -> Result<(), String> {
    if native.abi == 0 {
        return Err(format!(
            "[native] needs abi = {} (the extension ABI the libraries were built for)",
            NEYUKI_EXT_ABI_VERSION
        ));
    }
    if native.targets.is_empty() {
        return Err(
            "[native] lists no libraries; add e.g. x86_64-linux = \"native/libfoo.so\"".to_string(),
        );
    }
    for (target, path) in &native.targets {
        if !files.iter().any(|(p, _)| p == path) {
            return Err(format!(
                "the native library for {} ('{}') is not in the package; build it first, \
                 or check .neyukiignore",
                target, path
            ));
        }
    }
    Ok(())
}

/// Resolves a native library path inside an installed package directory.
pub fn library_in(dir: &Path, relative: &str) -> PathBuf {
    relative
        .split('/')
        .fold(dir.to_path_buf(), |p, part| p.join(part))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(targets: &[(&str, &str)]) -> NativeManifest {
        NativeManifest {
            abi: NEYUKI_EXT_ABI_VERSION,
            targets: targets
                .iter()
                .map(|(t, p)| (t.to_string(), p.to_string()))
                .collect(),
        }
    }

    #[test]
    fn picks_the_running_target() {
        let target = current_target();
        let m = manifest(&[(&target, "native/lib.so"), ("sparc-plan9", "native/x.so")]);
        assert_eq!(
            m.library_for_current_target("@a/b").unwrap(),
            "native/lib.so"
        );
    }

    #[test]
    fn explains_a_missing_target_and_abi_mismatch() {
        let m = manifest(&[("sparc-plan9", "native/x.so")]);
        let err = m.library_for_current_target("@a/b").unwrap_err();
        assert!(
            err.contains(&current_target()) && err.contains("sparc-plan9"),
            "{err}"
        );

        let mut m = manifest(&[(&current_target(), "native/x.so")]);
        m.abi = NEYUKI_EXT_ABI_VERSION + 1;
        let err = m.library_for_current_target("@a/b").unwrap_err();
        assert!(err.contains("ABI"), "{err}");
    }

    #[test]
    fn library_paths_stay_inside_the_package() {
        assert!(valid_library_path("native/x86_64-linux/libz.so"));
        for bad in [
            "",
            "/etc/x.so",
            "../x.so",
            "a/../../x.so",
            "C:/x.dll",
            "a\\b.dll",
            "a//b",
        ] {
            assert!(!valid_library_path(bad), "{bad}");
        }
        let mut m = NativeManifest::default();
        assert!(m.parse_line("x86_64-linux", "../evil.so").is_err());
        assert!(m.parse_line("abi", "one").is_err());
    }

    #[test]
    fn manifest_section_round_trips() {
        let text = "[package]\nname = \"@a/z\"\nversion = \"1.0.0\"\n\n[native]\nabi = 1\nx86_64-linux = \"native/libz.so\"\naarch64-macos = \"native/libz.dylib\"\n";
        let parsed = crate::pkg::PackageManifest::parse(text).unwrap();
        let native = parsed.native.clone().unwrap();
        assert_eq!(native.abi, 1);
        assert_eq!(native.targets["aarch64-macos"], "native/libz.dylib");
        let again = crate::pkg::PackageManifest::parse(&parsed.serialize()).unwrap();
        assert_eq!(again, parsed);
        assert!(
            crate::pkg::PackageManifest::parse("[native]\nx86_64-linux = \"/abs.so\"\n").is_err()
        );
    }

    #[test]
    fn publish_needs_every_listed_library() {
        let m = manifest(&[("x86_64-linux", "native/libz.so")]);
        assert!(check_publish(&m, &[]).is_err());
        assert!(check_publish(&m, &[("native/libz.so".into(), vec![0x7f])]).is_ok());
    }
}
