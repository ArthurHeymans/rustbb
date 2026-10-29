//! Signature verification over exact manifest bytes; no custom cryptography.
use crate::invalid;
use std::fs;
use std::io::{self, Read};
use std::path::Path;

pub fn hex<const N: usize>(text: &str) -> io::Result<[u8; N]> {
    let text = text.trim();
    if text.len() != N * 2 || !text.is_ascii() {
        return Err(invalid("invalid hex length/encoding"));
    }
    let mut result = [0; N];
    for (i, slot) in result.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).map_err(io::Error::other)?;
    }
    Ok(result)
}
pub fn verify(message: &[u8], public_key: &Path, signature: &[u8]) -> io::Result<()> {
    let mut key = String::new();
    fs::File::open(public_key)?
        .take(1025)
        .read_to_string(&mut key)?;
    if key.len() > 1024 || signature.len() > 1024 {
        return Err(invalid("key/signature exceeds limit"));
    }
    let key =
        ed25519_dalek::VerifyingKey::from_bytes(&hex::<32>(&key)?).map_err(io::Error::other)?;
    let signature = ed25519_dalek::Signature::from_bytes(&hex::<64>(
        std::str::from_utf8(signature).map_err(io::Error::other)?,
    )?);
    key.verify_strict(message, &signature)
        .map_err(|_| invalid("manifest signature verification failed"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Signer;
    #[test]
    fn signature_binds_manifest_bytes() {
        let key = ed25519_dalek::SigningKey::from_bytes(&[17; 32]);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("public-key");
        let encode = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        fs::write(&path, encode(key.verifying_key().as_bytes())).unwrap();
        let signature = encode(&key.sign(b"manifest").to_bytes());
        assert!(verify(b"manifest", &path, signature.as_bytes()).is_ok());
        assert!(verify(b"modified", &path, signature.as_bytes()).is_err());
        assert!(hex::<32>("bad").is_err());
    }
}
