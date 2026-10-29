use crate::{invalid, validate_static_elf};
use object::{Object, ObjectSection};
use rbb_manifest::Manifest;
use std::io;

/// Read target metadata, never execute a foreign-architecture executable.
pub fn command_manifest(binary: &[u8]) -> io::Result<Manifest> {
    validate_static_elf(binary)?;
    let elf = object::File::parse(binary).map_err(io::Error::other)?;
    let section = elf
        .section_by_name(rbb_manifest::SECTION)
        .ok_or_else(|| invalid("rbb has no command manifest; rebuild rbb-cmds"))?;
    let manifest = Manifest::parse(section.data().map_err(io::Error::other)?)?;
    let machine = u16::from_le_bytes(binary[18..20].try_into().unwrap());
    let architecture = manifest.target.split('-').next().unwrap_or_default();
    if !matches!(
        (architecture, machine),
        ("x86_64", 62) | ("aarch64", 183) | ("riscv64", 243)
    ) {
        return Err(invalid(
            "command manifest target differs from ELF architecture",
        ));
    }
    Ok(manifest)
}
