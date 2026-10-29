#!/usr/bin/env python3
"""Real DHCP + HTTP(S)/TFTP -> second-kernel boot, with negative trust cases."""
import argparse
from functools import partial
import hashlib
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import os
from pathlib import Path
import secrets
import shutil
import ssl
import subprocess
import sys
import threading

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("disk", ROOT / "scripts/test-disk-root.py")
DISK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(DISK)
QEMU = DISK.QEMU
run = QEMU.run
CASES = ["dhcp-ipxe-http", "dhcp-pxe-tftp", "native-http", "signed-https", "bad-signature", "bad-artifact", "untrusted-tls", "missing-download", "dhcp-timeout", "static-http", "redirect-http", "redirect-signed-https", "tls-downgrade"]


class Handler(SimpleHTTPRequestHandler):
    def do_GET(self):
        self.server.paths.append(self.path)
        if self.path in ["/redirect.toml", "/downgrade.toml"]:
            location = "/nested/manifest.toml" if self.path == "/redirect.toml" else f"http://10.0.2.2:{self.server.http_port}/manifest.toml"
            self.send_response(302)
            self.send_header("Location", location)
            self.send_header("Content-Length", "0")
            self.end_headers()
        else:
            super().do_GET()

    def log_message(self, message, *args):
        pass


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def certificates(openssl, directory):
    ca, key = directory / "ca.crt", directory / "ca.key"
    server, server_key = directory / "server.crt", directory / "server.key"
    run([openssl, "req", "-x509", "-newkey", "ed25519", "-nodes", "-keyout", key, "-out", ca, "-days", "2", "-subj", "/CN=rbb test CA", "-addext", "basicConstraints=critical,CA:TRUE", "-addext", "keyUsage=critical,keyCertSign,cRLSign"], capture_output=True)
    csr = directory / "server.csr"
    run([openssl, "req", "-new", "-newkey", "ed25519", "-nodes", "-keyout", server_key, "-out", csr, "-subj", "/CN=10.0.2.2"], capture_output=True)
    extensions = directory / "server.ext"
    extensions.write_text("subjectAltName=IP:10.0.2.2,DNS:localhost\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n")
    run([openssl, "x509", "-req", "-in", csr, "-CA", ca, "-CAkey", key, "-CAcreateserial", "-out", server, "-days", "2", "-extfile", extensions], capture_output=True)
    return ca, server, server_key


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--arch", choices=QEMU.ARCHITECTURES, default="x86_64")
    parser.add_argument("--kernel", type=Path)
    parser.add_argument("--case", choices=CASES, action="append")
    parser.add_argument("--openssl", default=os.environ.get("OPENSSL", "openssl"))
    parser.add_argument("--timeout", type=int, default=120)
    args = parser.parse_args()
    if not shutil.which(args.openssl):
        parser.error("OpenSSL is needed only on the host to create disposable TLS fixtures")
    args.qemu = QEMU.ARCHITECTURES[args.arch]["qemu"]
    arch = QEMU.ARCHITECTURES[args.arch]["uname"]
    mkimage, binary, guest, target = DISK.build(args.arch)
    signer = target / "debug/rbb-sign"
    results = target / "netboot-tests" / args.arch
    results.mkdir(parents=True, exist_ok=True)
    web = results / "web"
    web.mkdir(exist_ok=True)
    (web / "pxelinux.cfg").mkdir(exist_ok=True)
    kernel = (args.kernel or target / "test-kernels" / f"rbb-{args.arch}" / ("arch/arm64/boot/Image" if args.arch == "arm64" else "arch/x86/boot/bzImage")).resolve()
    if args.arch == "arm64":
        kernel = QEMU.prepare_arm64_kernel(kernel, results)
    shutil.copy2(kernel, web / "kernel")
    (web / "kernel.bad").write_bytes(b"tampered kernel")
    (web / "pxelinux.0").write_text("PXE placeholder; config discovery must be used")
    seed = results / "test-signing-seed"
    seed.write_text(secrets.token_hex(32))
    seed.chmod(0o600)
    public_key = results / "public-key"
    ca, certificate, private_key = certificates(args.openssl, results)
    http = ThreadingHTTPServer(("127.0.0.1", 0), partial(Handler, directory=str(web)))
    https = ThreadingHTTPServer(("127.0.0.1", 0), partial(Handler, directory=str(web)))
    for server in [http, https]:
        server.paths = []
        server.http_port = http.server_port
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(certificate, private_key)
    https.socket = context.wrap_socket(https.socket, server_side=True)
    threads = [threading.Thread(target=server.serve_forever, daemon=True) for server in [http, https]]
    for thread in threads:
        thread.start()
    try:
        for name in args.case or CASES:
            http.paths.clear()
            https.paths.clear()
            redirected = name.startswith("redirect-")
            proof = secrets.token_hex(16)
            cmdline = f"console={QEMU.ARCHITECTURES[args.arch]['console']} rdinit=/init panic=-1 quiet loglevel=4 rbb.stage=second rbb.netproof={proof}"
            stage_config = results / "second.toml"
            stage_config.write_text('''[[services]]
name = "network-second-stage"
kind = "oneshot"
command = ["/bin/guest-test", "kernel-stage"]
''')
            stage = web / "second.cpio.gz"
            run([mkimage, "--busybox", binary, "--file", f"{guest}:/bin/guest-test", "--file", f"{stage_config}:/etc/rbb/init.toml", "-o", stage])
            source = f"http://10.0.2.2:{http.server_port}/manifest.toml"
            bootfile = f"http://10.0.2.2:{http.server_port}/boot.ipxe"
            native = name not in ["dhcp-ipxe-http", "dhcp-pxe-tftp", "dhcp-timeout"]
            signed = name in ["signed-https", "bad-signature", "bad-artifact", "missing-download", "redirect-signed-https"]
            fallback = name in ["bad-signature", "bad-artifact", "untrusted-tls", "missing-download", "dhcp-timeout", "tls-downgrade"]
            (web / "boot.ipxe").write_text(f"#!ipxe\nset base http://10.0.2.2:{http.server_port}\nchain ${{base}}/child.ipxe\n")
            (web / "child.ipxe").write_text(f"#!ipxe\nkernel kernel {cmdline}\ninitrd second.cpio.gz\nboot\n")
            (web / "pxelinux.cfg/default").write_text(f"DEFAULT net\nLABEL net\nKERNEL kernel\nINITRD second.cpio.gz\nAPPEND {cmdline}\n")
            directory = web / "nested" if redirected else web
            directory.mkdir(exist_ok=True)
            if redirected:
                (directory / "payload").mkdir(exist_ok=True)
                shutil.copy2(kernel, directory / "payload/kernel")
                shutil.copy2(stage, directory / "payload/second.cpio.gz")
            prefix = "payload/" if redirected else ""
            manifest = directory / "manifest.toml"
            manifest.write_text(f'''version = 1
[[entries]]
id = "network"
title = "verified network kernel"
architecture = "{arch}"
cmdline = "{cmdline}"
kernel = {{ source = "{prefix}{'missing' if name == 'missing-download' else 'kernel.bad' if name == 'bad-artifact' else 'kernel'}", sha256 = "{digest(kernel)}" }}
initrd = [{{ source = "{prefix}second.cpio.gz", sha256 = "{digest(stage)}" }}]
''')
            run([signer, "--key", seed, "--manifest", manifest, "--signature", directory / "manifest.toml.sig", "--public-key", public_key])
            if name == "bad-signature":
                manifest.write_text(manifest.read_text() + "\n# changed after signing\n")
            if name in ["signed-https", "untrusted-tls", "redirect-signed-https", "tls-downgrade"]:
                source = f"https://10.0.2.2:{https.server_port}/manifest.toml"
            if redirected:
                source = source.removesuffix("manifest.toml") + "redirect.toml"
            if name == "tls-downgrade":
                source = source.removesuffix("manifest.toml") + "downgrade.toml"
            if not native:
                source = "dhcp"
            if name == "dhcp-pxe-tftp":
                bootfile = "pxelinux.0"
            config = results / f"{name}.toml"
            config.write_text(f'''[network]
dhcp = {'false' if name == 'static-http' else 'true'}
timeout_ms = {700 if name == 'dhcp-timeout' else 15000}
''' + ('addresses = ["10.0.2.15/24"]\ngateway = "10.0.2.2"\n' if name == "static-http" else "") + f'''[boot]
source = "{source}"
require_hashes = {'true' if native else 'false'}
''' + ('manifest_key = "/etc/rbb/public-key"\n' if signed else "") + ('tls_ca = "/etc/rbb/ca.crt"\n' if name in ["signed-https", "redirect-signed-https", "tls-downgrade"] else "") + '''[[services]]
name = "fallback"
kind = "oneshot"
command = ["/bin/guest-test", "boot-fallback"]
''')
            image = results / f"{name}.cpio.gz"
            run([mkimage, "--busybox", binary, "--file", f"{guest}:/bin/guest-test", "--file", f"{config}:/etc/rbb/init.toml", "--file", f"{public_key}:/etc/rbb/public-key", "--file", f"{ca}:/etc/rbb/ca.crt", "-o", image])
            command = QEMU.qemu_command(args, kernel, image)
            command[command.index("-append") + 1] += " rbb.stage=first" + (" rbb.test.no_kexec" if fallback else "")
            index = command.index("-nic")
            del command[index:index + 2]
            backend = "hubport,id=bootnet,hubid=0" if name == "dhcp-timeout" else f"user,id=bootnet,tftp={web},bootfile={bootfile}"
            command += ["-netdev", backend, "-device", "virtio-net-device,netdev=bootnet" if args.arch == "arm64" else "virtio-net-pci,netdev=bootnet"]
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
            assert output.returncode == 0 and "Kernel panic" not in log, f"{name}: QEMU/kernel failure"
            assert "RBB_GUEST_TEST_FAIL" not in log
            if fallback:
                assert "RBB_BOOT_FALLBACK_PASS" in log and "RBB_NETBOOT_PROOF=" not in log, f"{name}: unsafe/no recovery"
                assert "for kexec" not in log, f"{name}: loaded an untrusted image"
            else:
                assert f"RBB_KEXEC_SECOND_KERNEL_PASS arch={arch}" in log
                assert f"RBB_NETBOOT_PROOF={proof}" in log, f"{name}: no fresh server-provided second-stage proof"
                assert "RBB_BOOT_FALLBACK_PASS" not in log
            if name != "static-http" and name != "dhcp-timeout":
                assert "rbb-net: DHCP" in log, f"{name}: no real DHCP lease"
            if name == "bad-signature":
                assert "signature verification failed" in log
            if name == "bad-artifact":
                assert "SHA-256 mismatch" in log
            if name == "dhcp-timeout":
                assert "timed out" in log
            if redirected:
                paths = https.paths if signed else http.paths
                assert "/nested/payload/kernel" in paths, "relative kernel resolved against pre-redirect URL"
                if signed:
                    assert "/nested/manifest.toml.sig" in paths, "signature did not follow effective manifest URL"
            if name == "tls-downgrade":
                assert "/downgrade.toml" in https.paths and "/manifest.toml" not in http.paths, "HTTPS downgraded to plaintext"
            print(f"NETBOOT_CASE_PASS arch={args.arch} case={name}", flush=True)
    finally:
        for server in [http, https]:
            server.shutdown()
            server.server_close()
        for thread in threads:
            thread.join(timeout=5)
    print(f"RBB_NETBOOT_END_TO_END_PASS arch={args.arch}", flush=True)


if __name__ == "__main__":
    try:
        main()
    except (AssertionError, OSError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        sys.exit(1)
