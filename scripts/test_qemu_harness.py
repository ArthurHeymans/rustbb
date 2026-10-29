"""Host-side tests for architecture selection and EFI zboot normalization."""
import argparse
import gzip
import importlib.util
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location("qemu_harness", Path(__file__).with_name("test-qemu.py"))
HARNESS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(HARNESS)


class HarnessTests(unittest.TestCase):
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
