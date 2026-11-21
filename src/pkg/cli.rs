//! The `neyuki pkg` commands.

use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use super::credentials;
use super::install::{self, Target};
use super::manifest::PackageManifest;
use super::paths::{self, MANIFEST, PackageName};
use super::publish;
use super::registry::Registry;
use super::resolver::{default_requirement, parse_spec};
use super::store::PackageStore;
use super::tarball;
use super::version::Version;

pub fn run_pkg_cli(args: &[String]) -> Result<(), String> {
    let Some(subcmd) = args.first() else {
        print_pkg_help();
        return Ok(());
    };
    let rest = &args[1..];
    let flag = |name: &str| rest.iter().any(|a| a == name);
    let positional: Vec<&str> = rest
        .iter()
        .map(String::as_str)
        .filter(|a| !a.starts_with('-'))
        .collect();

    match subcmd.as_str() {
        "init" => init_project(positional.first().copied()),
        "add" => {
            if positional.is_empty() {
                return Err("usage: neyuki pkg add <@user/package>[@version] ...".to_string());
            }
            add_dependencies(&positional, flag("--no-install"))
        }
        "remove" | "rm" => {
            if positional.is_empty() {
                return Err("usage: neyuki pkg remove <@user/package> ...".to_string());
            }
            remove_dependencies(&positional, flag("-g") || flag("--global"))
        }
        "install" | "i" => {
            if flag("-g") || flag("--global") {
                install_global(&positional)
            } else if !positional.is_empty() {
                add_dependencies(&positional, false)
            } else {
                install_project(false, flag("--frozen"))
            }
        }
        "update" => install_project(true, false),
        "list" | "ls" => list_packages(flag("-g") || flag("--global")),
        "check" => check_project(),
        "search" => search(&positional.join(" ")),
        "info" => info(
            positional
                .first()
                .copied()
                .ok_or("usage: neyuki pkg info <@user/package>")?,
        ),
        "login" => login(positional.first().copied()),
        "logout" => logout(),
        "whoami" => whoami(),
        "publish" => publish_package(flag("--dry-run")),
        "yank" => yank(&positional, flag("--undo")),
        "help" | "--help" | "-h" => {
            print_pkg_help();
            Ok(())
        }
        other => Err(format!(
            "unknown pkg command '{}'. Run 'neyuki pkg help' for the commands.",
            other
        )),
    }
}

fn print_pkg_help() {
    println!("Neyuki package manager");
    println!("Usage: neyuki pkg <command> [args]\n");
    println!("Projects:");
    println!("  init [name]                 Create a neyuki.toml here");
    println!("  add <pkg>[@req] ...         Add dependencies and install them");
    println!("  remove <pkg> ...            Remove dependencies");
    println!("  install [--frozen]          Install what neyuki.toml and neyuki.lock list");
    println!("  update                      Move every dependency to its newest matching version");
    println!("  list                        Show installed packages");
    println!("  check                       Validate neyuki.toml");
    println!("\nGlobal packages (require(\"@user/package\") from any script):");
    println!("  install -g <pkg>[@req] ...  Install for your user");
    println!("  remove -g <pkg> ...         Uninstall");
    println!("  list -g                     Show them");
    println!("\nRegistry:");
    println!("  search <words>              Find packages");
    println!("  info <pkg>                  Show a package's versions");
    println!("  login [token]               Save an API token from neyuki.cc/account");
    println!("  logout | whoami");
    println!("  publish [--dry-run]         Upload this project as a new version");
    println!("  yank <version> [--undo]     Hide a version from new installs");
    println!("\nPackages are named @user/package. In code, require(\"@pkg/user/package\") loads");
    println!("one from ./neyuki_packages, and require(\"@user/package\") one installed with -g.");
    println!("NEYUKI_REGISTRY, NEYUKI_TOKEN, NEYUKI_PATH and NEYUKI_HOME override the defaults.");
}

fn registry() -> Registry {
    let base = paths::registry_url();
    Registry::new(credentials::token_for(&base))
}

/// The project around the working directory: where `neyuki.toml` is.
fn project_root() -> Result<PathBuf, String> {
    let cwd =
        std::env::current_dir().map_err(|e| format!("cannot read the working directory: {}", e))?;
    cwd.ancestors()
        .find(|dir| dir.join(MANIFEST).is_file())
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            format!(
                "no {} here or in a parent directory; run 'neyuki pkg init' first",
                MANIFEST
            )
        })
}

fn read_manifest(path: &Path) -> Result<PackageManifest, String> {
    let text =
        fs::read_to_string(path).map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
    PackageManifest::parse(&text).map_err(|e| format!("{}: {}", path.display(), e))
}

fn write_manifest(path: &Path, manifest: &PackageManifest) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {}", dir.display(), e))?;
    }
    fs::write(path, manifest.serialize())
        .map_err(|e| format!("cannot write {}: {}", path.display(), e))
}

fn report(result: &install::Report) {
    for pkg in &result.installed {
        println!("  + {}", pkg);
    }
    for pkg in &result.removed {
        println!("  - {}", pkg);
    }
    for warning in &result.warnings {
        eprintln!("warning: {}", warning);
    }
    if result.installed.is_empty() && result.removed.is_empty() {
        println!(
            "Everything is up to date ({} package{}).",
            result.unchanged,
            plural(result.unchanged)
        );
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn init_project(name: Option<&str>) -> Result<(), String> {
    let manifest_path = Path::new(MANIFEST);
    if manifest_path.exists() {
        return Err(format!("{} already exists here", MANIFEST));
    }
    let fallback = std::env::current_dir()
        .ok()
        .and_then(|dir| dir.file_name().map(|n| n.to_string_lossy().to_lowercase()))
        .unwrap_or_else(|| "my_project".to_string());
    let name = name.map(str::to_string).unwrap_or(fallback);
    let mut manifest = PackageManifest::new(&name, Version::new(0, 1, 0));
    manifest.entry = "init.nyk".to_string();
    write_manifest(manifest_path, &manifest)?;
    println!("Created {} for '{}'.", MANIFEST, name);
    if name.parse::<PackageName>().is_err() {
        println!(
            "To publish it, set name to @<your username>/{}.",
            name.trim_start_matches('@')
        );
    }
    Ok(())
}

/// Adds `specs` to the manifest, picking `^latest` when no requirement is
/// given, then installs.
fn add_to(target: &Target, specs: &[&str], install_after: bool) -> Result<(), String> {
    let registry = registry();
    let mut manifest = if target.manifest.exists() {
        read_manifest(&target.manifest)?
    } else {
        PackageManifest::new("global", Version::new(0, 0, 0))
    };
    for spec in specs {
        let (name, req) = parse_spec(spec)?;
        let req = match req {
            Some(req) => req,
            None => {
                let meta = registry.package(&name)?;
                let latest = meta.latest.ok_or_else(|| {
                    format!("{} has no installable versions (all are yanked)", name)
                })?;
                default_requirement(&latest)
            }
        };
        println!("Adding {} {}", name, req);
        manifest.add_dependency(name.to_string(), req);
    }
    if install_after {
        install_with(target, &manifest, &registry)
    } else {
        write_manifest(&target.manifest, &manifest)
    }
}

/// Writes the changed manifest and installs from it; if the install fails,
/// the manifest goes back to what it was, so a failed `add` changes nothing.
fn install_with(
    target: &Target,
    manifest: &PackageManifest,
    registry: &Registry,
) -> Result<(), String> {
    let previous = fs::read(&target.manifest).ok();
    write_manifest(&target.manifest, manifest)?;
    match install::install(target, registry, false, false) {
        Ok(result) => {
            report(&result);
            Ok(())
        }
        Err(err) => {
            match previous {
                Some(bytes) => {
                    let _ = fs::write(&target.manifest, bytes);
                }
                None => {
                    let _ = fs::remove_file(&target.manifest);
                }
            }
            Err(err)
        }
    }
}

fn add_dependencies(specs: &[&str], no_install: bool) -> Result<(), String> {
    let root = project_root()?;
    add_to(&Target::project(&root), specs, !no_install)
}

fn install_global(specs: &[&str]) -> Result<(), String> {
    let root = paths::global_packages_dir()?;
    let target = Target::global(&root);
    if specs.is_empty() {
        // Reinstall what was installed globally before.
        report(&install::install(&target, &registry(), false, false)?);
    } else {
        add_to(&target, specs, true)?;
    }
    println!("Global packages are in {}", root.display());
    Ok(())
}

fn remove_dependencies(specs: &[&str], global: bool) -> Result<(), String> {
    let target = if global {
        Target::global(&paths::global_packages_dir()?)
    } else {
        Target::project(&project_root()?)
    };
    if !target.manifest.exists() {
        return Err("nothing is installed there".to_string());
    }
    let mut manifest = read_manifest(&target.manifest)?;
    for spec in specs {
        let (name, _) = parse_spec(spec)?;
        if manifest.dependencies.remove(&name.to_string()).is_none() {
            return Err(format!("{} is not a dependency", name));
        }
        println!("Removing {}", name);
    }
    install_with(&target, &manifest, &registry())
}

fn install_project(update: bool, frozen: bool) -> Result<(), String> {
    let root = project_root()?;
    let result = install::install(&Target::project(&root), &registry(), update, frozen)?;
    report(&result);
    Ok(())
}

fn list_packages(global: bool) -> Result<(), String> {
    let root = if global {
        paths::global_packages_dir()?
    } else {
        project_root()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(paths::PACKAGES_DIR)
    };
    let installed = install::installed_dirs(&root);
    // Packages placed by hand (neyuki_packages/<name>/init.nyk) still load
    // with require("<name>").
    let manual: Vec<String> = if global {
        Vec::new()
    } else {
        PackageStore::new(root.parent().unwrap_or(Path::new(".")))
            .list_installed()?
            .into_iter()
            .filter(|dir| {
                root.join(dir).join("init.nyk").is_file()
                    || root.join(dir).join("main.nyk").is_file()
            })
            .collect()
    };
    if installed.is_empty() && manual.is_empty() {
        println!("No packages in {}", root.display());
        return Ok(());
    }
    println!("{} ({}):", root.display(), installed.len() + manual.len());
    for (name, dir) in &installed {
        let version = install::installed_version(dir).unwrap_or_else(|| "?".to_string());
        println!("  {}@{}", name, version);
    }
    for dir in manual {
        println!("  {} (placed by hand)", dir);
    }
    Ok(())
}

fn check_project() -> Result<(), String> {
    let root = project_root()?;
    let manifest = read_manifest(&root.join(MANIFEST))?;
    println!("{} v{} is valid.", manifest.name, manifest.version);
    println!("Dependencies ({}):", manifest.dependencies.len());
    for (name, req) in &manifest.dependencies {
        let note = if name.parse::<PackageName>().is_ok() {
            ""
        } else {
            "  (not a registry name)"
        };
        println!("  - {} {}{}", name, req, note);
    }
    Ok(())
}

fn search(query: &str) -> Result<(), String> {
    let registry = registry();
    let (total, hits) = registry.search(query)?;
    if hits.is_empty() {
        println!("No packages found.");
        return Ok(());
    }
    for hit in &hits {
        let version = hit
            .latest
            .as_deref()
            .map(|v| format!("@{}", v))
            .unwrap_or_default();
        println!("{}{}  ({} downloads)", hit.name, version, hit.downloads);
        if !hit.description.is_empty() {
            println!("    {}", hit.description);
        }
    }
    if total as usize > hits.len() {
        println!(
            "\n{} more at {}/packages?q={}",
            total as usize - hits.len(),
            registry.base(),
            query.replace(' ', "+")
        );
    }
    Ok(())
}

fn info(spec: &str) -> Result<(), String> {
    let (name, _) = parse_spec(spec)?;
    let registry = registry();
    let meta = registry.package(&name)?;
    println!("{}", meta.name);
    if !meta.description.is_empty() {
        println!("{}", meta.description);
    }
    println!("{}/packages/{}\n", registry.base(), meta.name);
    for version in &meta.versions {
        let latest = meta.latest.as_ref() == Some(&version.version);
        let deps: Vec<String> = version
            .dependencies
            .iter()
            .map(|(k, v)| format!("{} {}", k, v))
            .collect();
        println!(
            "  {}{}{}{}",
            version.version,
            if latest { "  (latest)" } else { "" },
            if version.yanked { "  (yanked)" } else { "" },
            if deps.is_empty() {
                String::new()
            } else {
                format!("  needs {}", deps.join(", "))
            }
        );
    }
    Ok(())
}

fn login(token: Option<&str>) -> Result<(), String> {
    let base = paths::registry_url();
    let token = match token {
        Some(token) => token.trim().to_string(),
        None => {
            println!("Create a token at {}/account, then paste it here.", base);
            print!("Token: ");
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            std::io::stdin()
                .lock()
                .read_line(&mut line)
                .map_err(|e| format!("cannot read the token: {}", e))?;
            line.trim().to_string()
        }
    };
    if token.is_empty() {
        return Err("no token given".to_string());
    }
    let (username, verified) = Registry::new(Some(token.clone())).whoami()?;
    credentials::save(&base, &token)?;
    println!("Logged in to {} as {}.", base, username);
    if !verified {
        println!("Confirm your email address before publishing.");
    }
    Ok(())
}

fn logout() -> Result<(), String> {
    let base = paths::registry_url();
    if credentials::remove(&base)? {
        println!(
            "Logged out of {}. The token still works until you delete it at {}/account.",
            base, base
        );
    } else {
        println!("Not logged in to {}.", base);
    }
    Ok(())
}

fn whoami() -> Result<(), String> {
    let registry = registry();
    let (username, verified) = registry.whoami()?;
    println!(
        "{}{}",
        username,
        if verified {
            ""
        } else {
            " (email not confirmed)"
        }
    );
    Ok(())
}

fn publish_package(dry_run: bool) -> Result<(), String> {
    let root = project_root()?;
    let manifest = read_manifest(&root.join(MANIFEST))?;
    let files = publish::collect(&root)?;
    let metadata = publish::metadata(&manifest, &files)?;
    let archive = tarball::pack(&files)?;
    if archive.len() > publish::MAX_ARCHIVE {
        return Err(format!(
            "the package is {:.1} MB compressed; the limit is 10 MB. List what to leave out in .neyukiignore",
            archive.len() as f64 / 1024.0 / 1024.0
        ));
    }
    let name: PackageName = manifest.name.parse()?;
    let registry = registry();
    if !dry_run {
        registry.whoami()?;
    }
    let unpacked: usize = files.iter().map(|(_, bytes)| bytes.len()).sum();
    println!(
        "{}@{}: {} file{}",
        name,
        manifest.version,
        files.len(),
        plural(files.len())
    );
    for (path, bytes) in &files {
        println!("  {:>9}  {}", human(bytes.len()), path);
    }
    println!(
        "{} unpacked, {} packed",
        human(unpacked),
        human(archive.len())
    );
    if dry_run {
        println!("Dry run: nothing was uploaded.");
        return Ok(());
    }
    let result = registry.publish(&name, &manifest.version, &metadata, &archive)?;
    println!(
        "Published {}@{}: {}",
        name,
        manifest.version,
        result
            .get("url")
            .and_then(|u| u.as_str())
            .unwrap_or(registry.base())
    );
    Ok(())
}

fn human(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / 1024.0 / 1024.0)
    }
}

/// `yank <version>` for this project's package, or `yank <@user/pkg>@<version>`.
fn yank(args: &[&str], undo: bool) -> Result<(), String> {
    let usage = "usage: neyuki pkg yank <version> [--undo], or yank <@user/package>@<version>";
    let spec = args.first().ok_or(usage)?;
    let (name, version) = if let Some(rest) = spec.strip_prefix('@') {
        let (name, version) = rest.split_once('@').ok_or(usage)?;
        (
            PackageName::from_str(&format!("@{}", name))?,
            Version::from_str(version)?,
        )
    } else {
        let root = project_root()?;
        let manifest = read_manifest(&root.join(MANIFEST))?;
        (manifest.name.parse()?, Version::from_str(spec)?)
    };
    let result = registry().yank(&name, &version, !undo)?;
    let latest = result
        .get("latest")
        .and_then(|v| v.as_str())
        .unwrap_or("none");
    println!(
        "{} {}@{}. Latest is now {}.",
        if undo { "Restored" } else { "Yanked" },
        name,
        version,
        latest
    );
    Ok(())
}
