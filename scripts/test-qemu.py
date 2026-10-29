#!/usr/bin/env python3
"""Build, independently inspect, and boot the Rust initramfs (no root/KVM needed)."""
import argparse
import gzip
import hashlib
import os
from pathlib import Path
import selectors
import shutil
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
ARCHITECTURES = {
    "x86_64": {
        "target": "x86_64-unknown-linux-musl",
        "qemu": "qemu-system-x86_64",
        "machine": "pc,accel=tcg",
        "cpu": "max",
        "console": "ttyS0",
        "uname": "x86_64",
        "elf_machine": 62,
    },
    "arm64": {
        "target": "aarch64-unknown-linux-musl",
        "qemu": "qemu-system-aarch64",
        "machine": "virt,accel=tcg",
        "cpu": "cortex-a72",
        "console": "ttyAMA0",
        "uname": "aarch64",
        "elf_machine": 183,
    },
}


def run(command, **kwargs):
    print("+", " ".join(map(str, command)), flush=True)
    return subprocess.run(list(map(str, command)), cwd=ROOT, check=True, **kwargs)


def build_environment(arch):
    """Use Rust's linker plus a target C/asm compiler for the ring TLS backend."""
    env = os.environ.copy()
    target = ARCHITECTURES[arch]["target"]
    sysroot = subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip()
    version = subprocess.check_output(["rustc", "-vV"], text=True)
    host = next(line.removeprefix("host: ") for line in version.splitlines() if line.startswith("host: "))
    linker = Path(sysroot) / "lib/rustlib" / host / "bin/rust-lld"
    if linker.is_file():
        env.setdefault(f"CARGO_TARGET_{target.upper().replace('-', '_')}_LINKER", str(linker))
    cc_key = f"CC_{target.replace('-', '_')}"
    if cc_key not in env and "CC" not in env:
        candidates = [f"{ARCHITECTURES[arch]['uname']}-linux-musl-gcc", f"{ARCHITECTURES[arch]['uname']}-unknown-linux-musl-gcc"]
        if host.split('-')[0] == ARCHITECTURES[arch]["uname"] and "linux" in host:
            candidates += ["musl-gcc", "clang", "cc"]
        else:
            candidates += [f"{ARCHITECTURES[arch]['uname']}-linux-gnu-gcc", f"{ARCHITECTURES[arch]['uname']}-unknown-linux-gnu-gcc"]
        compiler = next((candidate for candidate in candidates if shutil.which(candidate)), None)
        if compiler is None:
            raise RuntimeError(f"TLS backend requires a target C/asm compiler; set {cc_key}")
        env[cc_key] = compiler
    if shutil.which("llvm-ar"):
        env.setdefault(f"AR_{target.replace('-', '_')}", "llvm-ar")
    return env


def qemu_command(args, kernel, image):
    architecture = ARCHITECTURES[args.arch]
    return list(map(str, [args.qemu, "-machine", architecture["machine"], "-cpu", architecture["cpu"], "-m", "256M", "-smp", "2",
                         "-nodefaults", "-display", "none", "-serial", "stdio", "-monitor", "none", "-nic", "none",
                         "-no-reboot", "-kernel", kernel, "-initrd", image,
                         "-append", f"console={architecture['console']} rdinit=/init panic=-1 quiet loglevel=4"]))


def prepare_arm64_kernel(kernel, results):
    """Unwrap an EFI gzip zboot image for QEMU's direct Linux boot protocol."""
    data = kernel.read_bytes()
    if data[4:8] != b"zimg":
        return kernel
    if len(data) < 64 or data[24:28] != b"gzip":
        raise RuntimeError("only gzip-compressed EFI zboot kernels are supported")
    offset = int.from_bytes(data[8:12], "little")
    size = int.from_bytes(data[12:16], "little")
    if offset < 64 or size == 0 or offset + size > len(data):
        raise RuntimeError("invalid EFI zboot payload bounds")
    image = gzip.decompress(data[offset:offset + size])
    if len(image) < 64 or image[56:60] != b"ARMd":
        raise RuntimeError("EFI zboot payload is not an arm64 Linux Image")
    output = results / "Image"
    output.write_bytes(image)
    print(f"extracted arm64 Image sha256={hashlib.sha256(image).hexdigest()}", flush=True)
    return output


def test_console(args, kernel, image, logfile):
    command = qemu_command(args, kernel, image)
    print("+", " ".join(command), flush=True)
    log = bytearray()
    commands = [b"echo RBB_DEFAULT_CONSOLE_PASS\n", b"uname -a\n", b"ctl status\n", b"poweroff\n"]
    sent = 0
    with subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT) as process:
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(process.stdout, selectors.EVENT_READ)
                deadline = time.monotonic() + args.timeout
                while time.monotonic() < deadline:
                    if not selector.select(timeout=0.2):
                        continue
                    chunk = process.stdout.read1(4096)
                    if not chunk:
                        break
                    log.extend(chunk)
                    if sent < len(commands) and log.count(b"rbb# ") > sent:
                        process.stdin.write(commands[sent])
                        process.stdin.flush()
                        sent += 1
                else:
                    raise RuntimeError("default console QEMU timed out")
            assert process.wait(timeout=5) == 0, "default console QEMU failed"
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
            text = log.decode(errors="replace")
            logfile.write_text(text)
            print(text, flush=True)
    assert sent == len(commands), "console did not accept commands"
    assert "RBB_DEFAULT_CONSOLE_PASS" in text.splitlines(), "echo command did not execute"
    assert "console state=up" in text and "hello state=complete" in text
    assert any(line.startswith("Linux ") and line.endswith(ARCHITECTURES[args.arch]["uname"]) for line in text.splitlines())
    assert "rbb-init: shutdown complete" in text
    assert "Kernel panic" not in text
    print(f"QEMU default console PASS; log: {logfile}", flush=True)


def inspect_archive(path, compressed, architecture):
    data = path.read_bytes()
    if compressed:
        data = gzip.decompress(data)
    offset = 0
    entries = {}
    while True:
        assert data[offset:offset + 6] == b"070701", f"bad newc magic at {offset}"
        fields = [int(data[offset + 6 + i * 8:offset + 14 + i * 8], 16) for i in range(13)]
        mode, uid, gid, mtime, size, namesize = fields[1], fields[2], fields[3], fields[5], fields[6], fields[11]
        assert (uid, gid, mtime) == (0, 0, 0), "non-reproducible metadata"
        start = offset + 110
        name = data[start:start + namesize - 1].decode()
        assert data[start + namesize - 1] == 0
        start = (start + namesize + 3) & ~3
        payload = data[start:start + size]
        offset = (start + size + 3) & ~3
        if name == "TRAILER!!!":
            break
        assert name not in entries
        assert not name.startswith("/") and ".." not in name.split("/")
        entries[name] = (mode, payload)
    assert entries["init"] == (0o120777, b"bbin/init")
    assert entries["bin/echo"] == (0o120777, b"../bbin/echo")
    binary = entries["bbin/rbb"][1]
    assert binary.startswith(b"\x7fELF\x02\x01")
    assert int.from_bytes(binary[18:20], "little") == architecture["elf_machine"], "wrong guest ELF architecture"
    assert entries["dev/console"][0] == 0o020600
    assert "etc/rbb/init.toml" in entries
    print(f"inspected {len(entries)} newc entries; sha256={hashlib.sha256(path.read_bytes()).hexdigest()}", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--arch", choices=ARCHITECTURES, default="x86_64")
    parser.add_argument("--kernel", default=os.environ.get("RBB_KERNEL"))
    parser.add_argument("--qemu", default=os.environ.get("QEMU"))
    parser.add_argument("--timeout", type=int, default=60)
    args = parser.parse_args()
    architecture = ARCHITECTURES[args.arch]
    target = architecture["target"]
    if args.kernel is None:
        if args.arch != "x86_64":
            parser.error("arm64 requires --kernel /path/to/arm64/Image (or RBB_KERNEL)")
        args.kernel = "/run/current-system/kernel"
    args.qemu = args.qemu or architecture["qemu"]
    kernel = Path(args.kernel).resolve()
    if not kernel.is_file():
        parser.error(f"kernel not found: {kernel}; supply --kernel (or RBB_KERNEL)")
    print(f"testing {args.arch}; kernel sha256={hashlib.sha256(kernel.read_bytes()).hexdigest()}", flush=True)
    if not shutil.which(args.qemu):
        parser.error("qemu-system-x86_64 not found")
    env = build_environment(args.arch)
    run(["cargo", "build", "-p", "rbb-mkimage"], env=env)
    run(["cargo", "build", "--release", "-p", "rbb-cmds", "--bins", "--target", target], env=env)
    target_dir = Path(env.get("CARGO_TARGET_DIR", ROOT / "target"))
    if not target_dir.is_absolute():
        target_dir = ROOT / target_dir
    mkimage = target_dir / "debug/rbb-mkimage"
    binary = target_dir / target / "release/rbb"
    guest = target_dir / target / "release/rbb-guest-test"
    results = target_dir / "qemu-tests" / args.arch
    results.mkdir(parents=True, exist_ok=True)
    if args.arch == "arm64":
        kernel = prepare_arm64_kernel(kernel, results)
    for action, compression in [("poweroff", "gzip"), ("reboot", "none")]:
        action_file = results / "action"
        action_file.write_text(action)
        image = results / f"{action}.cpio{'.gz' if compression == 'gzip' else ''}"
        command = [mkimage, "--busybox", binary, "--file", f"{guest}:/bin/guest-test",
                   "--file", f"{ROOT / 'tests/qemu/init.toml'}:/etc/rbb/init.toml",
                   "--file", f"{action_file}:/etc/rbb/test-action", "--compress", compression]
        run([*command, "-o", image])
        second = results / "repeated.cpio"
        run([*command, "-o", second])
        assert image.read_bytes() == second.read_bytes(), "image is not reproducible"
        second.unlink()
        inspect_archive(image, compression == "gzip", architecture)
        # cpio is optional; this independently checks our writer with GNU cpio.
        if shutil.which("cpio"):
            data = image.read_bytes()
            listing = run(["cpio", "-it", "--quiet"], input=gzip.decompress(data) if compression == "gzip" else data, capture_output=True)
            assert b"bbin/rbb\n" in listing.stdout
        command = qemu_command(args, kernel, image)
        print("+", " ".join(map(str, command)), flush=True)
        try:
            output = subprocess.run(list(map(str, command)), stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=args.timeout)
            log = output.stdout.decode(errors="replace")
        except subprocess.TimeoutExpired as error:
            log = (error.stdout or b"").decode(errors="replace")
            (results / f"{action}.log").write_text(log)
            print(log)
            raise RuntimeError(f"QEMU {action} timed out; log: {results / f'{action}.log'}") from error
        (results / f"{action}.log").write_text(log)
        print(log, flush=True)
        assert output.returncode == 0, f"QEMU exit {output.returncode}"
        assert "RBB_GUEST_TEST_PASS" in log, "guest tests did not pass"
        assert f"RBB_GUEST_ARCH={architecture['uname']}" in log, "guest architecture was not verified"
        assert "RBB_GUEST_TEST_FAIL" not in log, "guest test failure"
        assert "rbb-init: shutdown deadline; sent SIGKILL" in log, "stubborn service was not killed at shutdown"
        assert "rbb-init: shutdown complete" in log, "shutdown did not complete"
        assert "Kernel panic" not in log
        print(f"QEMU {action} PASS; log: {results / f'{action}.log'}", flush=True)
    # Exercise the actual default user-facing image, including --build and its
    # default config, rather than only the purpose-built verification payload.
    image = results / "rescue.cpio.gz"
    run([mkimage, "--build", "--workspace", ROOT, "--target", target, "-o", image], env=env)
    test_console(args, kernel, image, results / "console.log")
    print(f"RBB_QEMU_END_TO_END_PASS arch={args.arch}", flush=True)


if __name__ == "__main__":
    try:
        main()
    except (AssertionError, OSError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        sys.exit(1)
