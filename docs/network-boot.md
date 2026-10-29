# Network boot

The network boot path reuses the same staging, integrity and PID 1 execution
machinery as [local kernel boot](kernel-boot.md). It configures a real interface,
obtains a DHCPv4 lease or applies static addresses, fetches a boot configuration
and kernel/initrd over the network, then reaches a second kernel through kexec.

```toml
[network]
# interface = "eth0"       # otherwise first Ethernet interface, sorted by name
# dhcp defaults to true; this is an early-boot one-shot lease, not a lease daemon
# timeout_ms = 15000

[boot]
source = "dhcp"            # BOOTP filename / DHCP boot options
# or source = "https://boot.example/manifest.toml"
# manifest_key = "/etc/rbb/boot-public-key"
# tls_ca = "/etc/rbb/boot-ca.pem"

[[services]]
name = "rescue"
command = ["/bin/sh"]
```

An explicit `[network]` can also set `dhcp = false`, `addresses =
["192.0.2.10/24"]`, `gateway = "192.0.2.1"`, and `dns = ["192.0.2.53"]`.
Static IPv6 addresses/routes use the same model, but DHCPv6/SLAAC policy is not
implemented by this first network increment. With `source = "dhcp"` and no
network section, a default DHCPv4 setup is attempted. Explicit HTTP/TFTP sources
need a configured network (or an already-configured kernel network).

## Discovery and formats

- DHCPv4 address, mask, gateway/classless routes and DNS are applied through
  rtnetlink. Route option 121 takes precedence over the ordinary router option.
- BOOTP `file`, `sname` and `siaddr`, options 66/67 and option-overload fields are
  retained. A full HTTP/HTTPS/TFTP boot URL is used directly; a plain filename is
  resolved against the DHCP boot server using TFTP.
- Client architecture option 93 is 9 for x86_64 and 11 for arm64, not BIOS PC 0.
  A PXE-compatible vendor-class string is sent; this remains a Linux userspace
  client, not an EFI/UNDI firmware implementation.
- Native TOML/JSON manifests, bounded boot-oriented iPXE scripts, PXELINUX config
  discovery and direct raw Linux kernel URLs are recognized.
- PXELINUX probes MAC, decreasing IPv4-hex prefixes, then `default`, relative to
  the bootfile directory. LABEL/KERNEL/INITRD/APPEND and DEFAULT are supported.
- iPXE supports `set`, `${variable}`, `kernel`, `initrd`, `imgargs`, `boot`, and
  bounded `chain`; it is not a full iPXE interpreter. Conditional/goto/menu/shell
  syntax and arbitrary command execution are not supported. Chained scripts
  should establish their own variables; full iPXE environment semantics are not
  claimed. PXELINUX INCLUDE/CONFIG are also not yet supported.
- URL schemes are restricted to HTTP/HTTPS/TFTP. Network-provided paths cannot
  turn into reads of host-local `file://` paths. URL credentials and fragments
  are rejected. Inputs, script expansion, recursion, downloads and timeouts are
  bounded. Large kernel/initrd data is streamed, not buffered in an unbounded Vec.

## Reused crates and deliberate gaps

`rtnetlink` handles Linux networking; `mozim` handles DHCP's state machine and raw
socket transport. The small, Apache-2.0 [local mozim patch](../vendor/mozim/RBB_PATCHES.md)
exposes BOOTP boot fields, adds client boot options, and supports overloaded
fields. Upstream 0.3.3 otherwise discards or cannot send this boot metadata. The
source is vendored with its license and provenance; no upstream PR was filed.

HTTP/TLS uses `ureq` and `rustls` with the ring crypto provider, never OpenSSL in
the guest. Unlike the original minimal toolkit, building this crypto backend
requires a suitable target C/assembly compiler. Cross-building arm64 must provide
`CC_aarch64_unknown_linux_musl` and, if necessary, `AR_aarch64_unknown_linux_musl`.
Rust's bundled linker remains usable for the final static musl binary.

TFTP reuses `tftp_client`'s packet codec, **not its downloader**: that downloader
buffers the whole file and does not enforce the transfer-ID/block checks needed
here. Our bounded reader locks the server transfer ID, ignores duplicate data,
re-acks duplicate blocks, supports block-number wrap and enforces retry/deadline
limits. It uses RFC1350 512-byte blocks; option/blocksize negotiation is future
work. The early-boot DHCP client is not a replacement for a long-running network
manager after real-root handoff.

## Integrity and TLS

`require_hashes = true` requires a native manifest binding a SHA-256 digest for
every artifact. Digests alone do not authenticate an untrusted manifest. Set
`manifest_key` to a local, trusted Ed25519 public-key file (64 hex digits) to
require a signature over the **exact raw manifest bytes** and artifact digests.
The default signature source is the manifest URL/path with `.sig` appended; an
explicit `signature` source can override that. Signatures are 128 hex digits.
Signature-required boot never falls back to unsigned PXE/iPXE configurations.

A host helper signs manifests without passing private values on the command line:

```sh
cargo run -p rbb-mkimage --bin rbb-sign -- \
  --key private-seed.hex --manifest manifest.toml \
  --signature manifest.toml.sig --public-key boot-public-key
```

The seed is 32 bytes encoded as 64 hex digits in a mode-0600 file. Never include
it in an image or web root. Embed only the public key. Production signing-key
management/HSM support is outside this helper's scope.

Relative metadata/artifact paths and default signature URLs use the effective
manifest URL after redirects, not the original request directory. Redirected
HTTP and signed-HTTPS manifests are exercised with payloads only available under
the final directory.

HTTPS always verifies certificates. `tls_ca` supplies a deliberate CA bundle;
otherwise WebPKI roots are used. HTTPS redirects may not downgrade to HTTP, and
there is no insecure verification fallback. A working clock is required; test
kernels enable the emulated RTC and initialize time from it. Real deployments
need an RTC or an authenticated time policy. DHCP and TFTP are unauthenticated;
unsigned netboot explicitly trusts the server/network and emits a warning.

## Commands

- `ip link [show|set IF up|down]`
- `ip addr [show|add CIDR dev IF]`
- `ip route [show|add PREFIX via IP dev IF]`
- `dhclient [IF]` — one-shot lease/configuration, saves `/run/rbb/network.toml`
- `pxeboot [--interface IF] [--dhcp-timeout-ms MS] [boot options]`
- `boot --source URL [--manifest-key KEY] [--tls-ca CA] [--no-load|--no-exec]`
- `wget -O OUTPUT URL` — bounded, atomically installed network download

## End-to-end tests

```sh
python3 scripts/test-netboot.py --arch x86_64
python3 scripts/test-netboot.py --arch arm64
```

Use the dedicated kernels or `--kernel`; the host needs OpenSSL **only for
creating disposable TLS test certificates**, plus both QEMU emulators and Rust
cross-build tools. HTTP(S) fixtures bind host loopback; TFTP/DHCP use isolated
QEMU user networking. A DHCP-timeout case uses an isolated hub with no server.
No TAP interfaces, root host privileges, public boot services or host disks are
used. Test signing seeds and TLS private keys stay outside the served directory.

Cases cover DHCP/iPXE/HTTP, PXELINUX/TFTP, native HTTP, signed HTTPS, tampered
signature, tampered artifact, untrusted TLS, missing download, DHCP timeout and
static IPv4, redirected HTTP/signed HTTPS and blocked HTTPS-to-HTTP downgrade
(13 cases per architecture). Success must reach a second-kernel command line containing a fresh
server-provided nonce, not merely download/parse a config or return to rescue.
Negative cases must reach rescue without loading a kernel. Both architectures
run the same suite; results are in `target/netboot-tests/ARCH/`.

Still pending from the broader roadmap: DHCPv6/SLAAC policy and IPv6 E2E coverage,
more complete boot-script/config subsets, and the remaining builder, userland and
service-management increments. This is not a claim of complete u-root parity.
