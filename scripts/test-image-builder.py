#!/usr/bin/env python3
"""Boot merged raw/CRC/gzip bases, presets, preserved init and generated uinit services."""
import argparse
import gzip
import importlib.util
from pathlib import Path
import secrets
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("disk", ROOT / "scripts/test-disk-root.py")
DISK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(DISK)
QEMU = DISK.QEMU


def archive(records):
    # Independent fixture producer, deliberately not the production writer.
    output = bytearray()
    for ino, (name, mode, data, uid, gid, device, links, identity) in enumerate(records + [("TRAILER!!!", 0, b"", 0, 0, (0, 0), 1, None)], 1):
        encoded = name.encode() + b"\0"
        fields = [identity or ino, mode, uid, gid, links, 12345, len(data), 0, 0, *device, len(encoded), sum(data) & 0xffffffff]
        output += b"070702" + "".join(f"{value:08x}" for value in fields).encode() + encoded
        output += bytes(-len(output) % 4)
        output += data
        output += bytes(-len(output) % 4)
    output += bytes(-len(output) % 512)
    return bytes(output)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--arch", choices=QEMU.ARCHITECTURES, default="x86_64")
    parser.add_argument("--kernel", type=Path)
    parser.add_argument("--timeout", type=int, default=60)
    args = parser.parse_args()
    args.qemu = QEMU.ARCHITECTURES[args.arch]["qemu"]
    mkimage, binary, guest, target = DISK.build(args.arch)
    results = target / "image-builder-tests" / args.arch
    results.mkdir(parents=True, exist_ok=True)
    kernel = args.kernel or target / "test-kernels" / f"rbb-{args.arch}" / ("arch/arm64/boot/Image" if args.arch == "arm64" else "arch/x86/boot/bzImage")
    if args.arch == "arm64":
        kernel = QEMU.prepare_arm64_kernel(kernel, results)
    for index, preset in enumerate(["minimal", "rescue", "diskboot", "netboot"]):
        proof = secrets.token_hex(16)
        records = [
            (".", 0o040755, b"", 0, 0, (0, 0), 2, None),
            ("init", 0o100755, b"#!/bin/sh\n/bin/guest-test image-uinit\n", 0, 0, (0, 0), 1, None),
            ("base-marker", 0o100644, proof.encode(), 0, 0, (0, 0), 1, None),
            ("base-owned", 0o100640, b"owned", 123, 456, (0, 0), 1, None),
            ("base-fifo", 0o010600, b"", 0, 0, (0, 0), 1, None),
            ("base-device", 0o020600, b"", 0, 0, (1, 3), 1, None),
            ("base-hard-one", 0o100644, b"", 0, 0, (0, 0), 2, 500),
            ("base-hard-two", 0o100644, b"shared", 0, 0, (0, 0), 2, 500),
            ("base-link", 0o120777, b"base-hard-one", 0, 0, (0, 0), 1, None),
        ]
        base = results / f"{preset}.base"
        data = archive(records)
        base.write_bytes(gzip.compress(data, mtime=0) if index % 2 else data)
        shell = results / "image-check"
        shell.write_text("#!/bin/sh\n/bin/guest-test image-check\n")
        shell.chmod(0o755)
        expected = results / "preset"
        expected.write_text(preset)
        image = results / f"{preset}.cpio.gz"
        command = [mkimage, "--busybox", binary, "--preset", preset, "--base", base,
                   "--preserve-init", "--uinit", "/inito", "--shell", "/bin/image-check",
                   "--file", f"{guest}:/bin/guest-test", "--file", f"{shell}:/bin/image-check",
                   "--file", f"{expected}:/expected-preset"]
        if preset == "netboot":
            second = results / "second.base"
            second.write_bytes(archive([("extra-marker", 0o100644, b"second base", 0, 0, (0, 0), 1, None)]))
            command += ["--base", second]
        QEMU.run([*command, "-o", image])
        repeated = results / "repeated.gz"
        QEMU.run([*command, "-o", repeated])
        assert image.read_bytes() == repeated.read_bytes(), "merged image is not reproducible"
        launch = QEMU.qemu_command(args, kernel, image)
        launch[launch.index("-append") + 1] += f" rbb.image_proof={proof}"
        result = subprocess.run(launch, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=args.timeout)
        log = result.stdout.decode(errors="replace")
        (results / f"{preset}.log").write_text(log)
        print(log, flush=True)
        assert result.returncode == 0 and "RBB_GUEST_TEST_FAIL" not in log and "Kernel panic" not in log
        assert "RBB_IMAGE_UINIT_PASS" in log
        assert f"RBB_IMAGE_PASS arch={QEMU.ARCHITECTURES[args.arch]['uname']} preset={preset}" in log
    print(f"RBB_IMAGE_BUILDER_END_TO_END_PASS arch={args.arch}", flush=True)


if __name__ == "__main__":
    try:
        main()
    except (AssertionError, OSError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        sys.exit(1)
