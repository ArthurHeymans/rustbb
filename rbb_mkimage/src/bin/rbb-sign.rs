//! Host-side signing helper. Private material is read from a private file,
//! never a command-line value, and is never printed or embedded into an image.
use clap::Parser;
use ed25519_dalek::Signer;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Sign exact boot-manifest bytes with an Ed25519 seed")]
struct Args {
    #[arg(long)]
    key: PathBuf,
    #[arg(long)]
    manifest: PathBuf,
    #[arg(long)]
    signature: PathBuf,
    #[arg(long)]
    public_key: PathBuf,
}
fn encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn main() -> io::Result<()> {
    let args = Args::parse();
    if fs::metadata(&args.key)?.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::other(
            "signing seed file must not be accessible to group/other",
        ));
    }
    let mut text = String::new();
    fs::File::open(args.key)?
        .take(1025)
        .read_to_string(&mut text)?;
    let seed = rbb_boot::verification::hex::<32>(&text)?;
    let key = ed25519_dalek::SigningKey::from_bytes(&seed);
    let mut message = Vec::new();
    fs::File::open(args.manifest)?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut message)?;
    if message.len() > 1024 * 1024 {
        return Err(io::Error::other("manifest exceeds 1MiB"));
    }
    fs::write(args.signature, encode(&key.sign(&message).to_bytes()))?;
    fs::write(args.public_key, encode(key.verifying_key().as_bytes()))?;
    Ok(())
}
