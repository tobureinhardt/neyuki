//! `neyuki.toml` reading and writing.

use std::collections::BTreeMap;
use std::str::FromStr;

use super::native::NativeManifest;
use super::version::{Version, VersionReq};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageManifest {
    pub name: String,
    pub version: Version,
    pub description: Option<String>,
    pub entry: String,
    /// An SPDX expression such as `MIT`, shown on the registry.
    pub license: Option<String>,
    /// Where the source lives, an http(s) URL.
    pub repository: Option<String>,
    /// Up to ten search words for the registry.
    pub keywords: Vec<String>,
    pub dependencies: BTreeMap<String, VersionReq>,
    pub dev_dependencies: BTreeMap<String, VersionReq>,
    pub scripts: BTreeMap<String, String>,
    /// The `[app]` section read by `neyuki build`.
    pub app: AppManifest,
    /// `[native]`: prebuilt extension libraries per target (see `pkg::native`).
    pub native: Option<NativeManifest>,
}

/// `[app]`: how `neyuki build` packages the project as a desktop app.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AppManifest {
    /// The product name shown to users (default: the package name).
    pub name: Option<String>,
    /// A reverse-DNS id for the macOS bundle (`CFBundleIdentifier`).
    pub identifier: Option<String>,
    /// An `.ico`, `.png` or `.icns` file.
    pub icon: Option<String>,
    /// Files and directories to bundle (default: `assets` when it exists).
    pub assets: Option<Vec<String>>,
    /// Keep the console window on Windows, for command-line apps.
    pub console: bool,
    pub publisher: Option<String>,
    pub copyright: Option<String>,
}

impl AppManifest {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A TOML key: bare when it can be, otherwise quoted (`"@yuki/colors"`).
fn toml_key(key: &str) -> String {
    if !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        key.to_string()
    } else {
        format!("\"{}\"", key)
    }
}

/// Reads `["a", "b"]` (or a single `"a"`) as a list of strings.
fn parse_string_list(raw: &str) -> Vec<String> {
    let inner = raw.trim();
    let inner = inner
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(inner);
    inner
        .split(',')
        .map(|item| item.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

impl Default for PackageManifest {
    fn default() -> Self {
        Self {
            name: "unnamed_pkg".to_string(),
            version: Version::new(0, 1, 0),
            description: None,
            entry: "main.nyk".to_string(),
            license: None,
            repository: None,
            keywords: Vec::new(),
            dependencies: BTreeMap::new(),
            dev_dependencies: BTreeMap::new(),
            scripts: BTreeMap::new(),
            app: AppManifest::default(),
            native: None,
        }
    }
}

impl PackageManifest {
    pub fn new(name: impl Into<String>, version: Version) -> Self {
        Self {
            name: name.into(),
            version,
            ..Default::default()
        }
    }

    pub fn add_dependency(&mut self, name: impl Into<String>, req: VersionReq) {
        self.dependencies.insert(name.into(), req);
    }

    pub fn parse(content: &str) -> Result<Self, String> {
        let mut manifest = Self::default();
        let mut current_section = "package";

        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                let sec = &trimmed[1..trimmed.len() - 1].trim();
                current_section = match *sec {
                    "package" => "package",
                    "dependencies" => "dependencies",
                    "dev-dependencies" | "dev_dependencies" => "dev-dependencies",
                    "scripts" => "scripts",
                    "app" => "app",
                    "native" | "native.targets" => {
                        manifest.native.get_or_insert_with(NativeManifest::default);
                        "native"
                    }
                    _ => "unknown",
                };
                continue;
            }

            let Some((key, val_raw)) = trimmed.split_once('=') else {
                continue;
            };
            // Registry names need quoted keys: `"@yuki/colors" = "^1.0"`.
            let key = key.trim().trim_matches('"').trim_matches('\'');
            let val = val_raw.trim().trim_matches('"').trim_matches('\'');

            match current_section {
                "package" => match key {
                    "name" => manifest.name = val.to_string(),
                    "version" => manifest.version = Version::from_str(val)?,
                    "description" => manifest.description = Some(val.to_string()),
                    "entry" => manifest.entry = val.to_string(),
                    "license" => manifest.license = Some(val.to_string()),
                    "repository" => manifest.repository = Some(val.to_string()),
                    "keywords" => manifest.keywords = parse_string_list(val_raw),
                    _ => {}
                },
                "dependencies" => {
                    let req = VersionReq::parse(val)?;
                    manifest.dependencies.insert(key.to_string(), req);
                }
                "dev-dependencies" => {
                    let req = VersionReq::parse(val)?;
                    manifest.dev_dependencies.insert(key.to_string(), req);
                }
                "scripts" => {
                    manifest.scripts.insert(key.to_string(), val.to_string());
                }
                "native" => {
                    manifest
                        .native
                        .get_or_insert_with(NativeManifest::default)
                        .parse_line(key, val)?;
                }
                "app" => {
                    let app = &mut manifest.app;
                    match key {
                        "name" => app.name = Some(val.to_string()),
                        "identifier" => app.identifier = Some(val.to_string()),
                        "icon" => app.icon = Some(val.to_string()),
                        "assets" => app.assets = Some(parse_string_list(val_raw)),
                        "console" => app.console = val == "true",
                        "publisher" => app.publisher = Some(val.to_string()),
                        "copyright" => app.copyright = Some(val.to_string()),
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        Ok(manifest)
    }

    pub fn serialize(&self) -> String {
        let mut out = String::new();
        out.push_str("[package]\n");
        out.push_str(&format!("name = \"{}\"\n", self.name));
        out.push_str(&format!("version = \"{}\"\n", self.version));
        if let Some(desc) = &self.description {
            out.push_str(&format!("description = \"{}\"\n", desc));
        }
        out.push_str(&format!("entry = \"{}\"\n", self.entry));
        if let Some(license) = &self.license {
            out.push_str(&format!("license = \"{}\"\n", license));
        }
        if let Some(repository) = &self.repository {
            out.push_str(&format!("repository = \"{}\"\n", repository));
        }
        if !self.keywords.is_empty() {
            let quoted: Vec<String> = self.keywords.iter().map(|k| format!("\"{}\"", k)).collect();
            out.push_str(&format!("keywords = [{}]\n", quoted.join(", ")));
        }
        out.push('\n');

        if !self.dependencies.is_empty() {
            out.push_str("[dependencies]\n");
            for (k, v) in &self.dependencies {
                out.push_str(&format!("{} = \"{}\"\n", toml_key(k), v));
            }
            out.push('\n');
        }

        if !self.dev_dependencies.is_empty() {
            out.push_str("[dev-dependencies]\n");
            for (k, v) in &self.dev_dependencies {
                out.push_str(&format!("{} = \"{}\"\n", toml_key(k), v));
            }
            out.push('\n');
        }

        if !self.scripts.is_empty() {
            out.push_str("[scripts]\n");
            for (k, v) in &self.scripts {
                out.push_str(&format!("{} = \"{}\"\n", k, v));
            }
            out.push('\n');
        }

        if !self.app.is_empty() {
            let app = &self.app;
            out.push_str("[app]\n");
            let fields = [
                ("name", &app.name),
                ("identifier", &app.identifier),
                ("icon", &app.icon),
                ("publisher", &app.publisher),
                ("copyright", &app.copyright),
            ];
            for (key, value) in fields {
                if let Some(value) = value {
                    out.push_str(&format!("{} = \"{}\"\n", key, value));
                }
            }
            if let Some(assets) = &app.assets {
                let quoted: Vec<String> = assets.iter().map(|a| format!("\"{}\"", a)).collect();
                out.push_str(&format!("assets = [{}]\n", quoted.join(", ")));
            }
            if app.console {
                out.push_str("console = true\n");
            }
            out.push('\n');
        }

        if let Some(native) = &self.native {
            native.serialize(&mut out);
        }

        out
    }
}
