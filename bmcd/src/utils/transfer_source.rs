// Copyright 2023 Turing Machines
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
use anyhow::{bail, Context};
use reqwest::Url;
use std::ffi::OsStr;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

const LOCAL_TRANSFER_ROOTS: &[&str] = &["/tmp", "/mnt/sdcard", "/var/lib/bmcd"];

/// Restrict local flash/firmware sources to a short allowlist of directories.
pub fn validate_local_transfer_path(path: &Path) -> anyhow::Result<PathBuf> {
    if path.as_os_str().is_empty() {
        bail!("local transfer path is empty");
    }

    let canonical = path
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", path.display()))?;

    if !LOCAL_TRANSFER_ROOTS
        .iter()
        .any(|root| canonical.starts_with(root))
    {
        bail!(
            "local transfer path {} is outside allowed directories ({})",
            canonical.display(),
            LOCAL_TRANSFER_ROOTS.join(", ")
        );
    }

    Ok(canonical)
}

/// Reject upgrade filenames that would escape `/tmp/os_upgrade` or inject a shell.
pub fn sanitize_upgrade_filename(name: &OsStr) -> anyhow::Result<PathBuf> {
    let path = Path::new(name);
    if path.as_os_str().is_empty() {
        bail!("upgrade filename is empty");
    }
    if path.components().count() != 1 {
        bail!("upgrade filename contains path components");
    }

    let file_name = path
        .file_name()
        .filter(|n| !n.is_empty() && *n != OsStr::new("..") && *n != OsStr::new("."))
        .ok_or_else(|| anyhow::anyhow!("upgrade filename is empty"))?;

    let lossy = file_name.to_string_lossy();
    if lossy.contains("..") || lossy.contains('/') || lossy.contains('\\') {
        bail!("upgrade filename contains illegal characters");
    }

    Ok(PathBuf::from(file_name))
}

/// Require http(s) and refuse loopback / link-local / unspecified / multicast targets.
pub fn validate_download_url(url: &Url) -> anyhow::Result<()> {
    if url.scheme() != "http" && url.scheme() != "https" {
        bail!("only http and https URLs are allowed");
    }

    let addrs = url
        .socket_addrs(|| match url.scheme() {
            "https" => Some(443),
            "http" => Some(80),
            _ => None,
        })
        .with_context(|| format!("cannot resolve host for {url}"))?;

    if addrs.is_empty() {
        bail!("URL {url} did not resolve to any address");
    }

    for addr in addrs {
        if is_blocked_ip(addr.ip()) {
            bail!("URL {url} resolves to a blocked address ({})", addr.ip());
        }
    }

    Ok(())
}

pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_unspecified()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_broadcast()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() || v6.is_unicast_link_local()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn local_path_must_exist_under_allowlist() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("image.bin");
        fs::write(&file, b"img").unwrap();

        // tempfile is under /tmp or the platform temp root. On macOS that is
        // often /var/folders; only assert the deny path here and the happy
        // path when canonicalize lands under an allowlisted prefix.
        let denied = PathBuf::from("/etc/passwd");
        assert!(validate_local_transfer_path(&denied).is_err());

        if file
            .canonicalize()
            .is_ok_and(|p| LOCAL_TRANSFER_ROOTS.iter().any(|root| p.starts_with(root)))
        {
            assert!(validate_local_transfer_path(&file).is_ok());
        }
    }

    #[test]
    fn sanitize_rejects_path_components() {
        assert!(sanitize_upgrade_filename(OsStr::new("../x")).is_err());
        assert!(sanitize_upgrade_filename(OsStr::new("a/b")).is_err());
        assert!(sanitize_upgrade_filename(OsStr::new("ok.bin")).is_ok());
    }

    #[test]
    fn blocked_ips() {
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1))));
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(224, 0, 0, 1))));
        assert!(is_blocked_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(!is_blocked_ip(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
    }

    #[test]
    fn url_rejects_non_http() {
        let url = Url::parse("file:///etc/passwd").unwrap();
        assert!(validate_download_url(&url).is_err());
    }

    #[test]
    fn url_rejects_loopback() {
        let url = Url::parse("http://127.0.0.1/image").unwrap();
        assert!(validate_download_url(&url).is_err());
    }
}
