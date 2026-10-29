#!/usr/bin/env python3
"""Build dedicated QEMU kernels from a read-only Linux Git revision, out of tree."""
import argparse
from datetime import datetime, timezone
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def run(command, **kwargs):
    print("+", " ".join(map(str, command)), flush=True)
    return subprocess.run(list(map(str, command)), check=True, **kwargs)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--revision", required=True, help="Linux Git commit; never changes its checkout")
    parser.add_argument("--arch", choices=["x86_64", "arm64", "both"], default="both")
    parser.add_argument("--jobs", type=int, default=min(os.cpu_count() or 1, 12))
    parser.add_argument("--cc", default=os.environ.get("KERNEL_CC", "clang"), help="target clang; prefer unwrapped clang on Nix")
    parser.add_argument("--hostcc", default=os.environ.get("HOSTCC", "clang"))
    parser.add_argument("--ld", default=os.environ.get("KERNEL_LD", "ld.lld"), help="target linker; prefer unwrapped lld on Nix")
    args = parser.parse_args()
    for tool in ["git", "make", "clang", "ld.lld", "llvm-ar", "llvm-nm", "llvm-objcopy", "bison", "flex", "bc", "pkg-config", args.cc, args.hostcc, args.ld]:
        if not shutil.which(tool):
            parser.error(f"missing {tool}; enter a kernel build environment first")
    revision = subprocess.check_output(["git", "-C", str(args.source), "rev-parse", f"{args.revision}^{{commit}}"], text=True).strip()
    if len(revision) not in [40, 64] or any(c not in "0123456789abcdef" for c in revision):
        parser.error("revision did not resolve to a full Git commit hash")
    if not 1 <= args.jobs <= 128:
        parser.error("jobs must be 1..128")
    epoch = subprocess.check_output(["git", "-C", str(args.source), "show", "-s", "--format=%ct", revision], text=True).strip()
    env = os.environ.copy()
    env.update(SOURCE_DATE_EPOCH=epoch, KBUILD_BUILD_TIMESTAMP=datetime.fromtimestamp(int(epoch), timezone.utc).strftime("%a %b %d %H:%M:%S UTC %Y"), KBUILD_BUILD_USER="rustbb", KBUILD_BUILD_HOST="qemu", KBUILD_BUILD_VERSION="1")
    artifacts = ROOT / "target/test-kernels"
    artifacts.mkdir(parents=True, exist_ok=True)
    source = artifacts / f"linux-{revision}"
    if not source.exists():
        with tempfile.TemporaryDirectory(dir=artifacts) as tmp:
            tmp = Path(tmp)
            archive = tmp / "source.tar"
            run(["git", "-C", args.source, "archive", revision, "-o", archive])
            tree = tmp / "source"
            tree.mkdir()
            with tarfile.open(archive) as tar:
                tar.extractall(tree, filter="data")
            tree.rename(source)
    for arch in (["x86_64", "arm64"] if args.arch == "both" else [args.arch]):
        out = artifacts / f"rbb-{arch}"
        out.mkdir(exist_ok=True)
        fragment = out / "fragment.config"
        fragment.write_text((ROOT / "tests/kernels/common.config").read_text() + (ROOT / f"tests/kernels/{arch}.config").read_text())
        make_arch = "arm64" if arch == "arm64" else "x86"
        base = ["make", "-C", source, f"O={out}", f"ARCH={make_arch}", "LLVM=1", f"CC={args.cc}", f"HOSTCC={args.hostcc}", f"LD={args.ld}"]
        run([*base, f"KCONFIG_ALLCONFIG={fragment}", "allnoconfig"], env=env)
        config = (out / ".config").read_text()
        for feature in ["KEXEC", "KEXEC_FILE", "VIRTIO_BLK", "VIRTIO_NET", "EXT4_FS", "DEVTMPFS", "SIGNALFD", "BPF", "RTC_HCTOSYS"]:
            if f"CONFIG_{feature}=y\n" not in config:
                raise RuntimeError(f"{arch}: required {feature} not enabled")
        target = "Image" if arch == "arm64" else "bzImage"
        run([*base, f"-j{args.jobs}", target], env=env)
        binary = out / ("arch/arm64/boot/Image" if arch == "arm64" else "arch/x86/boot/bzImage")
        (out / "provenance.txt").write_text(f"Linux commit: {revision}\nBuild epoch: {epoch}\nKernel SHA256: {hashlib.sha256(binary.read_bytes()).hexdigest()}\nConfig SHA256: {hashlib.sha256((out / '.config').read_bytes()).hexdigest()}\n")
        print(f"KERNEL_READY arch={arch} path={binary}", flush=True)


if __name__ == "__main__":
    main()
