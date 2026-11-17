//! The client for the package registry's HTTP API (neyuki.cc/api).

use std::collections::BTreeMap;
use std::str::FromStr;
use std::time::Duration;

use serde_json::{Value as Json, json};

use super::paths::{PackageName, registry_url};
use super::version::Version;

/// The most a package download may be; the registry caps uploads at 10 MB.
const MAX_DOWNLOAD: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct VersionMeta {
    pub version: Version,
    pub checksum: String,
    pub entry: String,
    pub dependencies: BTreeMap<String, String>,
    pub yanked: bool,
}

#[derive(Clone, Debug)]
pub struct PackageMeta {
    pub name: PackageName,
    pub description: String,
    pub latest: Option<Version>,
    pub versions: Vec<VersionMeta>,
}

#[derive(Clone, Debug)]
pub struct SearchHit {
    pub name: String,
    pub description: String,
    pub latest: Option<String>,
    pub downloads: u64,
}

pub struct Registry {
    base: String,
    agent: ureq::Agent,
    token: Option<String>,
}

fn user_agent() -> String {
    format!(
        "neyuki/{} ({})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS
    )
}

impl Registry {
    pub fn new(token: Option<String>) -> Self {
        // TLS through the platform library and the OS trust store, as in
        // `@neyuki/http` (see src/vm/libs/http.rs for why).
        let tls = ureq::tls::TlsConfig::builder()
            .provider(ureq::tls::TlsProvider::NativeTls)
            .root_certs(ureq::tls::RootCerts::PlatformVerifier)
            .build();
        let agent = ureq::config::Config::builder()
            .tls_config(tls)
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(120)))
            .user_agent(user_agent())
            .build()
            .new_agent();
        Self {
            base: registry_url(),
            agent,
            token,
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn token(&self) -> Result<&str, String> {
        self.token
            .as_deref()
            .ok_or_else(|| "not logged in; run 'neyuki pkg login' first".to_string())
    }

    /// Reads a response: the JSON body on success, the registry's error
    /// message otherwise.
    fn finish(
        &self,
        result: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<Json, String> {
        let mut response =
            result.map_err(|e| format!("cannot reach the registry at {}: {}", self.base, e))?;
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .with_config()
            .limit(4 * 1024 * 1024)
            .read_to_string()
            .map_err(|e| format!("cannot read the registry's answer: {}", e))?;
        let body: Json = serde_json::from_str(&text).unwrap_or(Json::Null);
        if (200..300).contains(&status) {
            return Ok(body);
        }
        let message = body
            .get("error")
            .and_then(Json::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("the registry answered {}", status));
        Err(message)
    }

    fn get_json(&self, path: &str) -> Result<Json, String> {
        self.finish(
            self.agent
                .get(&self.url(path))
                .header("accept", "application/json")
                .call(),
        )
    }

    pub fn package(&self, name: &PackageName) -> Result<PackageMeta, String> {
        let body = self.get_json(&format!("/api/packages/{}", name))?;
        parse_package(name, &body)
    }

    /// The archive for one version, checked against `checksum`.
    pub fn download(
        &self,
        name: &PackageName,
        version: &Version,
        checksum: &str,
    ) -> Result<Vec<u8>, String> {
        let url = self.url(&format!("/api/packages/{}/download/{}.tgz", name, version));
        let mut response = self
            .agent
            .get(&url)
            .call()
            .map_err(|e| format!("cannot download {}@{}: {}", name, version, e))?;
        if !response.status().is_success() {
            let _ = self.finish(Ok(response))?;
            return Err(format!("cannot download {}@{}", name, version));
        }
        let bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_DOWNLOAD)
            .read_to_vec()
            .map_err(|e| format!("cannot download {}@{}: {}", name, version, e))?;
        let actual = super::sha256_hex(&bytes);
        if !actual.eq_ignore_ascii_case(checksum) {
            return Err(format!(
                "checksum mismatch for {}@{}: expected {}, got {}. The download is corrupt or \
                 was tampered with; nothing was installed",
                name, version, checksum, actual
            ));
        }
        Ok(bytes)
    }

    pub fn search(&self, query: &str) -> Result<(u64, Vec<SearchHit>), String> {
        let encoded: String = query
            .bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{:02X}", b),
            })
            .collect();
        let body = self.get_json(&format!("/api/packages?q={}", encoded))?;
        let total = body.get("total").and_then(Json::as_u64).unwrap_or(0);
        let hits = body
            .get("packages")
            .and_then(Json::as_array)
            .map(|list| {
                list.iter()
                    .map(|p| SearchHit {
                        name: p
                            .get("name")
                            .and_then(Json::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        description: p
                            .get("description")
                            .and_then(Json::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        latest: p.get("latest").and_then(Json::as_str).map(str::to_string),
                        downloads: p.get("downloads").and_then(Json::as_u64).unwrap_or(0),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok((total, hits))
    }

    /// The account the token belongs to: (username, email verified).
    pub fn whoami(&self) -> Result<(String, bool), String> {
        let token = self.token()?;
        let body = self.finish(
            self.agent
                .get(&self.url("/api/me"))
                .header("authorization", &format!("Bearer {}", token))
                .call(),
        )?;
        let user = body
            .get("user")
            .filter(|u| !u.is_null())
            .ok_or("the token is not valid")?;
        let name = user
            .get("username")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_string();
        let verified = user
            .get("emailVerified")
            .and_then(Json::as_bool)
            .unwrap_or(false);
        Ok((name, verified))
    }

    pub fn publish(
        &self,
        name: &PackageName,
        version: &Version,
        metadata: &Json,
        tarball: &[u8],
    ) -> Result<Json, String> {
        let token = self.token()?;
        let boundary = format!("neyuki-{}", &super::sha256_hex(tarball)[..24]);
        let mut body = Vec::with_capacity(tarball.len() + 4096);
        body.extend_from_slice(
            format!(
                "--{b}\r\nContent-Disposition: form-data; name=\"metadata\"\r\nContent-Type: application/json\r\n\r\n{m}\r\n\
                 --{b}\r\nContent-Disposition: form-data; name=\"tarball\"; filename=\"package.tgz\"\r\nContent-Type: application/gzip\r\n\r\n",
                b = boundary,
                m = metadata
            )
            .as_bytes(),
        );
        body.extend_from_slice(tarball);
        body.extend_from_slice(format!("\r\n--{}--\r\n", boundary).as_bytes());

        self.finish(
            self.agent
                .put(&self.url(&format!("/api/packages/{}/versions/{}", name, version)))
                .header("authorization", &format!("Bearer {}", token))
                .header(
                    "content-type",
                    &format!("multipart/form-data; boundary={}", boundary),
                )
                .send(&body[..]),
        )
    }

    pub fn yank(
        &self,
        name: &PackageName,
        version: &Version,
        yanked: bool,
    ) -> Result<Json, String> {
        let token = self.token()?;
        self.finish(
            self.agent
                .post(&self.url(&format!("/api/packages/{}/versions/{}/yank", name, version)))
                .header("authorization", &format!("Bearer {}", token))
                .header("content-type", "application/json")
                .send(json!({ "yanked": yanked }).to_string()),
        )
    }
}

fn parse_package(name: &PackageName, body: &Json) -> Result<PackageMeta, String> {
    let bad = || format!("the registry sent an unexpected answer for {}", name);
    let mut versions = Vec::new();
    for v in body
        .get("versions")
        .and_then(Json::as_array)
        .ok_or_else(bad)?
    {
        let text = v.get("version").and_then(Json::as_str).ok_or_else(bad)?;
        let dependencies = v
            .get("dependencies")
            .and_then(Json::as_object)
            .map(|deps| {
                deps.iter()
                    .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        versions.push(VersionMeta {
            version: Version::from_str(text)?,
            checksum: v
                .get("checksum")
                .and_then(Json::as_str)
                .ok_or_else(bad)?
                .to_string(),
            entry: v
                .get("entry")
                .and_then(Json::as_str)
                .unwrap_or("init.nyk")
                .to_string(),
            dependencies,
            yanked: v.get("yanked").and_then(Json::as_bool).unwrap_or(false),
        });
    }
    versions.sort_by(|a, b| b.version.cmp(&a.version));
    Ok(PackageMeta {
        name: name.clone(),
        description: body
            .get("description")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_string(),
        latest: body
            .get("latest")
            .and_then(Json::as_str)
            .and_then(|v| Version::from_str(v).ok()),
        versions,
    })
}
