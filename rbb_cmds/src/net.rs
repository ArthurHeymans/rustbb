use rbb_boot::{
    invalid,
    network::{self, NetConfig},
};
use std::io;
use std::path::Path;

pub fn dhclient(args: &[String]) -> io::Result<()> {
    let mut config = NetConfig::default();
    match args {
        [] => {}
        [name] if !name.starts_with('-') => config.interface = Some(name.clone()),
        _ => return Err(invalid("dhclient [INTERFACE] (one-shot early-boot lease)")),
    }
    let lease = network::configure(&config)?;
    println!(
        "interface={} ipv4={:?} boot={:?}",
        lease.interface, lease.ipv4, lease.boot_url
    );
    Ok(())
}
pub fn pxeboot(args: &[String]) -> io::Result<()> {
    let mut config = NetConfig::default();
    let mut boot = vec!["--source".into(), "dhcp".into()];
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--interface" => {
                config.interface = Some(
                    args.next()
                        .ok_or_else(|| invalid("--interface needs a name"))?
                        .clone(),
                )
            }
            "--dhcp-timeout-ms" => {
                config.timeout_ms = args
                    .next()
                    .ok_or_else(|| invalid("--dhcp-timeout-ms needs a number"))?
                    .parse()
                    .map_err(io::Error::other)?
            }
            _ => boot.push(arg.clone()),
        }
    }
    network::configure(&config)?;
    crate::boot::boot(&boot)
}
pub fn wget(args: &[String]) -> io::Result<()> {
    let (source, output) = match args {
        [flag, output, source] if flag == "-O" => (source, output),
        [source, flag, output] if flag == "-O" => (source, output),
        _ => return Err(invalid("wget -O OUTPUT URL")),
    };
    let output = Path::new(output);
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    rbb_boot::fetch::download(source, output, parent)
}
