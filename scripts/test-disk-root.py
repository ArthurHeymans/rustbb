#!/usr/bin/env python3
"""Boot disposable ext4/GPT disks and verify real-root PID 1 and rescue fallback."""
import argparse
import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("qemu", ROOT / "scripts/test-qemu.py")
QEMU = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(QEMU)
UUID = "e26e38ac-8aae-40c2-a52a-99400b5a338e"
PARTUUID = "11111111-2222-3333-4444-555555555555"


def run(command, **kwargs):
    return QEMU.run(command, **kwargs)


def build(arch, features=None):
    env = QEMU.build_environment(arch)
    if features is None:
        features = env.get("RBB_TEST_FEATURES", "")
    target = QEMU.ARCHITECTURES[arch]["target"]
    run(["cargo", "build", "-p", "rbb-mkimage"], env=env)
    command = ["cargo", "build", "--release", "-p", "rbb-cmds", "--bins", "--target", target]
    if features:
        command += ["--features", features]
    run(command, env=env)
    directory = Path(env.get("CARGO_TARGET_DIR", ROOT / "target")).absolute()
    return directory / "debug/rbb-mkimage", directory / target / "release/rbb", directory / target / "release/rbb-guest-test", directory


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--arch", choices=QEMU.ARCHITECTURES, default="x86_64")
    parser.add_argument("--kernel", type=Path)
    parser.add_argument("--mkfs", default=os.environ.get("MKFS_EXT4", "mkfs.ext4"))
    parser.add_argument("--timeout", type=int, default=30)
    args = parser.parse_args()
    if not shutil.which(args.mkfs) or not shutil.which("sfdisk"):
        parser.error("mkfs.ext4 and sfdisk are required (only regular test files will be modified)")
    architecture = QEMU.ARCHITECTURES[args.arch]
    args.qemu = architecture["qemu"]
    mkimage, binary, payload, target = build(args.arch)
    results = target / "disk-root-tests" / args.arch
    results.mkdir(parents=True, exist_ok=True)
    kernel = (args.kernel or target / "test-kernels" / f"rbb-{args.arch}" / ("arch/arm64/boot/Image" if args.arch == "arm64" else "arch/x86/boot/bzImage")).resolve()
    if not kernel.is_file():
        parser.error(f"build a test kernel first or pass --kernel; not found: {kernel}")
    if args.arch == "arm64":
        kernel = QEMU.prepare_arm64_kernel(kernel, results)
    root = results / "root"
    if root.exists():
        try:
            shutil.rmtree(root)
        except OSError as error:
            raise RuntimeError(f"cannot reset disposable root fixture: {root}") from error
    for path in ["bin", "sbin", "etc", "dev", "proc", "sys", "run", "tmp"]:
        (root / path).mkdir(parents=True, exist_ok=True)
    shutil.copy2(payload, root / "bin/guest-test")
    (root / "sbin/init").symlink_to("/bin/guest-test")
    (root / "etc/disk-root-proof").write_text("rbb-disk-root")
    disk = results / "root.ext4"
    with disk.open("wb") as output:
        output.truncate(64 * 1024 * 1024)
    run([args.mkfs, "-F", "-q", "-U", UUID, "-L", "RBBROOT", "-d", root, disk])
    gpt = results / "root.gpt"
    with gpt.open("wb") as output:
        output.truncate(68 * 1024 * 1024)
    run(["sfdisk", gpt], input=f"label: gpt\nunit: sectors\n\nstart=2048,size=131072,type=L,uuid={PARTUUID},name=rbbroot\n", text=True)
    with disk.open("rb") as source, gpt.open("r+b") as dest:
        dest.seek(1024 * 1024)
        shutil.copyfileobj(source, dest)
    cases = [
        ("device", disk, "/dev/vda", False, "/sbin/init"),
        ("uuid", disk, f"UUID={UUID}", False, "/sbin/init"),
        ("label", disk, "LABEL=RBBROOT", False, "/sbin/init"),
        ("partuuid", gpt, f"PARTUUID={PARTUUID}", False, "/sbin/init"),
        ("writable", disk, "/dev/vda", True, "/sbin/init"),
        ("missing-device", disk, "/dev/does-not-exist", False, "/sbin/init"),
        ("missing-init", disk, "/dev/vda", False, "/does-not-exist"),
    ]
    for name, attached, selector, writable, init in cases:
        fallback = name.startswith("missing")
        config = results / f"{name}.toml"
        config.write_text(f'''shutdown_timeout_ms = 200
[root]
device = "/dev/vda"
init = ["{init}", "disk-init"]
wait_ms = 500
[[services]]
name = "fallback"
kind = "oneshot"
command = ["/bin/guest-test", "boot-fallback"]
''')
        image = results / f"{name}.cpio.gz"
        run([mkimage, "--busybox", binary, "--file", f"{payload}:/bin/guest-test", "--file", f"{config}:/etc/rbb/init.toml", "-o", image])
        command = QEMU.qemu_command(args, kernel, image)
        command[command.index("-append") + 1] += f" root={selector} {'rw' if writable else 'ro'}"
        command += ["-drive", f"file={attached},format=raw,if=none,id=rootdisk", "-device",
                    "virtio-blk-device,drive=rootdisk" if args.arch == "arm64" else "virtio-blk-pci,drive=rootdisk"]
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
        marker = "RBB_BOOT_FALLBACK_PASS" if fallback else "RBB_DISK_ROOT_PASS"
        assert f"{marker} arch={architecture['uname']}" in log, f"{name}: missing guest proof"
        if fallback:
            assert "boot preparation failed:" in log
            assert "switching to real init" not in log
        else:
            assert "switching to real init" in log
        print(f"DISK_ROOT_CASE_PASS arch={args.arch} case={name}", flush=True)
    print(f"RBB_DISK_ROOT_END_TO_END_PASS arch={args.arch}", flush=True)


if __name__ == "__main__":
    try:
        main()
    except (AssertionError, OSError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        sys.exit(1)
