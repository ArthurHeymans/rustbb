//! Bounded HTTP(S)/TFTP readers. Reuse ureq/rustls and the tftp_client codec;
//! its unbounded downloader is not used (it accepts unexpected TIDs/duplicates).
use crate::invalid;
use std::ffi::CString;
use std::fs;
use std::io::{self, Read};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tftp_client::parser::{Packet, RequestMode};
use url::Url;

#[derive(Clone)]
pub struct Policy {
    pub deadline: Instant,
    pub tls_ca: Option<PathBuf>,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            deadline: Instant::now() + Duration::from_secs(60),
            tls_ca: None,
        }
    }
}
impl Policy {
    pub fn remaining(&self) -> io::Result<Duration> {
        let time = self.deadline.saturating_duration_since(Instant::now());
        if time.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "network boot deadline exceeded",
            ));
        }
        Ok(time)
    }
}
pub fn validate_url(url: &Url) -> io::Result<()> {
    if !["http", "https", "tftp"].contains(&url.scheme())
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid(
            "boot URL must use http/https/tftp, with no credentials or fragment",
        ));
    }
    Ok(())
}

pub fn open(url: &Url, policy: &Policy) -> io::Result<Box<dyn Read>> {
    Ok(open_resolved(url, policy)?.0)
}

/// Retain the effective response URI so relative metadata paths follow redirects.
pub fn open_resolved(url: &Url, policy: &Policy) -> io::Result<(Box<dyn Read>, Url)> {
    validate_url(url)?;
    if url.scheme() == "tftp" {
        return Ok((Box::new(Tftp::new(url, policy.clone())?), url.clone()));
    }
    let mut tls = ureq::tls::TlsConfig::builder();
    if let Some(path) = &policy.tls_ca {
        let mut pem = Vec::new();
        fs::File::open(path)?
            .take(1024 * 1024 + 1)
            .read_to_end(&mut pem)?;
        if pem.len() > 1024 * 1024 {
            return Err(invalid("TLS CA bundle exceeds 1MiB"));
        }
        let mut certs = Vec::new();
        for item in ureq::tls::parse_pem(&pem) {
            match item.map_err(io::Error::other)? {
                ureq::tls::PemItem::Certificate(cert) => certs.push(cert),
                _ => return Err(invalid("CA bundle must contain only certificates")),
            }
        }
        if certs.is_empty() {
            return Err(invalid("empty TLS CA bundle"));
        }
        tls = tls.root_certs(ureq::tls::RootCerts::new_with_certs(&certs));
    }
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(policy.remaining()?))
        .https_only(url.scheme() == "https")
        .max_redirects(5)
        .proxy(None)
        .tls_config(tls.build())
        .build();
    let response = ureq::Agent::new_with_config(config)
        .get(url.as_str())
        .call()
        .map_err(io::Error::other)?;
    let effective =
        Url::parse(&ureq::ResponseExt::get_uri(&response).to_string()).map_err(io::Error::other)?;
    validate_url(&effective)?;
    Ok((Box::new(response.into_body().into_reader()), effective))
}

pub fn bytes(url: &Url, policy: &Policy, limit: usize) -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    open(url, policy)?
        .take(limit as u64 + 1)
        .read_to_end(&mut data)?;
    if data.len() > limit {
        return Err(invalid("download exceeds size limit"));
    }
    Ok(data)
}

pub fn artifact(source: &str, policy: &Policy) -> io::Result<Box<dyn Read>> {
    if source.contains("://") {
        return open(&Url::parse(source).map_err(io::Error::other)?, policy);
    }
    let file = fs::File::open(Path::new(source))?;
    if !file.metadata()?.is_file() {
        return Err(invalid("boot artifact must be a regular file"));
    }
    Ok(Box::new(file))
}

pub fn download(source: &str, output: &Path, parent: &Path) -> io::Result<()> {
    let url = Url::parse(source).map_err(io::Error::other)?;
    let input = open(&url, &Policy::default())?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    crate::loader::copy_verified(input, &mut file, 256 * 1024 * 1024, None)?;
    file.persist(output).map_err(io::Error::other)?;
    Ok(())
}

struct Tftp {
    socket: UdpSocket,
    server: SocketAddr,
    peer: Option<SocketAddr>,
    last: Vec<u8>,
    expected: u16,
    buffer: Vec<u8>,
    offset: usize,
    finished: bool,
    policy: Policy,
}
impl Tftp {
    fn new(url: &Url, policy: Policy) -> io::Result<Self> {
        let server = (
            url.host_str().ok_or_else(|| invalid("TFTP host missing"))?,
            url.port().unwrap_or(69),
        )
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| invalid("TFTP host resolution failed"))?;
        let socket = UdpSocket::bind(if server.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        })?;
        let filename = percent_encoding::percent_decode_str(url.path().trim_start_matches('/'))
            .decode_utf8()
            .map_err(io::Error::other)?;
        if filename.is_empty() || filename.len() > 1024 || url.query().is_some() {
            return Err(invalid("invalid TFTP path/query"));
        }
        let last = Packet::ReadRequest {
            filename: CString::new(filename.as_bytes()).map_err(io::Error::other)?,
            mode: RequestMode::Octet,
        }
        .to_bytes();
        socket.send_to(&last, server)?;
        Ok(Self {
            socket,
            server,
            peer: None,
            last,
            expected: 1,
            buffer: Vec::new(),
            offset: 0,
            finished: false,
            policy,
        })
    }
    fn block(&mut self) -> io::Result<()> {
        let mut retries = 0;
        let mut interval = Duration::from_millis(250);
        loop {
            self.socket
                .set_read_timeout(Some(self.policy.remaining()?.min(interval)))?;
            let mut bytes = [0; 517];
            let (length, from) = match self.socket.recv_from(&mut bytes) {
                Ok(value) => value,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    if retries >= 3 {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "TFTP retry limit exceeded",
                        ));
                    }
                    retries += 1;
                    interval = (interval * 2).min(Duration::from_secs(2));
                    self.socket
                        .send_to(&self.last, self.peer.unwrap_or(self.server))?;
                    continue;
                }
                Err(error) => return Err(error),
            };
            if from.ip() != self.server.ip() || self.peer.is_some_and(|peer| peer != from) {
                continue;
            }
            if length > 516 {
                return Err(invalid("oversized TFTP packet"));
            }
            let packet = Packet::from_bytes(&bytes[..length]).map_err(io::Error::other)?;
            match packet {
                Packet::Data { block_n, data } if block_n == self.expected => {
                    self.peer = Some(from);
                    self.last = Packet::Acknowledgment { block_n }.to_bytes();
                    self.socket.send_to(&self.last, from)?;
                    self.expected = self.expected.wrapping_add(1);
                    self.finished = data.len() < 512;
                    self.buffer = data;
                    self.offset = 0;
                    return Ok(());
                }
                Packet::Data { block_n, .. }
                    if self.peer.is_some() && block_n == self.expected.wrapping_sub(1) =>
                {
                    // Re-ack duplicates, never append them a second time.
                    self.socket.send_to(&self.last, from)?;
                }
                Packet::Data { .. } => {}
                Packet::Error { code, msg } => {
                    return Err(io::Error::other(format!(
                        "TFTP {code:?}: {}",
                        msg.to_string_lossy()
                    )))
                }
                _ => return Err(invalid("unexpected TFTP response")),
            }
        }
    }
}
impl Read for Tftp {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.offset == self.buffer.len() {
            if self.finished {
                return Ok(0);
            }
            self.block()?;
        }
        let length = output.len().min(self.buffer.len() - self.offset);
        output[..length].copy_from_slice(&self.buffer[self.offset..self.offset + length]);
        self.offset += length;
        Ok(length)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reject_unsafe_transports() {
        for value in [
            "file:///etc/passwd",
            "ftp://server/file",
            "http://user:password@server/file",
            "http://server/file#fragment",
        ] {
            assert!(validate_url(&Url::parse(value).unwrap()).is_err());
        }
    }
    #[test]
    fn tftp_duplicate_and_wrong_transfer_id_do_not_corrupt_data() {
        let listener = UdpSocket::bind("127.0.0.1:0").unwrap();
        listener
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut request = [0; 2048];
            let (_, peer) = listener.recv_from(&mut request).unwrap();
            let first = Packet::Data {
                block_n: 1,
                data: vec![b'a'; 512],
            }
            .to_bytes();
            listener.send_to(&first, peer).unwrap();
            listener.recv_from(&mut request).unwrap();
            let rogue = UdpSocket::bind("127.0.0.1:0").unwrap();
            rogue
                .send_to(
                    &Packet::Data {
                        block_n: 2,
                        data: b"wrong".to_vec(),
                    }
                    .to_bytes(),
                    peer,
                )
                .unwrap();
            listener.send_to(&first, peer).unwrap();
            listener.recv_from(&mut request).unwrap();
            listener
                .send_to(
                    &Packet::Data {
                        block_n: 2,
                        data: b"end".to_vec(),
                    }
                    .to_bytes(),
                    peer,
                )
                .unwrap();
            listener.recv_from(&mut request).unwrap();
        });
        let url = Url::parse(&format!("tftp://{address}/test")).unwrap();
        let data = bytes(&url, &Policy::default(), 2048).unwrap();
        assert_eq!(&data[..512], &[b'a'; 512]);
        assert_eq!(&data[512..], b"end");
        server.join().unwrap();
    }
}
