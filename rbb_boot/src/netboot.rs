//! DHCP boot URLs, signed native manifests and a bounded boot-oriented PXE/iPXE
//! subset. No shell interpreter, script downloads or signature downgrade paths.
use crate::{
    fetch::{self, Policy},
    invalid, local,
    model::{Artifact, BootConfig, BootEntry, Manifest},
    network::NetState,
    verification,
};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::Path;
use url::Url;

fn resolve(base: &Url, path: &str) -> io::Result<Artifact> {
    if path.contains(['\0', '\r', '\n']) {
        return Err(invalid("invalid remote artifact path"));
    }
    let url = base.join(path).map_err(io::Error::other)?;
    fetch::validate_url(&url)?;
    Ok(Artifact::local(url.to_string()))
}
fn signature(source: &Url, config: &BootConfig, policy: &Policy) -> io::Result<Vec<u8>> {
    if let Some(path) = &config.signature {
        if path.contains("://") {
            return fetch::bytes(&Url::parse(path).map_err(io::Error::other)?, policy, 1024);
        }
        let mut bytes = Vec::new();
        fs::File::open(path)?.take(1025).read_to_end(&mut bytes)?;
        if bytes.len() > 1024 {
            return Err(invalid("manifest signature exceeds limit"));
        }
        return Ok(bytes);
    }
    let mut url = source.clone();
    url.set_path(&format!("{}.sig", source.path()));
    fetch::bytes(&url, policy, 1024)
}
fn native(text: &str, uri: &Url, require_hashes: bool) -> io::Result<Vec<BootEntry>> {
    let mut manifest = Manifest::parse(text, require_hashes)?;
    for entry in &mut manifest.entries {
        for artifact in std::iter::once(&mut entry.kernel).chain(entry.initrd.iter_mut()) {
            let mut resolved = resolve(uri, &artifact.source)?;
            resolved.sha256 = artifact.sha256.take();
            *artifact = resolved;
        }
    }
    Ok(manifest.entries)
}

pub fn expand(text: &str, variables: &BTreeMap<String, String>) -> io::Result<String> {
    let mut result = text.to_string();
    for _ in 0..8 {
        let Some(start) = result.find("${") else {
            return Ok(result);
        };
        let end = result[start + 2..]
            .find('}')
            .ok_or_else(|| invalid("unterminated iPXE variable"))?
            + start
            + 2;
        let name = &result[start + 2..end];
        let value = variables
            .get(name)
            .ok_or_else(|| invalid(format!("undefined iPXE variable {name}")))?;
        result.replace_range(start..=end, value);
        if result.len() > 8192 {
            return Err(invalid("iPXE expansion exceeds limit"));
        }
    }
    if result.contains("${") {
        return Err(invalid("iPXE variable recursion/count limit"));
    }
    Ok(result)
}

fn ipxe(
    text: &str,
    uri: &Url,
    config: &BootConfig,
    policy: &Policy,
    depth: usize,
) -> io::Result<Vec<BootEntry>> {
    let mut variables = BTreeMap::new();
    if let Some(server) = uri.host_str() {
        variables.insert("next-server".into(), server.into());
    }
    if let Ok(state) = NetState::read() {
        variables.insert("net0/mac".into(), state.mac.clone());
        variables.insert("mac".into(), state.mac);
        if let Some(ip) = state.ipv4 {
            variables.insert("ip".into(), ip.to_string());
        }
    }
    let mut entry = None::<BootEntry>;
    for line in text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        let expanded = expand(line, &variables)?;
        let words = shlex::split(&expanded).ok_or_else(|| invalid("invalid iPXE quoting"))?;
        if words
            .iter()
            .any(|w| ["&&", "||", ";"].contains(&w.as_str()))
        {
            return Err(invalid("iPXE shell control operators not supported"));
        }
        let Some(command) = words.first().map(String::as_str) else {
            continue;
        };
        match command {
            "set" if words.len() >= 3 => {
                variables.insert(words[1].clone(), words[2..].join(" "));
            }
            "kernel" if words.len() >= 2 => {
                if entry.is_some() {
                    return Err(invalid("multiple iPXE kernel commands without boot"));
                }
                entry = Some(BootEntry {
                    id: "ipxe".into(),
                    title: "iPXE network boot".into(),
                    architecture: std::env::consts::ARCH.into(),
                    kernel: resolve(uri, &words[1])?,
                    initrd: Vec::new(),
                    cmdline: words[2..].join(" "),
                });
            }
            "initrd" if words.len() == 2 => entry
                .as_mut()
                .ok_or_else(|| invalid("iPXE initrd before kernel"))?
                .initrd
                .push(resolve(uri, &words[1])?),
            "imgargs" if words.len() >= 3 => {
                entry
                    .as_mut()
                    .ok_or_else(|| invalid("iPXE imgargs before kernel"))?
                    .cmdline = words[2..].join(" ")
            }
            "boot" if words.len() == 1 => {
                let entry = entry
                    .take()
                    .ok_or_else(|| invalid("iPXE boot before kernel"))?;
                entry.validate(config.require_hashes)?;
                return Ok(vec![entry]);
            }
            "chain" if words.len() == 2 => {
                if entry.is_some() {
                    return Err(invalid("iPXE chain after kernel"));
                }
                let next = uri.join(&words[1]).map_err(io::Error::other)?;
                return catalog_inner(&next, config, policy, depth + 1);
            }
            "echo" => {}
            "dhcp" if words.len() == 1 => {} // network was already configured
            _ => return Err(invalid(format!("unsupported iPXE command: {command}"))),
        }
    }
    Err(invalid("iPXE script has no boot or chain command"))
}

fn pxe_text(text: &str, base: &Url, config: &BootConfig) -> io::Result<Vec<BootEntry>> {
    if config.require_hashes {
        return Err(invalid(
            "PXELINUX has no artifact digests; use a signed native manifest",
        ));
    }
    let mut entries = local::parse_pxe(text, &|path| resolve(base, path))?;
    if entries.is_empty() {
        return Err(invalid("no PXELINUX labels"));
    }
    if let Some(default) = text.lines().find_map(|line| {
        let (word, value) = line.trim().split_once(char::is_whitespace)?;
        word.eq_ignore_ascii_case("default")
            .then(|| format!("extlinux-{}", value.trim()))
    }) {
        if let Some(index) = entries.iter().position(|e| e.id == default) {
            entries.swap(0, index);
        }
    }
    Ok(entries)
}
fn pxe_base(uri: &Url) -> io::Result<Url> {
    if let Some((prefix, _)) = uri.path().split_once("/pxelinux.cfg/") {
        let mut base = uri.clone();
        base.set_path(&format!("{prefix}/"));
        return Ok(base);
    }
    uri.join(".").map_err(io::Error::other)
}
fn probe_names(state: Option<&NetState>) -> Vec<String> {
    let mut result = Vec::new();
    if let Some(state) = state {
        if !state.mac.is_empty() {
            result.push(format!("01-{}", state.mac.replace(':', "-")));
        }
        if let Some(ip) = state.ipv4 {
            let hex = ip
                .octets()
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<String>();
            for length in (1..=8).rev() {
                result.push(hex[..length].into());
            }
        }
    }
    result.push("default".into());
    result
}

fn catalog_inner(
    uri: &Url,
    config: &BootConfig,
    policy: &Policy,
    depth: usize,
) -> io::Result<Vec<BootEntry>> {
    if depth > 4 {
        return Err(invalid("network config chain depth exceeds limit"));
    }
    fetch::validate_url(uri)?;
    let mut body = Vec::new();
    let mut effective = uri.clone();
    let fetched = fetch::open_resolved(uri, policy).and_then(|(reader, resolved)| {
        effective = resolved;
        reader.take(1024 * 1024 + 1).read_to_end(&mut body)
    });
    let uri = &effective;
    if let Some(key) = &config.manifest_key {
        fetched?;
        if body.len() > 1024 * 1024 {
            return Err(invalid("signed manifest exceeds 1MiB"));
        }
        verification::verify(&body, Path::new(key), &signature(uri, config, policy)?)?;
        return native(
            std::str::from_utf8(&body).map_err(io::Error::other)?,
            uri,
            true,
        );
    }
    if fetched.is_ok() {
        // Direct Linux image URL, without an initrd. Integrity-required boot
        // must instead use a manifest that binds the expected digest.
        if (body.len() >= 64 && &body[56..60] == b"ARMd")
            || (body.len() >= 0x238 && &body[0x202..0x206] == b"HdrS")
        {
            if config.require_hashes {
                return Err(invalid("direct kernel URL needs a digest-bearing manifest"));
            }
            return Ok(vec![BootEntry {
                id: "network-kernel".into(),
                title: "direct network kernel".into(),
                architecture: std::env::consts::ARCH.into(),
                kernel: resolve(uri, uri.as_str())?,
                initrd: Vec::new(),
                cmdline: String::new(),
            }]);
        }
        if body.len() > 1024 * 1024 {
            return Err(invalid("boot config exceeds 1MiB"));
        }
        if let Ok(text) = std::str::from_utf8(&body) {
            if text.trim_start().starts_with("#!ipxe") {
                return ipxe(text, uri, config, policy, depth);
            }
            if text.trim_start().starts_with('{') || text.contains("[[entries]]") {
                return native(text, uri, config.require_hashes);
            }
            if text
                .lines()
                .any(|l| l.trim_start().to_ascii_lowercase().starts_with("label "))
            {
                return pxe_text(text, &pxe_base(uri)?, config);
            }
        }
    }
    if let Err(error) = fetched {
        if !uri.path().ends_with("pxelinux.0") {
            return Err(error);
        }
    }
    if config.require_hashes {
        return Err(invalid("no digest-bearing native network manifest"));
    }
    let base = pxe_base(uri)?;
    let state = NetState::read().ok();
    for name in probe_names(state.as_ref()) {
        let candidate = base
            .join(&format!("pxelinux.cfg/{name}"))
            .map_err(io::Error::other)?;
        if let Ok(bytes) = fetch::bytes(&candidate, policy, 1024 * 1024) {
            return pxe_text(
                std::str::from_utf8(&bytes).map_err(io::Error::other)?,
                &base,
                config,
            );
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no native, iPXE or PXELINUX boot configuration found",
    ))
}
pub fn catalog(uri: &Url, config: &BootConfig, policy: &Policy) -> io::Result<Vec<BootEntry>> {
    if config.manifest_key.is_none() {
        eprintln!("rbb-netboot: unsigned boot config; trust the server/transport or configure manifest_key");
    }
    catalog_inner(uri, config, policy, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expansion_and_pxe_search_are_bounded() {
        let vars = BTreeMap::from([("a".into(), "${a}".into())]);
        assert!(expand("${a}", &vars).is_err());
        assert!(expand("${missing}", &BTreeMap::new()).is_err());
        let state = NetState {
            interface: "eth0".into(),
            mac: "52:54:00:12:34:56".into(),
            ipv4: Some("10.0.2.15".parse().unwrap()),
            boot_url: None,
        };
        let names = probe_names(Some(&state));
        assert_eq!(names[0], "01-52-54-00-12-34-56");
        assert_eq!(names[1], "0A00020F");
        assert_eq!(names.last().unwrap(), "default");
    }
}
