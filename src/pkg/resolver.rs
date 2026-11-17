//! Picks one version of every package a project needs.
//!
//! Packages install flat (`neyuki_packages/<scope>/<name>`), so each package
//! gets exactly one version, and every requirement on it must accept that
//! version. A version already in `neyuki.lock` is kept while it still fits,
//! even after it is yanked, so existing projects keep installing; new picks
//! take the highest matching version that is not yanked.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::str::FromStr;

use super::lockfile::{LockedPackage, Lockfile};
use super::paths::PackageName;
use super::registry::PackageMeta;
use super::version::{Version, VersionReq};

pub struct DependencyResolver<F> {
    fetch: F,
    cache: HashMap<PackageName, PackageMeta>,
}

impl<F> DependencyResolver<F>
where
    F: FnMut(&PackageName) -> Result<PackageMeta, String>,
{
    /// `fetch` looks a package up, normally in the registry.
    pub fn new(fetch: F) -> Self {
        Self {
            fetch,
            cache: HashMap::new(),
        }
    }

    fn meta(&mut self, name: &PackageName) -> Result<&PackageMeta, String> {
        if !self.cache.contains_key(name) {
            let meta = (self.fetch)(name)?;
            self.cache.insert(name.clone(), meta);
        }
        Ok(&self.cache[name])
    }

    /// Resolves `dependencies` (from `neyuki.toml`) into a lockfile.
    /// `locked` is the current lockfile, if any; pass `None` to update.
    ///
    /// Picks are greedy, so a requirement found later can rule out a version
    /// picked earlier. Then that requirement is remembered and resolution
    /// starts over, picking only versions that meet every requirement seen
    /// so far for the package.
    pub fn resolve(
        &mut self,
        dependencies: &BTreeMap<String, VersionReq>,
        locked: Option<&Lockfile>,
    ) -> Result<Lockfile, String> {
        let mut learned: Constraints = BTreeMap::new();
        for _ in 0..64 {
            match self.attempt(dependencies, locked, &learned)? {
                Attempt::Done(lockfile) => return Ok(lockfile),
                Attempt::Retry(name, req, by) => {
                    let list = learned.entry(name).or_default();
                    if list.iter().any(|(r, b)| *r == req && *b == by) {
                        return Err("dependency resolution went in circles".to_string());
                    }
                    list.push((req, by));
                }
            }
        }
        Err("dependency resolution did not settle after 64 rounds".to_string())
    }

    fn attempt(
        &mut self,
        dependencies: &BTreeMap<String, VersionReq>,
        locked: Option<&Lockfile>,
        learned: &Constraints,
    ) -> Result<Attempt, String> {
        let mut queue: VecDeque<(PackageName, VersionReq, String)> = VecDeque::new();
        for (name, req) in dependencies {
            queue.push_back((
                PackageName::from_str(name)?,
                req.clone(),
                "neyuki.toml".to_string(),
            ));
        }

        let mut chosen: BTreeMap<PackageName, LockedPackage> = BTreeMap::new();
        while let Some((name, req, wanted_by)) = queue.pop_front() {
            if let Some(pick) = chosen.get(&name) {
                if !req.matches(&pick.version) {
                    return Ok(Attempt::Retry(name, req, wanted_by));
                }
                continue;
            }

            let mut all: Vec<(VersionReq, String)> = vec![(req, wanted_by)];
            all.extend(learned.get(&name).into_iter().flatten().cloned());
            let fits = |v: &Version| all.iter().all(|(r, _)| r.matches(v));

            let locked_version = locked
                .and_then(|lock| lock.get_package(&name.to_string()))
                .map(|p| p.version.clone())
                .filter(|v| fits(v));
            let meta = self.meta(&name)?;
            let picked = locked_version
                .and_then(|v| meta.versions.iter().find(|m| m.version == v))
                .or_else(|| {
                    meta.versions
                        .iter()
                        .filter(|m| !m.yanked && fits(&m.version))
                        .max_by(|a, b| a.version.cmp(&b.version))
                });
            let Some(version) = picked else {
                return Err(unsatisfiable(&name, meta, &all));
            };

            let mut deps = Vec::new();
            let pick_name = format!("{}@{}", name, version.version);
            for (dep, dep_req) in &version.dependencies {
                let dep_name = PackageName::from_str(dep)?;
                let dep_req = VersionReq::parse(dep_req).map_err(|e| {
                    format!("{} has a bad requirement for {}: {}", pick_name, dep, e)
                })?;
                deps.push(dep_name.to_string());
                queue.push_back((dep_name, dep_req, pick_name.clone()));
            }
            deps.sort();
            let package = LockedPackage {
                name: name.to_string(),
                version: version.version.clone(),
                checksum: version.checksum.clone(),
                dependencies: deps,
            };
            chosen.insert(name, package);
        }

        let mut lockfile = Lockfile::new();
        for (_, package) in chosen {
            lockfile.add_package(package);
        }
        Ok(Attempt::Done(lockfile))
    }
}

/// Requirements learned from conflicts: package -> (requirement, who needs it).
type Constraints = BTreeMap<PackageName, Vec<(VersionReq, String)>>;

enum Attempt {
    Done(Lockfile),
    /// Start over, also requiring this of the package.
    Retry(PackageName, VersionReq, String),
}

fn unsatisfiable(name: &PackageName, meta: &PackageMeta, all: &[(VersionReq, String)]) -> String {
    let needs: Vec<String> = all
        .iter()
        .map(|(req, by)| format!("{} (from {})", req, by))
        .collect();
    let fits = |v: &Version| all.iter().all(|(r, _)| r.matches(v));
    if meta.versions.is_empty() {
        return format!(
            "{} has no published versions (needed by {})",
            name, all[0].1
        );
    }
    if meta.versions.iter().any(|m| fits(&m.version)) {
        return format!(
            "every version of {} that meets {} was yanked",
            name,
            needs.join(" and ")
        );
    }
    let available: Vec<String> = meta
        .versions
        .iter()
        .map(|m| m.version.to_string())
        .collect();
    if all.len() > 1 {
        format!(
            "version conflict for {}: no version meets {}. Packages install one version each. \
             Available: {}",
            name,
            needs.join(" and "),
            available.join(", ")
        )
    } else {
        format!(
            "no version of {} matches {}; available: {}",
            name,
            needs[0],
            available.join(", ")
        )
    }
}

/// Parses a `name` or `name@req` argument such as `@yuki/router@^1.2`.
pub fn parse_spec(spec: &str) -> Result<(PackageName, Option<VersionReq>), String> {
    let (name, req) = match spec.get(1..).and_then(|rest| rest.find('@')) {
        Some(at) => (&spec[..at + 1], Some(&spec[at + 2..])),
        None => (spec, None),
    };
    let name = PackageName::from_str(name)?;
    let req = req.map(VersionReq::parse).transpose()?;
    Ok((name, req))
}

/// The requirement `neyuki pkg add` writes for a fresh dependency: `^x.y.z`
/// of the latest version.
pub fn default_requirement(latest: &Version) -> VersionReq {
    VersionReq::Caret(latest.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pkg::registry::VersionMeta;

    /// (version, yanked, dependencies as (name, requirement))
    type Release<'a> = (&'a str, bool, &'a [(&'a str, &'a str)]);

    fn meta(name: &str, versions: &[Release]) -> PackageMeta {
        PackageMeta {
            name: name.parse().unwrap(),
            description: String::new(),
            latest: None,
            versions: versions
                .iter()
                .map(|(v, yanked, deps)| VersionMeta {
                    version: v.parse().unwrap(),
                    checksum: format!("sum-{}-{}", name, v),
                    entry: "init.nyk".into(),
                    dependencies: deps
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                    yanked: *yanked,
                })
                .collect(),
        }
    }

    fn registry() -> HashMap<String, PackageMeta> {
        let mut all = HashMap::new();
        for m in [
            meta(
                "@yuki/router",
                &[
                    ("1.0.0", false, &[("@yuki/colors", "^2.0.0")]),
                    ("1.2.0", false, &[("@yuki/colors", "^2.1.0")]),
                    ("1.3.0", true, &[]),
                    ("2.0.0-beta.1", false, &[]),
                ],
            ),
            meta(
                "@yuki/colors",
                &[
                    ("2.0.0", false, &[]),
                    ("2.1.5", false, &[]),
                    ("3.0.0", false, &[]),
                ],
            ),
        ] {
            all.insert(m.name.to_string(), m);
        }
        all
    }

    fn deps(list: &[(&str, &str)]) -> BTreeMap<String, VersionReq> {
        list.iter()
            .map(|(k, v)| (k.to_string(), VersionReq::parse(v).unwrap()))
            .collect()
    }

    fn resolve(list: &[(&str, &str)], lock: Option<&Lockfile>) -> Result<Lockfile, String> {
        let all = registry();
        let mut resolver = DependencyResolver::new(|name: &PackageName| {
            all.get(&name.to_string())
                .cloned()
                .ok_or_else(|| format!("no {}", name))
        });
        resolver.resolve(&deps(list), lock)
    }

    fn version_of(lock: &Lockfile, name: &str) -> String {
        lock.get_package(name).unwrap().version.to_string()
    }

    #[test]
    fn picks_highest_unyanked_release_and_transitive_deps() {
        let lock = resolve(&[("@yuki/router", "^1.0.0")], None).unwrap();
        assert_eq!(
            version_of(&lock, "@yuki/router"),
            "1.2.0",
            "1.3.0 is yanked, 2.0.0-beta is a prerelease"
        );
        assert_eq!(version_of(&lock, "@yuki/colors"), "2.1.5");
        assert_eq!(
            lock.get_package("@yuki/router").unwrap().dependencies,
            vec!["@yuki/colors"]
        );
    }

    #[test]
    fn keeps_locked_versions_even_when_yanked() {
        let mut old = Lockfile::new();
        old.add_package(LockedPackage {
            name: "@yuki/router".into(),
            version: "1.3.0".parse().unwrap(),
            checksum: "x".into(),
            dependencies: vec![],
        });
        let lock = resolve(&[("@yuki/router", "^1.0.0")], Some(&old)).unwrap();
        assert_eq!(version_of(&lock, "@yuki/router"), "1.3.0");
    }

    #[test]
    fn reports_conflicts_and_missing_versions() {
        let err = resolve(
            &[("@yuki/router", "^1.2.0"), ("@yuki/colors", "^3.0.0")],
            None,
        )
        .unwrap_err();
        assert!(err.contains("version conflict for @yuki/colors"), "{}", err);
        let err = resolve(&[("@yuki/colors", "^9.0.0")], None).unwrap_err();
        assert!(
            err.contains("available: 3.0.0, 2.1.5, 2.0.0") || err.contains("available"),
            "{}",
            err
        );
        let err = resolve(&[("@yuki/router", "=1.3.0")], None).unwrap_err();
        assert!(err.contains("yanked"), "{}", err);
    }

    #[test]
    fn revisits_a_pick_a_later_requirement_rules_out() {
        // `>=2.0.0` alone picks 3.0.0; the router then needs ^2.1, so the
        // resolver starts over and settles on 2.1.5, which meets both.
        let lock = resolve(
            &[("@yuki/colors", ">=2.0.0"), ("@yuki/router", "^1.2.0")],
            None,
        )
        .unwrap();
        assert_eq!(version_of(&lock, "@yuki/colors"), "2.1.5");
        assert_eq!(version_of(&lock, "@yuki/router"), "1.2.0");
    }

    #[test]
    fn prereleases_need_asking_for() {
        let lock = resolve(&[("@yuki/router", "^2.0.0-beta.1")], None).unwrap();
        assert_eq!(version_of(&lock, "@yuki/router"), "2.0.0-beta.1");
    }

    #[test]
    fn specs() {
        let (name, req) = parse_spec("@yuki/router@^1.2").unwrap();
        assert_eq!(name.to_string(), "@yuki/router");
        assert_eq!(req.unwrap().to_string(), "^1.2.0");
        assert!(parse_spec("@yuki/router").unwrap().1.is_none());
        assert!(parse_spec("router").is_err());
    }
}
