#!/usr/bin/env python3
"""Boot optional uutils/brush tools and the generated interactive rescue console."""
import argparse
import importlib.util
import json
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("disk", ROOT / "scripts/test-disk-root.py")
DISK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(DISK)
QEMU = DISK.QEMU


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--arch", choices=QEMU.ARCHITECTURES, default="x86_64")
    parser.add_argument("--kernel", type=Path)
    parser.add_argument("--timeout", type=int, default=90)
    parser.add_argument("--skip-minimal-build", action="store_true", help="debug: reuse the rich build and skip size/build-selection checks")
    args = parser.parse_args()
    args.qemu = QEMU.ARCHITECTURES[args.arch]["qemu"]
    mkimage, binary, guest, target = DISK.build(args.arch, features="rescue-tools" if args.skip_minimal_build else "")
    results = target / "rescue-tools-tests" / args.arch
    results.mkdir(parents=True, exist_ok=True)
    minimal = results / "minimal.cpio.gz"
    # Exercise the builder's own compiler/linker discovery and preset feature choice.
    sizes = {}
    if not args.skip_minimal_build:
        QEMU.run([mkimage, "--build", "--workspace", ROOT, "--target", QEMU.ARCHITECTURES[args.arch]["target"], "--preset", "minimal", "-o", minimal])
        sizes.update(minimal_binary_bytes=binary.stat().st_size, minimal_image_bytes=minimal.stat().st_size)
    mkimage, binary, guest, target = DISK.build(args.arch, features="rescue-tools")
    sizes["rescue_binary_bytes"] = binary.stat().st_size
    if not args.skip_minimal_build:
        assert sizes["rescue_binary_bytes"] > sizes["minimal_binary_bytes"], "optional dependencies did not change the build"
    kernel = args.kernel or target / "test-kernels" / f"rbb-{args.arch}" / ("arch/arm64/boot/Image" if args.arch == "arm64" else "arch/x86/boot/bzImage")
    if args.arch == "arm64":
        kernel = QEMU.prepare_arm64_kernel(kernel, results)
    config = results / "init.toml"
    config.write_text('''[[services]]
name = "verify"
kind = "oneshot"
command = ["/bin/guest-test", "richer-stage"]
''')
    image = results / "tools.cpio.gz"
    QEMU.run([mkimage, "--busybox", binary, "--preset", "rescue", "--file", f"{guest}:/bin/guest-test", "--file", f"{config}:/etc/rbb/init.toml", "-o", image])
    try:
        result = subprocess.run(QEMU.qemu_command(args, kernel, image), stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=args.timeout)
    except subprocess.TimeoutExpired as error:
        text = (error.stdout or b"").decode(errors="replace")
        (results / "tools.log").write_text(text)
        print(text, flush=True)
        raise RuntimeError(f"tools VM timed out; see {results / 'tools.log'}") from error
    text = result.stdout.decode(errors="replace")
    (results / "tools.log").write_text(text)
    print(text, flush=True)
    assert result.returncode == 0 and "Kernel panic" not in text and "RBB_GUEST_TEST_FAIL" not in text
    assert f"RBB_RICH_USERLAND_PASS arch={QEMU.ARCHITECTURES[args.arch]['uname']}" in text
    console = results / "console.cpio.gz"
    QEMU.run([mkimage, "--busybox", binary, "--preset", "rescue", "-o", console])
    sizes["rescue_image_bytes"] = console.stat().st_size
    (results / "sizes.json").write_text(json.dumps(sizes, indent=2) + "\n")
    QEMU.test_console(args, kernel, console, results / "console.log", prompt=None)
    print(f"RBB_RESCUE_TOOLS_END_TO_END_PASS arch={args.arch} sizes={sizes}", flush=True)


if __name__ == "__main__":
    try:
        main()
    except (AssertionError, OSError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        sys.exit(1)
