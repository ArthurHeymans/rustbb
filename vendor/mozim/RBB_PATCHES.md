# rbb patch set to mozim 0.3.3 (Apache-2.0)

Source: https://crates.io/api/v1/crates/mozim/0.3.3/download
Original crate SHA-256:
`078aec06f40dd603e750d96b20f9bf90a34547ea472d738ca2819a3880faa783`.
Only Cargo.toml, README, LICENSE, src/ and examples/ are included; upstream
RFC documentation, CI files and the upstream application lockfile are omitted.
Original source notices and license are retained. No code is relicensed.

Why a local patch is necessary for PXE boot:

- Upstream parses BOOTP `file` and `sname` but drops them from the public lease,
  losing boot metadata used by ordinary DHCP servers, including QEMU's server.
- It exposes requested option codes, but no way to send client architecture (93)
  and vendor class (60), so an arm64 client cannot identify itself correctly.
- BOOTP overloaded fields can contain binary options, so treating them as UTF-8
  strings before interpreting option 52 rejects otherwise valid packets.

Boot-metadata changes are limited to DHCPv4 config, message, option and lease modules:
expose the two boot fields, add bounded untyped client options without allowing
protocol-owned options to be overridden, and interpret overloaded file/sname
fields in RFC order. The existing state machine, socket transport, validation,
lease timers and DHCPv6 implementation are reused. Added host tests check wire
architecture options, boot-field exposure and overload extraction.

The richer userland also upgrades Tokio to 1.54, where the old AsyncFd::new
constructor is deprecated for I/O-safety reasons. The timer and raw-v4-socket
registration sites now use AsyncFd::register with explicit safety arguments:
each private AsyncFd owns its descriptor and never replaces/closes it through
an alias. The fork's minimum Tokio/Rust versions are updated accordingly. No
socket/state-machine behavior is otherwise changed.

The dependency is excluded as a root workspace member; it is still built as a
normal path dependency. Keep changes small and replace the local patch with an
upstream release once equivalent APIs are available. No upstream PR has been
filed by this work.
