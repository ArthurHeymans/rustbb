use clap::{Parser, ValueEnum};
use rbb_mkimage::{base_image, Entry};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Clone, Copy, ValueEnum)]
enum Compression {
    None,
    Gzip,
}

#[derive(Parser)]
#[command(about = "Build a static Rust rescue initramfs (newc cpio)")]
struct Args {
    #[arg(short, long, default_value = "initramfs.cpio.gz")]
    output: PathBuf,
    /// Prebuilt static rbb multicall executable
    #[arg(long, required_unless_present = "build", conflicts_with = "build")]
    busybox: Option<PathBuf>,
    /// Build rbb from this workspace before packaging (requires target installed)
    #[arg(long)]
    build: bool,
    /// Source workspace used by --build
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    #[arg(long, default_value = "x86_64-unknown-linux-musl")]
    target: String,
    /// Root filesystem overlay; no overwriting generated paths
    #[arg(long)]
    overlay: Vec<PathBuf>,
    /// Include a host file/directory/symlink at an image path: SOURCE:DEST
    #[arg(short = 'f', long = "file")]
    files: Vec<String>,
    #[arg(long, value_enum, default_value = "gzip")]
    compress: Compression,
}

fn build(target: &str, workspace: &std::path::Path) -> io::Result<PathBuf> {
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

fn main() -> io::Result<()> {
    let args = Args::parse();
    let binary = if args.build {
        build(&args.target, &args.workspace)?
    } else {
        args.busybox.unwrap()
    };
    let mut image = base_image(fs::read(binary)?)?;
    for overlay in args.overlay {
        if !overlay.is_dir() {
            return Err(io::Error::other("overlay must be a directory"));
        }
        let mut entries: Vec<_> = fs::read_dir(overlay)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            image.add_host(
                &entry.path(),
                entry
                    .file_name()
                    .to_str()
                    .ok_or_else(|| io::Error::other("non-UTF8 overlay name"))?,
            )?;
        }
    }
    for mapping in args.files {
        let (source, dest) = mapping
            .split_once(':')
            .ok_or_else(|| io::Error::other("--file expects SOURCE:DEST"))?;
        image.add_host(&PathBuf::from(source), dest)?;
    }
    if !image.entries.contains_key("etc/rbb/init.toml") {
        image.insert(
            "etc/rbb/init.toml",
            Entry::file(
                include_bytes!("../../examples/initramfs/init.toml").to_vec(),
                0o644,
            ),
        )?;
    }
    let init_config = &image.entries["etc/rbb/init.toml"];
    if init_config.mode & 0o170000 != 0o100000 {
        return Err(io::Error::other("init.toml must be a regular file"));
    }
    let text = std::str::from_utf8(&init_config.data).map_err(io::Error::other)?;
    rbb_init::config::Config::parse(text)?;
    // Build completely before replacing the requested output, even with compression.
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
        .unwrap_or_else(|| std::path::Path::new("."));
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
