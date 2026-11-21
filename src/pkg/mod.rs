//! The package manager: `neyuki.toml`, `neyuki.lock`, the registry at
//! neyuki.cc and the `neyuki pkg` commands.

pub mod cli;
pub mod credentials;
pub mod install;
pub mod lockfile;
pub mod manifest;
pub mod native;
pub mod paths;
pub mod publish;
pub mod registry;
pub mod resolver;
pub mod store;
pub mod tarball;
pub mod version;

pub use cli::run_pkg_cli;
pub use lockfile::{LockedPackage, Lockfile};
pub use manifest::{AppManifest, PackageManifest};
pub use paths::PackageName;
pub use store::PackageStore;
pub use version::{Version, VersionReq};

/// Lowercase hex SHA-256, the checksum format of `neyuki.lock` and the registry.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_semver_comparisons_and_caret() {
        let v1 = Version::new(1, 2, 3);
        let v2 = Version::new(1, 3, 0);
        let v3 = Version::new(2, 0, 0);

        assert!(v1 < v2);
        assert!(v2 < v3);

        let req_caret = VersionReq::parse("^1.2.0").unwrap();
        assert!(req_caret.matches(&v1));
        assert!(req_caret.matches(&v2));
        assert!(!req_caret.matches(&v3));

        let req_tilde = VersionReq::parse("~1.2.0").unwrap();
        assert!(req_tilde.matches(&v1));
        assert!(!req_tilde.matches(&v2));
    }

    #[test]
    fn test_manifest_parse_and_serialize() {
        let toml_str = r#"
[package]
name = "demo_service"
version = "1.0.4"
description = "A demo web service"
entry = "server.nyk"

[dependencies]
http = "^0.5.0"
json = "=1.0.0"

[scripts]
test = "neyuki test"
"#;
        let manifest = PackageManifest::parse(toml_str).unwrap();
        assert_eq!(manifest.name, "demo_service");
        assert_eq!(manifest.version, Version::new(1, 0, 4));
        assert_eq!(manifest.dependencies.len(), 2);
        assert!(manifest.dependencies.contains_key("http"));

        let serialized = manifest.serialize();
        assert!(serialized.contains("name = \"demo_service\""));
        assert!(serialized.contains("version = \"1.0.4\""));
    }

    #[test]
    fn test_lockfile_roundtrip() {
        let mut lockfile = Lockfile::new();
        lockfile.add_package(LockedPackage {
            name: "crypto_utils".to_string(),
            version: Version::new(2, 1, 0),
            checksum: "a1b2c3d4e5f6".to_string(),
            dependencies: vec!["buffer".to_string()],
        });

        let serialized = lockfile.serialize();
        let parsed = Lockfile::parse(&serialized).unwrap();
        assert_eq!(parsed.packages.len(), 1);
        assert_eq!(parsed.packages[0].name, "crypto_utils");
        assert_eq!(parsed.packages[0].version, Version::new(2, 1, 0));
        assert_eq!(parsed.packages[0].checksum, "a1b2c3d4e5f6");
    }

    #[test]
    fn version_requirements() {
        let v = |s: &str| s.parse::<Version>().unwrap();
        let range = VersionReq::parse(">=1.2, <2").unwrap();
        assert!(
            range.matches(&v("1.9.9"))
                && !range.matches(&v("2.0.0"))
                && !range.matches(&v("1.1.0"))
        );
        assert_eq!(range.to_string(), ">=1.2.0, <2.0.0");
        assert!(
            !VersionReq::parse("^1.0.0")
                .unwrap()
                .matches(&v("1.1.0-beta")),
            "prereleases need asking for"
        );
        assert!(
            VersionReq::parse("^1.1.0-beta")
                .unwrap()
                .matches(&v("1.1.0-beta.2"))
        );
        assert!(v("1.0.0-alpha") < v("1.0.0-alpha.1"));
        assert!(
            v("1.0.0-alpha.2") < v("1.0.0-alpha.10"),
            "numeric identifiers compare as numbers"
        );
        assert!(v("1.0.0-rc.1") < v("1.0.0"));
    }

    #[test]
    fn manifest_with_registry_names() {
        let text = "[package]
name = \"@yuki/app\"
version = \"1.0.0\"
entry = \"init.nyk\"
license = \"MIT\"
keywords = [\"a\", \"b\"]

[dependencies]
\"@yuki/router\" = \"^1.2\"
";
        let manifest = PackageManifest::parse(text).unwrap();
        assert_eq!(
            manifest.dependencies.keys().collect::<Vec<_>>(),
            vec!["@yuki/router"]
        );
        assert_eq!(manifest.keywords, vec!["a", "b"]);
        let again = PackageManifest::parse(&manifest.serialize()).unwrap();
        assert_eq!(again, manifest);
        assert!(
            manifest
                .serialize()
                .contains("\"@yuki/router\" = \"^1.2.0\"")
        );
    }
}
