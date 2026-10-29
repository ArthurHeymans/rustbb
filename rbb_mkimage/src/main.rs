use clap::{Parser, ValueEnum};
use rbb_mkimage::{command_manifest, preset_image, Conflict, Entry, Image};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Copy, ValueEnum)]
enum Compression {
    None,
    Gzip,
}

#[derive(Parser)]
#[command(about = "Build a reproducible Rust initramfs (newc cpio)")]
struct Args {
    #[arg(short, long, default_value = "initramfs.cpio.gz")]
    output: PathBuf,
    /// Prebuilt static rbb with its embedded target command manifest
    #[arg(long, required_unless_present = "build", conflicts_with = "build")]
    busybox: Option<PathBuf>,
    #[arg(long)]
    build: bool,
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    #[arg(long, default_value = "x86_64-unknown-linux-musl")]
    target: String,
    /// Select installed command aliases (does not strip code from a prebuilt rbb)
    #[arg(long, default_value = "rescue", value_parser = ["minimal", "rescue", "diskboot", "netboot"])]
    preset: String,
    /// Explicit alias selection instead of the preset; must include init
    #[arg(long, value_delimiter = ',')]
    commands: Vec<String>,
    /// Base newc/CRC or gzip archive; repeat to merge multiple bases in order
    #[arg(long)]
    base: Vec<PathBuf>,
    /// Duplicate-path policy for bases, generated entries, overlays and files
    #[arg(long, value_enum, default_value = "error")]
    conflict: Conflict,
    /// Preserve a base /init as /inito before installing rbb's init
    #[arg(long, requires = "base")]
    preserve_init: bool,
    /// Explicit image executable to install as /init (no host execution)
    #[arg(long = "init")]
    init: Option<String>,
    /// Shell for the generated console service; conflicts with a supplied config
    #[arg(long)]
    shell: Option<String>,
    /// Run this foreground oneshot before the console in the generated config
    #[arg(long)]
    uinit: Option<String>,
    #[arg(long)]
    overlay: Vec<PathBuf>,
    #[arg(short = 'f', long = "file")]
    files: Vec<String>,
    #[arg(long, value_enum, default_value = "gzip")]
    compress: Compression,
}

fn build(target: &str, workspace: &Path) -> io::Result<PathBuf> {
    let output = Command::new("cargo")
        .current_dir(workspace)
        .args([
            "build",
            "--release",
            "-p",
            "rbb-cmds",
            "--bin",
            "rbb",
            "--target",
            target,
            "--message-format=json-render-diagnostics",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other("cargo build failed"));
    }
    for line in output.stdout.split(|c| *c == b'\n') {
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) {
            if value["reason"] == "compiler-artifact" && value["target"]["name"] == "rbb" {
                if let Some(path) = value["executable"].as_str() {
                    return Ok(path.into());
                }
            }
        }
    }
    Err(io::Error::other("cargo did not report an rbb executable"))
}

fn extras(image: &mut Image, other: Image, conflict: Conflict) -> io::Result<()> {
    for reserved in [
        "bbin/rbb",
        "etc/rbb/commands.json",
        "etc/rbb/third-party-notices.txt",
        "etc/rbb/initramfs-release",
    ] {
        if other.entries.contains_key(reserved) {
            return Err(io::Error::other(format!(
                "overlay replaces builder-managed path: {reserved}"
            )));
        }
    }
    image.merge(other, conflict)
}

fn executable(image: &Image, path: &str) -> io::Result<()> {
    let file = image.resolve(path)?;
    if file.mode & 0o170000 != 0o100000 || file.mode & 0o111 == 0 {
        return Err(io::Error::other(format!(
            "image executable is not a regular executable: {path}"
        )));
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let args = Args::parse();
    let binary = if args.build {
        build(&args.target, &args.workspace)?
    } else {
        args.busybox.unwrap()
    };
    let binary = fs::read(binary)?;
    let expected = command_manifest(&binary)?;
    let generated = preset_image(binary, &args.preset, &args.commands)?;
    let mut image = Image::default();
    for base in args.base {
        image.merge(Image::read_base(&base, args.conflict)?, args.conflict)?;
    }
    if args.preserve_init {
        image.preserve_init()?;
    }
    image.merge(generated, args.conflict)?;
    for overlay in args.overlay {
        if !overlay.is_dir() {
            return Err(io::Error::other("overlay must be a directory"));
        }
        let mut extra = Image::default();
        let mut entries: Vec<_> = fs::read_dir(overlay)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            extra.add_host(
                &entry.path(),
                entry
                    .file_name()
                    .to_str()
                    .ok_or_else(|| io::Error::other("non-UTF8 overlay name"))?,
            )?;
        }
        extras(&mut image, extra, args.conflict)?;
    }
    for mapping in args.files {
        let (source, dest) = mapping
            .split_once(':')
            .ok_or_else(|| io::Error::other("--file expects SOURCE:DEST"))?;
        let mut extra = Image::default();
        extra.add_host(Path::new(source), dest)?;
        extras(&mut image, extra, args.conflict)?;
    }
    if let Some(init) = args.init {
        image.insert_with("init", Entry::link(&init), Conflict::Replace)?;
    }
    executable(&image, "init")?;
    if command_manifest(&image.resolve("bbin/rbb")?.data)? != expected {
        return Err(io::Error::other(
            "base rbb command manifest differs from supplied executable",
        ));
    }
    if !image.entries.contains_key("etc/rbb/init.toml") {
        let shell = args.shell.as_deref().unwrap_or("/bin/sh");
        executable(&image, shell)?;
        let mut text = String::new();
        if let Some(uinit) = args.uinit.as_deref() {
            executable(&image, uinit)?;
            text += &format!(
                "[[services]]\nname = 'uinit'\nkind = 'oneshot'\ncommand = [{}]\n\n",
                serde_json::to_string(uinit)?
            );
        }
        text += "[[services]]\nname = 'console'\n";
        text += &format!("command = [{}]\n", serde_json::to_string(shell)?);
        if args.uinit.is_some() {
            text += "after = ['uinit']\n";
        }
        image.insert("etc/rbb/init.toml", Entry::file(text.into_bytes(), 0o644))?;
    } else if args.shell.is_some() || args.uinit.is_some() {
        return Err(io::Error::other(
            "--shell/--uinit conflict with an existing init.toml",
        ));
    }
    let init_config = &image.entries["etc/rbb/init.toml"];
    if init_config.mode & 0o170000 != 0o100000 {
        return Err(io::Error::other("init.toml must be a regular file"));
    }
    rbb_init::config::Config::parse(
        std::str::from_utf8(&init_config.data).map_err(io::Error::other)?,
    )?;
    let mut archive = Vec::new();
    match args.compress {
        Compression::None => image.write_newc(&mut archive)?,
        Compression::Gzip => {
            let mut encoder = flate2::GzBuilder::new()
                .mtime(0)
                .operating_system(255)
                .write(&mut archive, flate2::Compression::default());
            image.write_newc(&mut encoder)?;
            encoder.finish()?;
        }
    }
    let parent = args
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&archive)?;
    file.as_file().sync_all()?;
    file.persist(&args.output).map_err(io::Error::other)?;
    eprintln!(
        "wrote {} ({} bytes, {} entries)",
        args.output.display(),
        archive.len(),
        image.entries.len()
    );
    Ok(())
}
