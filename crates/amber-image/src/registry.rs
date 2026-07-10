//! The OCI/Docker registry client: reference parsing, bearer-token auth, manifest
//! resolution (picking the arm64/linux image from a multi-arch index), and blob
//! download with sha256 verification.

// ureq's error carries a Response, so Results threading it trip the large-err lint.
#![allow(clippy::result_large_err)]

use crate::{Error, ImageConfig, Result};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Per-layer size ceiling. Streaming a blob is bounded by this so a hostile or
/// corrupt registry can't OOM the host with an unbounded download.
const MAX_BLOB: u64 = 4 << 30; // 4 GiB

const DEFAULT_REGISTRY: &str = "registry-1.docker.io";

// Accept every manifest flavour: OCI and Docker, single and index.
const ACCEPT_MANIFEST: &str = "application/vnd.oci.image.index.v1+json, \
     application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.oci.image.manifest.v1+json, \
     application/vnd.docker.distribution.manifest.v2+json";

/// A parsed image reference: registry host, repository, and a tag or digest.
#[derive(Debug, Clone)]
pub struct Reference {
    pub registry: String,
    pub repository: String,
    pub reference: String, // tag or "sha256:..."
}

impl std::fmt::Display for Reference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}:{}", self.registry, self.repository, self.reference)
    }
}

impl Reference {
    pub fn parse(s: &str) -> Result<Self> {
        if s.is_empty() {
            return Err(Error::Reference("empty".into()));
        }
        // Split the registry off the front: the first path component is a registry
        // only if it looks like a host (has a '.' or ':' or is localhost).
        let (registry, rest) = match s.split_once('/') {
            Some((head, tail))
                if head.contains('.') || head.contains(':') || head == "localhost" =>
            {
                (head.to_string(), tail.to_string())
            }
            _ => (DEFAULT_REGISTRY.to_string(), s.to_string()),
        };
        let registry = if registry == "docker.io" {
            DEFAULT_REGISTRY.to_string()
        } else {
            registry
        };

        // Split the tag/digest off the end of the remaining repository part.
        let (mut repository, reference) = if let Some((repo, dig)) = rest.split_once('@') {
            (repo.to_string(), dig.to_string())
        } else if let Some((repo, tag)) = rest.rsplit_once(':') {
            // A ':' is a tag only if it is not inside a path segment (no '/' after).
            if tag.contains('/') {
                (rest.clone(), "latest".to_string())
            } else {
                (repo.to_string(), tag.to_string())
            }
        } else {
            (rest.clone(), "latest".to_string())
        };

        // Docker Hub official images live under "library/".
        if registry == DEFAULT_REGISTRY && !repository.contains('/') {
            repository = format!("library/{repository}");
        }
        Ok(Reference {
            registry,
            repository,
            reference,
        })
    }
}

pub struct Layer {
    pub digest: String,
}

pub struct Manifest {
    pub config_digest: String,
    pub layers: Vec<Layer>,
}

pub struct Client {
    base: String, // https://<registry>/v2/<repo>
    repository: String,
    reference: String,
    token: Option<String>,
}

impl Client {
    pub fn new(r: &Reference) -> Result<Self> {
        Ok(Client {
            base: format!("https://{}/v2/{}", r.registry, r.repository),
            repository: r.repository.clone(),
            reference: r.reference.clone(),
            token: None,
        })
    }

    /// GET with the current bearer token, transparently acquiring one on a 401.
    fn get(&mut self, url: &str, accept: &str) -> Result<ureq::Response> {
        match self.request(url, accept) {
            Ok(resp) => Ok(resp),
            Err(ureq::Error::Status(401, resp)) => {
                let challenge = resp.header("www-authenticate").unwrap_or("").to_string();
                self.authenticate(&challenge)?;
                self.request(url, accept)
                    .map_err(|e| Error::Http(format!("after auth: {e}")))
            }
            Err(e) => Err(Error::Http(e.to_string())),
        }
    }

    fn request(&self, url: &str, accept: &str) -> std::result::Result<ureq::Response, ureq::Error> {
        let mut req = ureq::get(url).set("Accept", accept);
        if let Some(t) = &self.token {
            req = req.set("Authorization", &format!("Bearer {t}"));
        }
        req.call()
    }

    /// Resolve a `Bearer realm=...,service=...` challenge into a pull token.
    fn authenticate(&mut self, challenge: &str) -> Result<()> {
        let realm = challenge_field(challenge, "realm")
            .ok_or_else(|| Error::Registry(format!("no realm in challenge: {challenge}")))?;
        let service = challenge_field(challenge, "service").unwrap_or_default();
        let scope = format!("repository:{}:pull", self.repository);

        let resp = ureq::get(&realm)
            .query("service", &service)
            .query("scope", &scope)
            .call()
            .map_err(|e| Error::Http(format!("token request: {e}")))?;
        let body = resp.into_string()?;
        let v: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| Error::Json(e.to_string()))?;
        // Docker returns "token"; the OCI spec uses "access_token".
        let token = v["token"]
            .as_str()
            .or_else(|| v["access_token"].as_str())
            .ok_or_else(|| Error::Registry("no token in auth response".into()))?;
        self.token = Some(token.to_string());
        Ok(())
    }

    pub fn fetch_manifest(&mut self) -> Result<Manifest> {
        let url = format!("{}/manifests/{}", self.base, self.reference);
        let body = self.get(&url, ACCEPT_MANIFEST)?.into_string()?;
        let v: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| Error::Json(e.to_string()))?;

        // A multi-arch index: pick the arm64/linux image and fetch its manifest.
        if v.get("manifests").is_some() {
            let digest = pick_arm64(&v)?;
            let url = format!("{}/manifests/{}", self.base, digest);
            let body = self.get(&url, ACCEPT_MANIFEST)?.into_string()?;
            let v: serde_json::Value =
                serde_json::from_str(&body).map_err(|e| Error::Json(e.to_string()))?;
            return parse_manifest(&v);
        }
        parse_manifest(&v)
    }

    pub fn fetch_config(&mut self, digest: &str) -> Result<ImageConfig> {
        let url = format!("{}/blobs/{}", self.base, digest);
        let body = self.get(&url, "application/octet-stream")?.into_string()?;
        // Verify the config blob against its digest: the manifest references it by
        // sha256, so a mirror/MITM must not be able to swap entrypoint/cmd/env.
        let want = digest.strip_prefix("sha256:").unwrap_or(digest);
        let got = hex::encode(Sha256::digest(body.as_bytes()));
        if got != want {
            return Err(Error::Digest { want: want.to_string(), got });
        }
        parse_config(body.as_bytes())
    }

    /// Download a blob into `cache_dir/<hex>`, verifying its sha256. Cached hits
    /// (already present, named by digest) are returned without re-downloading.
    pub fn fetch_blob(&mut self, digest: &str, cache_dir: &Path) -> Result<PathBuf> {
        let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
        let path = cache_dir.join(hex);
        if path.exists() {
            return Ok(path);
        }
        let url = format!("{}/blobs/{}", self.base, digest);
        let mut reader = self.get(&url, "application/octet-stream")?.into_reader();
        // Stream to a temp file while hashing, with a hard size cap, instead of
        // buffering the whole (possibly hostile) blob in memory. The temp name
        // means a cache hit is always a complete, verified file.
        let tmp = cache_dir.join(format!("{hex}.partial"));
        let mut out = std::fs::File::create(&tmp)?;
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 64 * 1024];
        let mut total: u64 = 0;
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > MAX_BLOB {
                let _ = std::fs::remove_file(&tmp);
                return Err(Error::Registry(format!("blob {hex} exceeds {MAX_BLOB} bytes")));
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])?;
        }
        out.flush()?;
        drop(out);

        let got = hex::encode(hasher.finalize());
        if got != hex {
            let _ = std::fs::remove_file(&tmp);
            return Err(Error::Digest {
                want: hex.to_string(),
                got,
            });
        }
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }
}

fn parse_manifest(v: &serde_json::Value) -> Result<Manifest> {
    let config_digest = v["config"]["digest"]
        .as_str()
        .ok_or_else(|| Error::Registry("manifest has no config digest".into()))?
        .to_string();
    let layers = v["layers"]
        .as_array()
        .ok_or_else(|| Error::Registry("manifest has no layers".into()))?
        .iter()
        .filter_map(|l| l["digest"].as_str().map(|d| Layer { digest: d.to_string() }))
        .collect();
    Ok(Manifest {
        config_digest,
        layers,
    })
}

fn pick_arm64(index: &serde_json::Value) -> Result<String> {
    let manifests = index["manifests"]
        .as_array()
        .ok_or_else(|| Error::Registry("index has no manifests".into()))?;
    for m in manifests {
        let plat = &m["platform"];
        let arch = plat["architecture"].as_str().unwrap_or("");
        let os = plat["os"].as_str().unwrap_or("");
        if arch == "arm64" && os == "linux" {
            if let Some(d) = m["digest"].as_str() {
                return Ok(d.to_string());
            }
        }
    }
    Err(Error::Registry("no arm64/linux image in index".into()))
}

/// Parse an OCI image config blob into the run bits amber needs. Shared by the
/// registry path and the local-tar loader.
pub(crate) fn parse_config(body: &[u8]) -> Result<ImageConfig> {
    let v: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| Error::Json(e.to_string()))?;
    let c = &v["config"];
    Ok(ImageConfig {
        entrypoint: str_list(&c["Entrypoint"]),
        cmd: str_list(&c["Cmd"]),
        env: str_list(&c["Env"]),
        working_dir: c["WorkingDir"].as_str().filter(|s| !s.is_empty()).map(String::from),
    })
}

pub(crate) fn str_list(v: &serde_json::Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

/// Pull `key="value"` out of a `Bearer key="value",...` challenge string.
fn challenge_field(challenge: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=\"");
    let start = challenge.find(&needle)? + needle.len();
    let rest = &challenge[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_official_image_gets_library_prefix_and_latest() {
        let r = Reference::parse("alpine").unwrap();
        assert_eq!(r.registry, DEFAULT_REGISTRY);
        assert_eq!(r.repository, "library/alpine");
        assert_eq!(r.reference, "latest");
    }

    #[test]
    fn explicit_tag_is_parsed() {
        let r = Reference::parse("alpine:3.20").unwrap();
        assert_eq!(r.repository, "library/alpine");
        assert_eq!(r.reference, "3.20");
    }

    #[test]
    fn registry_host_is_split_off() {
        let r = Reference::parse("ghcr.io/user/img:tag").unwrap();
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.repository, "user/img");
        assert_eq!(r.reference, "tag");
    }

    #[test]
    fn localhost_with_port_is_a_registry_not_a_tag() {
        let r = Reference::parse("localhost:5000/img").unwrap();
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.repository, "img");
        assert_eq!(r.reference, "latest");
    }

    #[test]
    fn digest_reference_is_parsed() {
        let r = Reference::parse("repo@sha256:abc").unwrap();
        assert_eq!(r.reference, "sha256:abc");
    }

    #[test]
    fn empty_reference_errors() {
        assert!(Reference::parse("").is_err());
    }
}
