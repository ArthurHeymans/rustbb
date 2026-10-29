//! Early-boot network configuration using rtnetlink and mozim's DHCP engine.
use crate::invalid;
use futures_util::TryStreamExt;
use ipnet::IpNet;
use rtnetlink::{Handle, LinkUnspec, RouteMessageBuilder};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;
use url::Url;

fn yes() -> bool {
    true
}
fn timeout() -> u64 {
    15_000
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetConfig {
    pub interface: Option<String>,
    #[serde(default = "yes")]
    pub dhcp: bool,
    #[serde(default)]
    pub addresses: Vec<String>,
    pub gateway: Option<IpAddr>,
    #[serde(default)]
    pub dns: Vec<IpAddr>,
    #[serde(default = "timeout")]
    pub timeout_ms: u64,
}
impl Default for NetConfig {
    fn default() -> Self {
        Self {
            interface: None,
            dhcp: true,
            addresses: Vec::new(),
            gateway: None,
            dns: Vec::new(),
            timeout_ms: timeout(),
        }
    }
}
impl NetConfig {
    pub fn validate(&self) -> io::Result<()> {
        if self.timeout_ms == 0
            || self.timeout_ms > 300_000
            || self.addresses.len() > 16
            || self.dns.len() > 8
            || self
                .interface
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > 15 || s.contains(['/', '\0', ' ']))
        {
            return Err(invalid(
                "invalid network interface, timeout or address count",
            ));
        }
        for address in &self.addresses {
            address.parse::<IpNet>().map_err(io::Error::other)?;
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct NetState {
    pub interface: String,
    pub mac: String,
    pub ipv4: Option<Ipv4Addr>,
    pub boot_url: Option<String>,
}
impl NetState {
    pub fn read() -> io::Result<Self> {
        toml::from_str(&fs::read_to_string("/run/rbb/network.toml")?).map_err(io::Error::other)
    }
}
fn net_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

async fn index(handle: &Handle, name: &str) -> io::Result<u32> {
    let mut links = handle.link().get().match_name(name).execute();
    links
        .try_next()
        .await
        .map_err(net_error)?
        .map(|l| l.header.index)
        .ok_or_else(|| invalid(format!("interface not found: {name}")))
}
async fn up(handle: &Handle, name: &str, enabled: bool) -> io::Result<u32> {
    let index = index(handle, name).await?;
    let builder = LinkUnspec::new_with_index(index);
    let message = if enabled {
        builder.up().build()
    } else {
        builder.down().build()
    };
    handle
        .link()
        .set(message)
        .execute()
        .await
        .map_err(net_error)?;
    Ok(index)
}
async fn address(handle: &Handle, index: u32, network: IpNet) -> io::Result<()> {
    handle
        .address()
        .add(index, network.addr(), network.prefix_len())
        .replace()
        .execute()
        .await
        .map_err(net_error)
}
async fn route(
    handle: &Handle,
    index: u32,
    destination: IpNet,
    gateway: Option<IpAddr>,
) -> io::Result<()> {
    let message = match destination {
        IpNet::V4(net) => {
            let mut builder = RouteMessageBuilder::<Ipv4Addr>::new()
                .destination_prefix(net.network(), net.prefix_len())
                .output_interface(index);
            if let Some(gateway) = gateway {
                let IpAddr::V4(gateway) = gateway else {
                    return Err(invalid("gateway address family mismatch"));
                };
                builder = builder.gateway(gateway);
            }
            builder.build()
        }
        IpNet::V6(net) => {
            let mut builder = RouteMessageBuilder::<std::net::Ipv6Addr>::new()
                .destination_prefix(net.network(), net.prefix_len())
                .output_interface(index);
            if let Some(gateway) = gateway {
                let IpAddr::V6(gateway) = gateway else {
                    return Err(invalid("gateway address family mismatch"));
                };
                builder = builder.gateway(gateway);
            }
            builder.build()
        }
    };
    handle
        .route()
        .add(message)
        .replace()
        .execute()
        .await
        .map_err(net_error)
}

fn option_text(lease: &mozim::DhcpV4Lease, code: u8) -> io::Result<Option<String>> {
    let Some(bytes) = lease.get_option_raw(code) else {
        return Ok(None);
    };
    if bytes.len() < 2 || bytes[0] != code || bytes[1] as usize != bytes.len() - 2 {
        return Err(invalid("malformed DHCP boot option"));
    }
    let text = std::str::from_utf8(&bytes[2..])
        .map_err(io::Error::other)?
        .trim_end_matches('\0');
    if text.contains(['\0', '\r', '\n']) {
        return Err(invalid("invalid DHCP boot string"));
    }
    Ok((!text.is_empty()).then(|| text.into()))
}

pub fn boot_url(lease: &mozim::DhcpV4Lease) -> io::Result<Option<String>> {
    let filename = option_text(lease, 67)?.unwrap_or_else(|| lease.boot_file_name.clone());
    if filename.is_empty() {
        return Ok(None);
    }
    if filename.contains("://") {
        let url = Url::parse(&filename).map_err(io::Error::other)?;
        crate::fetch::validate_url(&url)?;
        return Ok(Some(url.into()));
    }
    let server = option_text(lease, 66)?.unwrap_or_else(|| {
        if !lease.siaddr.is_unspecified() {
            lease.siaddr.to_string()
        } else if !lease.boot_server_name.is_empty() {
            lease.boot_server_name.clone()
        } else {
            lease.srv_id.to_string()
        }
    });
    if server.is_empty() || server == "0.0.0.0" || server.contains(['/', '@', '?', '#', '\0']) {
        return Err(invalid("invalid DHCP boot server"));
    }
    let base = Url::parse(&format!("tftp://{server}/")).map_err(io::Error::other)?;
    Ok(Some(base.join(&filename).map_err(io::Error::other)?.into()))
}

fn select_interface(config: &NetConfig) -> io::Result<String> {
    if let Some(interface) = &config.interface {
        return Ok(interface.clone());
    }
    let mut names: Vec<_> = fs::read_dir("/sys/class/net")?.collect::<Result<_, _>>()?;
    names.sort_by_key(|e| e.file_name());
    names
        .into_iter()
        .find(|e| {
            e.file_name() != "lo"
                && fs::read_to_string(e.path().join("type")).is_ok_and(|s| s.trim() == "1")
        })
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .ok_or_else(|| invalid("no Ethernet interface found"))
}

async fn configure_async(config: &NetConfig) -> io::Result<NetState> {
    let (connection, handle, _) = rtnetlink::new_connection().map_err(net_error)?;
    tokio::spawn(connection);
    up(&handle, "lo", true).await?;
    let name = select_interface(config)?;
    let index = up(&handle, &name, true).await?;
    let mac = fs::read_to_string(format!("/sys/class/net/{name}/address"))?
        .trim()
        .to_string();
    let mut state = NetState {
        interface: name.clone(),
        mac,
        ..NetState::default()
    };
    let mut dns = config.dns.clone();
    let mut search = None;
    if config.dhcp {
        let mut dhcp = mozim::DhcpV4Config::new(&name);
        dhcp.set_iface_index(index)
            .set_iface_mac(&state.mac)
            .map_err(net_error)?
            .use_mac_as_client_id();
        dhcp.set_host_name("rbb-initramfs")
            .set_timeout_sec((config.timeout_ms / 1000).max(1) as u32)
            .request_extra_dhcp_opts(&[66, 67]);
        let arch: u16 = if cfg!(target_arch = "aarch64") { 11 } else { 9 };
        dhcp.set_extra_option(93, &arch.to_be_bytes())
            .map_err(net_error)?;
        dhcp.set_extra_option(
            60,
            format!("PXEClient:Arch:{arch:05}:UNDI:000000").as_bytes(),
        )
        .map_err(net_error)?;
        let mut client = mozim::DhcpV4Client::init(dhcp, None)
            .await
            .map_err(net_error)?;
        let lease = loop {
            if let mozim::DhcpV4State::Done(lease) = client.run().await.map_err(net_error)? {
                break lease;
            }
        };
        address(
            &handle,
            index,
            IpNet::new(IpAddr::V4(lease.yiaddr), lease.prefix_length()).map_err(net_error)?,
        )
        .await?;
        if let Some(routes) = &lease.classless_routes {
            for entry in routes {
                route(
                    &handle,
                    index,
                    IpNet::new(IpAddr::V4(entry.destination), entry.prefix_length)
                        .map_err(net_error)?,
                    (!entry.router.is_unspecified()).then_some(IpAddr::V4(entry.router)),
                )
                .await?;
            }
        } else if let Some(gateway) = lease.gateways.as_ref().and_then(|g| g.first()) {
            route(
                &handle,
                index,
                "0.0.0.0/0".parse().unwrap(),
                Some(IpAddr::V4(*gateway)),
            )
            .await?;
        }
        if dns.is_empty() {
            dns.extend(lease.dns_srvs.iter().flatten().copied().map(IpAddr::V4));
        }
        search = lease.domain_name.clone().filter(|d| {
            d.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
        });
        state.ipv4 = Some(lease.yiaddr);
        state.boot_url = boot_url(&lease)?;
        eprintln!(
            "rbb-net: DHCP {} address={} boot={:?}",
            name, lease.yiaddr, state.boot_url
        );
    }
    for value in &config.addresses {
        address(&handle, index, value.parse().map_err(net_error)?).await?;
    }
    if let Some(gateway) = config.gateway {
        route(
            &handle,
            index,
            if gateway.is_ipv4() {
                "0.0.0.0/0"
            } else {
                "::/0"
            }
            .parse()
            .unwrap(),
            Some(gateway),
        )
        .await?;
    }
    if !dns.is_empty() {
        let mut text =
            String::from("# generated by rbb early boot\noptions timeout:1 attempts:2\n");
        for server in dns {
            text.push_str(&format!("nameserver {server}\n"));
        }
        if let Some(search) = search {
            text.push_str(&format!("search {search}\n"));
        }
        fs::write("/etc/resolv.conf", text)?;
    }
    fs::create_dir_all("/run/rbb")?;
    fs::write(
        "/run/rbb/network.toml",
        toml::to_string(&state).map_err(net_error)?,
    )?;
    Ok(state)
}

pub fn configure(config: &NetConfig) -> io::Result<NetState> {
    config.validate()?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            tokio::time::timeout(
                Duration::from_millis(config.timeout_ms),
                configure_async(config),
            )
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "network/DHCP setup timed out"))?
        })
}

pub fn ip(args: &[String]) -> io::Result<()> {
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
        let (connection, handle, _) = rtnetlink::new_connection().map_err(net_error)?;
        tokio::spawn(connection);
        let words: Vec<_> = args.iter().map(String::as_str).collect();
        match words.as_slice() {
            ["link"] | ["link", "show"] => {
                let mut stream = handle.link().get().execute();
                while let Some(message) = stream.try_next().await.map_err(net_error)? { println!("{}: {:?}", message.header.index, message.attributes); }
            }
            ["link", "set", name, state @ ("up" | "down")] => { up(&handle, name, *state == "up").await?; }
            ["addr"] | ["addr", "show"] => {
                let mut stream = handle.address().get().execute();
                while let Some(message) = stream.try_next().await.map_err(net_error)? { println!("{} /{} {:?}", message.header.index, message.header.prefix_len, message.attributes); }
            }
            ["addr", "add", cidr, "dev", name] => { address(&handle, index(&handle, name).await?, cidr.parse().map_err(net_error)?).await?; }
            ["route"] | ["route", "show"] => {
                for message in [RouteMessageBuilder::<Ipv4Addr>::new().build(), RouteMessageBuilder::<std::net::Ipv6Addr>::new().build()] {
                    let mut stream = handle.route().get(message).execute();
                    while let Some(message) = stream.try_next().await.map_err(net_error)? { println!("{:?} {:?}", message.header, message.attributes); }
                }
            }
            ["route", "add", destination, "via", gateway, "dev", name] => {
                let gateway: IpAddr = gateway.parse().map_err(net_error)?;
                let destination = if *destination == "default" { if gateway.is_ipv4() { "0.0.0.0/0" } else { "::/0" } } else { destination };
                route(&handle, index(&handle, name).await?, destination.parse().map_err(net_error)?, Some(gateway)).await?;
            }
            _ => return Err(invalid("ip link [show|set IF up|down] | ip addr [show|add CIDR dev IF] | ip route [show|add PREFIX via IP dev IF]")),
        }
        Ok(())
    })
}

pub fn saved_boot_url() -> io::Result<String> {
    NetState::read()?
        .boot_url
        .ok_or_else(|| invalid("DHCP provided no boot URL; configure an explicit boot source"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_network_before_privileged_calls() {
        assert!(NetConfig {
            timeout_ms: 0,
            ..NetConfig::default()
        }
        .validate()
        .is_err());
        assert!(NetConfig {
            addresses: vec!["invalid".into()],
            ..NetConfig::default()
        }
        .validate()
        .is_err());
        assert!(NetConfig {
            interface: Some("../bad".into()),
            ..NetConfig::default()
        }
        .validate()
        .is_err());
    }
}
