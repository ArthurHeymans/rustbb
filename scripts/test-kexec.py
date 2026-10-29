#!/usr/bin/env python3
"""Verify local boot formats and controlled handoff reach a second Linux kernel."""
import argparse
import hashlib
import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("disk_tests", ROOT / "scripts/test-disk-root.py")
DISK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(DISK)
QEMU = DISK.QEMU
run = QEMU.run


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--arch", choices=QEMU.ARCHITECTURES, default="x86_64")
    parser.add_argument("--kernel", type=Path)
    parser.add_argument("--mkfs", default=os.environ.get("MKFS_EXT4", "mkfs.ext4"))
    parser.add_argument("--timeout", type=int, default=40)
    args = parser.parse_args()
    if not shutil.which(args.mkfs):
        parser.error("mkfs.ext4 required for disposable disk image")
    args.qemu = QEMU.ARCHITECTURES[args.arch]["qemu"]
    arch = QEMU.ARCHITECTURES[args.arch]["uname"]
    mkimage, binary, guest, target = DISK.build(args.arch)
    results = target / "kexec-tests" / args.arch
    results.mkdir(parents=True, exist_ok=True)
    kernel = (args.kernel or target / "test-kernels" / f"rbb-{args.arch}" / ("arch/arm64/boot/Image" if args.arch == "arm64" else "arch/x86/boot/bzImage")).resolve()
    if args.arch == "arm64":
        kernel = QEMU.prepare_arm64_kernel(kernel, results)
    console = QEMU.ARCHITECTURES[args.arch]["console"]
    cmdline = f"console={console} rdinit=/init panic=-1 quiet loglevel=4 rbb.stage=second"
    second_config = results / "second.toml"
    second_config.write_text('''[[services]]
name = "second-kernel-proof"
kind = "oneshot"
command = ["/bin/guest-test", "kernel-stage"]
''')
    second = results / "second.cpio.gz"
    run([mkimage, "--busybox", binary, "--file", f"{guest}:/bin/guest-test", "--file", f"{second_config}:/etc/rbb/init.toml", "-o", second])
    early = results / "early.cpio"
    # An earlier archive has a different config. Concatenated initrd order must
    # allow the actual second-stage config to replace it.
    run([mkimage, "--busybox", binary, "--compress", "none", "-o", early])
    root = results / "disk-files"
    if root.exists():
        shutil.rmtree(root)
    for directory in ["loader/entries", "grub", "extlinux"]:
        (root / directory).mkdir(parents=True, exist_ok=True)
    for source, dest in [(kernel, "kernel"), (early, "early.cpio"), (second, "second.cpio.gz")]:
        shutil.copy2(source, root / dest)
    (root / "loader/entries/stage2.conf").write_text(f"title BLS stage two\narchitecture {'aa64' if args.arch == 'arm64' else 'x64'}\nlinux /kernel\ninitrd /early.cpio\ninitrd /second.cpio.gz\noptions {cmdline}\n")
    (root / "grub/grub.cfg").write_text(f"menuentry 'GRUB stage two' --id grub-stage2 {{\nlinux /kernel {cmdline}\ninitrd /early.cpio /second.cpio.gz\n}}\n")
    (root / "extlinux/extlinux.conf").write_text(f"LABEL stage2\nMENU LABEL extlinux stage two\nLINUX /kernel\nINITRD /early.cpio,/second.cpio.gz\nAPPEND {cmdline}\n")
    disk = results / "boot.ext4"
    with disk.open("wb") as output:
        output.truncate(64 * 1024 * 1024)
    run([args.mkfs, "-F", "-q", "-d", root, disk])
    manifest = results / "boot.toml"
    manifest.write_text(f'''version = 1
[[entries]]
id = "native-stage2"
title = "verified native stage two"
architecture = "{arch}"
cmdline = "{cmdline}"
kernel = {{ source = "/boot/kernel", sha256 = "{digest(kernel)}" }}
initrd = [{{ source = "/boot/early.cpio", sha256 = "{digest(early)}" }}, {{ source = "/boot/second.cpio.gz", sha256 = "{digest(second)}" }}]
''')
    bad_manifest = results / "bad.toml"
    bad_manifest.write_text(manifest.read_text().replace(digest(kernel), "0" * 64))
    cases = [("bls", "vda-stage2"), ("grub", "vda-grub-stage2"), ("extlinux", "vda-extlinux-stage2"), ("native", "native-stage2"), ("controlled", "native-stage2"), ("bad-hash", "native-stage2")]
    for name, entry in cases:
        config = results / f"{name}.toml"
        boot = f'''[boot]
source = "{'disk' if name in ['bls', 'grub', 'extlinux'] else '/etc/rbb/boot.toml'}"
entry = "{entry}"
require_hashes = {'false' if name in ['bls', 'grub', 'extlinux'] else 'true'}
'''
        if name == "controlled":
            config.write_text('''shutdown_timeout_ms = 200
[[services]]
name = "stubborn"
command = ["/bin/guest-test", "stubborn"]
[[services]]
name = "boot"
kind = "oneshot"
command = ["/bin/guest-test", "controlled-kexec"]
after = ["stubborn"]
''')
        else:
            config.write_text(boot + '''[[services]]
name = "fallback"
kind = "oneshot"
command = ["/bin/guest-test", "boot-fallback"]
''')
        image = results / f"{name}.cpio.gz"
        command = [mkimage, "--busybox", binary, "--file", f"{guest}:/bin/guest-test", "--file", f"{config}:/etc/rbb/init.toml"]
        if name in ["native", "controlled", "bad-hash"]:
            for source, dest in [(kernel, "/boot/kernel"), (early, "/boot/early.cpio"), (second, "/boot/second.cpio.gz"), (bad_manifest if name == "bad-hash" else manifest, "/etc/rbb/boot.toml")]:
                command += ["--file", f"{source}:{dest}"]
        run([*command, "-o", image])
        command = QEMU.qemu_command(args, kernel, image)
        command[command.index("-append") + 1] += " rbb.stage=first" + (" rbb.test.no_kexec" if name == "bad-hash" else "")
        command += ["-drive", f"file={disk},format=raw,if=none,id=bootdisk", "-device", "virtio-blk-device,drive=bootdisk" if args.arch == "arm64" else "virtio-blk-pci,drive=bootdisk"]
        print("+", " ".join(command), flush=True)
        try:
            output = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=args.timeout)
            log = output.stdout.decode(errors="replace")
        except subprocess.TimeoutExpired as error:
            log = (error.stdout or b"").decode(errors="replace")
            (results / f"{name}.log").write_text(log)
            print(log)
            raise RuntimeError(f"{name} timed out") from error
        (results / f"{name}.log").write_text(log)
        print(log, flush=True)
        assert output.returncode == 0 and "Kernel panic" not in log
        assert "RBB_GUEST_TEST_FAIL" not in log
        if name == "bad-hash":
            assert "SHA-256 mismatch" in log and "RBB_BOOT_FALLBACK_PASS" in log
            assert "RBB_KEXEC_SECOND_KERNEL_PASS" not in log and "for kexec" not in log
        else:
            loaded_marker = "RBB_CONTROLLED_KEXEC_REQUEST" if name == "controlled" else "for kexec"
            assert loaded_marker in log and f"RBB_KEXEC_SECOND_KERNEL_PASS arch={arch}" in log
            assert "RBB_BOOT_FALLBACK_PASS" not in log
        if name == "controlled":
            assert "RBB_CONTROLLED_KEXEC_REQUEST" in log
            assert "shutdown deadline; sent SIGKILL" in log
            assert log.index("shutdown complete") < log.index("RBB_KEXEC_SECOND_KERNEL_PASS")
        print(f"KEXEC_CASE_PASS arch={args.arch} case={name}", flush=True)
    print(f"RBB_KEXEC_END_TO_END_PASS arch={args.arch}", flush=True)


if __name__ == "__main__":
    try:
        main()
    except (AssertionError, OSError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        sys.exit(1)
