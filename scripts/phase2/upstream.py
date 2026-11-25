"""Synthetic Git Smart HTTP endpoint for the isolated Phase 2 probes."""

import http.server
import os
from pathlib import Path
import ssl
import subprocess
import threading
import time
from urllib.parse import urlsplit


ROOT = Path("/fixture")


class Endpoint(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.git()

    def do_POST(self):
        self.git()

    def git(self):
        path = urlsplit(self.path)
        if path.path == "/health":
            self.send_response(200)
            self.end_headers()
            return
        parts = path.path.split("/")
        if len(parts) < 3 or parts[1] not in ("sha1.git", "sha256.git", "cross.git"):
            self.send_error(404)
            return
        if path.path not in (f"/{parts[1]}/info/refs", f"/{parts[1]}/git-upload-pack"):
            self.send_error(404)
            return
        mode = (ROOT / "mode").read_text().strip()
        if mode == "redirect":
            self.send_response(302)
            self.send_header("Location", "http://127.0.0.1:8081/healthz")
            self.end_headers()
            return
        if mode == "unavailable":
            self.send_response(503)
            self.send_header("Retry-After", "0")
            self.end_headers()
            return
        length = int(self.headers.get("Content-Length", "0"))
        if not 0 <= length < 256 * 1024:
            self.send_error(413)
            return
        body = self.rfile.read(length)
        fetching = b"command=fetch" in body
        if mode == "delay" and fetching:
            time.sleep(30)
        result = subprocess.run(
            ["git", "http-backend"], input=body, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            check=False, timeout=35, env={
                "PATH": os.environ["PATH"], "GIT_PROJECT_ROOT": str(ROOT / "repositories"),
                "GIT_HTTP_EXPORT_ALL": "1", "GIT_PROTOCOL": self.headers.get("Git-Protocol", ""),
                "GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": "safe.directory", "GIT_CONFIG_VALUE_0": "*",
                "REQUEST_METHOD": self.command, "PATH_INFO": path.path, "QUERY_STRING": path.query,
                "CONTENT_TYPE": self.headers.get("Content-Type", ""), "CONTENT_LENGTH": str(length),
            },
        )
        if result.returncode:
            print(f"synthetic git-http-backend failed with status {result.returncode}", flush=True)
            self.send_error(500)
            return
        header, payload = result.stdout.split(b"\r\n\r\n", 1)
        headers = dict(line.decode().split(": ", 1) for line in header.split(b"\r\n"))
        code = int(headers.pop("Status", "200").split()[0])
        if mode == "malformed" and fetching and b"PACK" in payload:
            position = payload.index(b"PACK")
            payload = payload[:position] + b"FAIL" + payload[position + 4:]
        self.send_response(code)
        for key, value in headers.items():
            self.send_header(key, value)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        try:
            if mode == "partial" and fetching:
                self.wfile.write(payload[:len(payload) // 2])
                self.close_connection = True
            else:
                self.wfile.write(payload)
        except (BrokenPipeError, ConnectionResetError, ssl.SSLError):
            pass

    def log_message(self, *_):
        pass


def main():
    plain = http.server.ThreadingHTTPServer(("0.0.0.0", 8088), Endpoint)
    https = http.server.ThreadingHTTPServer(("0.0.0.0", 8443), Endpoint)
    tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    tls.load_cert_chain(ROOT / "tls/upstream.crt", ROOT / "tls/upstream.key")
    https.socket = tls.wrap_socket(https.socket, server_side=True)
    threading.Thread(target=plain.serve_forever, daemon=True).start()
    https.serve_forever()


if __name__ == "__main__":
    main()
