//! Installing resolved packages into a packages directory.
//!
//! A project installs into `<project>/neyuki_packages`; global installs use
//! the global packages directory, which keeps its own `neyuki.toml` and
//! `neyuki.lock` listing what was installed with `-g`. Each package goes to
//! `<root>/<scope>/<name>/` with a `.neyuki-installed` marker recording what
//! is there, so unchanged packages are not downloaded again.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use super::lockfile::{LockedPackage, Lockfile};
use super::manifest::PackageManifest;
use super::paths::{LOCKFILE, MANIFEST, PackageName};
use super::registry::Registry;
use super::resolver::DependencyResolver;
use super::tarball;

const MARKER: &str = ".neyuki-installed";

/// Where one install reads its manifest and lockfile and puts packages.
pub struct Target {
    pub manifest: PathBuf,
    pub lockfile: PathBuf,
    pub packages: PathBuf,
}

impl Target {
    pub fn project(root: &Path) -> Self {
        Self {
            manifest: root.join(MANIFEST),
            lockfile: root.join(LOCKFILE),
            packages: root.join(super::paths::PACKAGES_DIR),
        }
    }

    pub fn global(root: &Path) -> Self {
        Self {
            manifest: root.join(MANIFEST),
            lockfile: root.join(LOCKFILE),
            packages: root.to_path_buf(),
        }
    }
}

#[derive(Default, Debug)]
pub struct Report {
    pub installed: Vec<String>,
    pub unchanged: usize,
    pub removed: Vec<String>,
}

fn marker_text(pkg: &LockedPackage) -> String {
    format!("{} {} {}\n", pkg.name, pkg.version, pkg.checksum)
}

pub fn read_lockfile(path: &Path) -> Result<Option<Lockfile>, String> {
    match fs::read_to_string(path) {
        Ok(text) => Lockfile::parse(&text).map(Some),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("cannot read {}: {}", path.display(), err)),
    }
}

/// Resolves the manifest's dependencies (keeping locked versions unless
/// `update`), installs them, removes packages no longer needed and writes
/// the lockfile. With `frozen`, the lockfile must already be complete and is
/// never rewritten.
pub fn install(
    target: &Target,
    registry: &Registry,
    update: bool,
    frozen: bool,
) -> Result<Report, String> {
    let manifest = match fs::read_to_string(&target.manifest) {
        Ok(text) => PackageManifest::parse(&text)?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => PackageManifest::default(),
        Err(err) => {
            return Err(format!(
                "cannot read {}: {}",
                target.manifest.display(),
                err
            ));
        }
    };
    let old = read_lockfile(&target.lockfile)?;

    let mut resolver = DependencyResolver::new(|name: &PackageName| registry.package(name));
    let lock = resolver.resolve(
        &manifest.dependencies,
        if update { None } else { old.as_ref() },
    )?;

    if frozen && old.as_ref() != Some(&lock) {
        return Err(format!(
            "{} is out of date with {}; run 'neyuki pkg install' without --frozen",
            LOCKFILE, MANIFEST
        ));
    }

    // A locked version must still be the bytes it was when it was locked.
    if let Some(old) = &old {
        for pkg in &lock.packages {
            if let Some(prev) = old.get_package(&pkg.name)
                && prev.version == pkg.version
                && !prev.checksum.is_empty()
                && !prev.checksum.eq_ignore_ascii_case(&pkg.checksum)
            {
                return Err(format!(
                    "the registry's checksum for {}@{} differs from {} ({} vs {}). \
                     Refusing to install it",
                    pkg.name, pkg.version, LOCKFILE, pkg.checksum, prev.checksum
                ));
            }
        }
    }

    let report = apply(&lock, &target.packages, registry)?;
    if !frozen && old.as_ref() != Some(&lock) {
        fs::write(&target.lockfile, lock.serialize())
            .map_err(|e| format!("cannot write {}: {}", target.lockfile.display(), e))?;
    }
    Ok(report)
}

/// Makes `root` hold exactly the packages in `lock`.
fn apply(lock: &Lockfile, root: &Path, registry: &Registry) -> Result<Report, String> {
    let mut report = Report::default();
    let mut wanted: BTreeMap<PackageName, &LockedPackage> = BTreeMap::new();
    for pkg in &lock.packages {
        wanted.insert(PackageName::from_str(&pkg.name)?, pkg);
    }

    for (name, pkg) in &wanted {
        let dir = name.dir_in(root);
        if fs::read_to_string(dir.join(MARKER)).ok().as_deref() == Some(marker_text(pkg).as_str()) {
            report.unchanged += 1;
            continue;
        }
        let archive = registry.download(name, &pkg.version, &pkg.checksum)?;
        place(&archive, &dir, &marker_text(pkg))?;
        report
            .installed
            .push(format!("{}@{}", pkg.name, pkg.version));
    }

    // Remove what an earlier install put here and is no longer wanted. Only
    // directories with a marker are touched, never files someone put there.
    for (name, dir) in installed_dirs(root) {
        if !wanted.contains_key(&name) {
            fs::remove_dir_all(&dir)
                .map_err(|e| format!("cannot remove {}: {}", dir.display(), e))?;
            report.removed.push(name.to_string());
            if let Some(scope) = dir.parent() {
                let _ = fs::remove_dir(scope); // only succeeds when empty
            }
        }
    }
    Ok(report)
}

/// Unpacks next to `dir` first and swaps it in, so a failed download or
/// unpack never leaves a half-written package behind.
fn place(archive: &[u8], dir: &Path, marker: &str) -> Result<(), String> {
    let parent = dir.parent().ok_or("bad package directory")?;
    fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {}", parent.display(), e))?;
    let leaf = dir.file_name().and_then(|n| n.to_str()).unwrap_or("pkg");
    let staging = parent.join(format!(".{}.{}.tmp", leaf, std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    let result = tarball::unpack(archive, &staging).and_then(|_| {
        fs::write(staging.join(MARKER), marker)
            .map_err(|e| format!("cannot write the install marker: {}", e))
    });
    if let Err(err) = result {
        let _ = fs::remove_dir_all(&staging);
        return Err(err);
    }
    if dir.exists() {
        fs::remove_dir_all(dir).map_err(|e| format!("cannot replace {}: {}", dir.display(), e))?;
    }
    fs::rename(&staging, dir).map_err(|e| {
        let _ = fs::remove_dir_all(&staging);
        format!("cannot move the package into {}: {}", dir.display(), e)
    })
}

/// Packages an install put under `root`, found by their markers.
pub fn installed_dirs(root: &Path) -> Vec<(PackageName, PathBuf)> {
    let mut out = Vec::new();
    let Ok(scopes) = fs::read_dir(root) else {
        return out;
    };
    for scope in scopes.flatten() {
        let Ok(names) = fs::read_dir(scope.path()) else {
            continue;
        };
        for entry in names.flatten() {
            let dir = entry.path();
            if !dir.join(MARKER).is_file() {
                continue;
            }
            let full = format!(
                "@{}/{}",
                scope.file_name().to_string_lossy(),
                entry.file_name().to_string_lossy()
            );
            if let Ok(name) = PackageName::from_str(&full) {
                out.push((name, dir));
            }
        }
    }
    out.sort();
    out
}

/// The version recorded in a package directory's marker.
pub fn installed_version(dir: &Path) -> Option<String> {
    let text = fs::read_to_string(dir.join(MARKER)).ok()?;
    text.split_whitespace().nth(1).map(str::to_string)
}
