use rbb_boot::{
    invalid, loader,
    model::{Artifact, BootConfig, BootEntry},
};
use std::io;
use std::path::Path;

pub fn boot(args: &[String]) -> io::Result<()> {
    let mut config = BootConfig {
        source: "disk".into(),
        entry: None,
        append: String::new(),
        require_hashes: false,
        menu: false,
        timeout_ms: 5000,
    };
    let (mut list, mut load_only) = (false, false);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--source" => config.source = args.next().ok_or_else(|| invalid("--source needs a path or disk"))?.clone(),
            "--entry" => config.entry = Some(args.next().ok_or_else(|| invalid("--entry needs an id"))?.clone()),
            "--append" => config.append = args.next().ok_or_else(|| invalid("--append needs a command line"))?.clone(),
            "--timeout-ms" => config.timeout_ms = args.next().ok_or_else(|| invalid("--timeout-ms needs a number"))?.parse().map_err(io::Error::other)?,
            "--require-hashes" => config.require_hashes = true,
            "--menu" => config.menu = true,
            "--no-load" | "--list" => list = true,
            "--no-exec" => load_only = true,
            _ => return Err(invalid("boot [--source disk|DIR|MANIFEST] [--entry ID] [--append CMDLINE] [--menu] [--timeout-ms MS] [--require-hashes] [--no-load|--no-exec]")),
        }
    }
    config.validate()?;
    if list {
        let catalog = loader::catalog(&config.source, config.require_hashes)?;
        for entry in &catalog.entries {
            println!(
                "{}\t{}\t{}\t{}",
                entry.id, entry.architecture, entry.title, entry.kernel.source
            );
        }
        return Ok(());
    }
    if !load_only && !Path::new("/run/rbb/control").exists() {
        return Err(invalid(
            "boot execution requires the rbb PID 1 control socket; use --no-load or --no-exec",
        ));
    }
    loader::load(&config)?;
    if !load_only {
        print!(
            "{}",
            rbb_init::control(Path::new("/run/rbb/control"), &["kexec".into()])?
        );
    }
    Ok(())
}

pub fn kexec(args: &[String]) -> io::Result<()> {
    if args == ["-e"] {
        print!(
            "{}",
            rbb_init::control(Path::new("/run/rbb/control"), &["kexec".into()])?
        );
        return Ok(());
    }
    if args.first().map(String::as_str) != Some("-l") || args.len() < 2 {
        return Err(invalid(
            "kexec -l KERNEL [--initrd FILE]... [--append CMDLINE] [--sha256 DIGEST] | kexec -e",
        ));
    }
    let mut entry = BootEntry {
        id: "manual".into(),
        title: "manual kernel".into(),
        architecture: std::env::consts::ARCH.into(),
        kernel: Artifact::local(&args[1]),
        initrd: Vec::new(),
        cmdline: String::new(),
    };
    let mut args = args[2..].iter();
    while let Some(arg) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| invalid("kexec option needs a value"))?;
        match arg.as_str() {
            "--initrd" => entry.initrd.push(Artifact::local(value)),
            "--append" => entry.cmdline = value.into(),
            "--sha256" => entry.kernel.sha256 = Some(value.into()),
            _ => return Err(invalid("unknown kexec load option")),
        }
    }
    let staged = loader::Staged::prepare(&entry, Path::new("/run/rbb/boot/stage"), false)?;
    staged.load()
}
