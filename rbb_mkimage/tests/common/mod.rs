// Structural ELF64 fixture with a target inventory; never executed as a guest.
pub fn elf() -> Vec<u8> {
    let names = b"\0.shstrtab\0.rbb.commands\0";
    let inventory = serde_json::to_vec(&rbb_manifest::Manifest::current(
        "x86_64-unknown-linux-musl",
    ))
    .unwrap();
    let mut data = vec![0; 312 + names.len() + inventory.len()];
    data[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    data[16] = 2;
    data[18] = 62;
    data[20] = 1;
    data[32] = 64;
    data[40] = 120;
    data[52] = 64;
    data[54] = 56;
    data[56] = 1;
    data[58] = 64;
    data[60] = 3;
    data[62] = 1;
    data[64] = 1;
    data[184] = 1;
    data[188] = 3;
    data[208..216].copy_from_slice(&312u64.to_le_bytes());
    data[216..224].copy_from_slice(&(names.len() as u64).to_le_bytes());
    data[248] = 11;
    data[252] = 1;
    data[256] = 2;
    data[272..280].copy_from_slice(&((312 + names.len()) as u64).to_le_bytes());
    data[280..288].copy_from_slice(&(inventory.len() as u64).to_le_bytes());
    data[312..312 + names.len()].copy_from_slice(names);
    data[312 + names.len()..].copy_from_slice(&inventory);
    data
}
