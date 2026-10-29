"""Host-side tests for architecture selection and EFI zboot normalization."""
import argparse
import gzip
import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location("qemu_harness", Path(__file__).with_name("test-qemu.py"))
HARNESS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(HARNESS)


class HarnessTests(unittest.TestCase):
    def compiler_environment(self, arch, variables=None, tools=("clang", "llvm-ar")):
        with mock.patch.dict(HARNESS.os.environ, variables or {}, clear=True), \
             mock.patch.object(HARNESS.subprocess, "check_output", side_effect=["/sysroot", "host: x86_64-unknown-linux-gnu"]), \
             mock.patch.object(Path, "is_file", return_value=True), \
             mock.patch.object(HARNESS.shutil, "which", side_effect=lambda name: f"/tools/{name}" if name in tools else None):
            return HARNESS.build_environment(arch)

    def test_native_tls_compiler_and_rust_linker(self):
        env = self.compiler_environment("x86_64")
        self.assertEqual(env["CC_x86_64_unknown_linux_musl"], "clang")
        self.assertEqual(env["AR_x86_64_unknown_linux_musl"], "llvm-ar")
        self.assertTrue(env["CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER"].endswith("/rust-lld"))

    def test_cross_compiler_is_preserved_or_discovered(self):
        env = self.compiler_environment("arm64", {"CC_aarch64_unknown_linux_musl": "/custom/cc"})
        self.assertEqual(env["CC_aarch64_unknown_linux_musl"], "/custom/cc")
        env = self.compiler_environment("arm64", tools=("aarch64-linux-musl-gcc",))
        self.assertEqual(env["CC_aarch64_unknown_linux_musl"], "aarch64-linux-musl-gcc")

    def test_missing_cross_compiler_does_not_use_host_cc(self):
        with self.assertRaisesRegex(RuntimeError, "set CC_aarch64_unknown_linux_musl"):
            self.compiler_environment("arm64")

    def test_architecture_selects_machine_and_console(self):
        for arch, machine, console in [("x86_64", "pc,accel=tcg", "ttyS0"), ("arm64", "virt,accel=tcg", "ttyAMA0")]:
            args = argparse.Namespace(arch=arch, qemu=HARNESS.ARCHITECTURES[arch]["qemu"])
            command = HARNESS.qemu_command(args, Path("kernel"), Path("initramfs"))
            self.assertEqual(command[command.index("-machine") + 1], machine)
            self.assertIn(f"console={console} ", command[command.index("-append") + 1])
            self.assertIn("-no-reboot", command)

    def test_unwraps_gzip_zboot(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            image = bytearray(64)
            image[56:60] = b"ARMd"
            compressed = gzip.compress(image)
            header = bytearray(64)
            header[:2] = b"MZ"
            header[4:8] = b"zimg"
            header[8:12] = (64).to_bytes(4, "little")
            header[12:16] = len(compressed).to_bytes(4, "little")
            header[24:28] = b"gzip"
            kernel = root / "vmlinuz"
            kernel.write_bytes(header + compressed)
            output = HARNESS.prepare_arm64_kernel(kernel, root)
            self.assertEqual(output.read_bytes(), image)
            self.assertEqual(kernel.read_bytes(), header + compressed)

    def test_rejects_invalid_zboot(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "vmlinuz"
            for data in [b"MZ\0\0zimg", b"MZ\0\0zimg" + bytes(56)]:
                kernel.write_bytes(data)
                with self.assertRaises(RuntimeError):
                    HARNESS.prepare_arm64_kernel(kernel, root)
            header = bytearray(64)
            header[4:8] = b"zimg"
            header[8:12] = (64).to_bytes(4, "little")
            header[12:16] = (4096).to_bytes(4, "little")
            header[24:28] = b"gzip"
            kernel.write_bytes(header)
            with self.assertRaisesRegex(RuntimeError, "bounds"):
                HARNESS.prepare_arm64_kernel(kernel, root)

    def test_raw_image_is_unchanged(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "Image"
            kernel.write_bytes(b"raw image")
            self.assertEqual(HARNESS.prepare_arm64_kernel(kernel, root), kernel)


if __name__ == "__main__":
    unittest.main()
