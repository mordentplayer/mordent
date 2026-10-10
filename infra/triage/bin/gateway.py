#!/usr/bin/env python3
"""Credential-injecting gateway for the Mordent issue-triage sandbox.

This process runs OUTSIDE the model sandbox: on the CI runner host, or as a
sidecar container attached only to the sandbox's docker network. It is the one
and only place the real OpenCode API credential exists. The sandboxed agent
talks to it over plain HTTP using an ephemeral, non-sensitive bearer token,
and the gateway swaps that token for the real credential in the upstream
request. The sandbox therefore never sees, and can never print, the credential
-- even if the issue text tries to prompt-inject the model into leaking it.

Security properties (see infra/triage/README.md for the full analysis):
  * The upstream credential is read from $OPENCODE_CRED, is never logged, is
    never written to disk, and is never returned to clients.
  * Any client-supplied Authorization header is discarded and overwritten.
  * Only /v1/* paths are proxied, to one fixed upstream host.
  * `/v1` is a chat-completions/ models API -- there is no route that echoes
    request headers, so the injected credential cannot be reflected back.
  * Request/response bodies are relayed byte-for-byte; the only rewrite is the
    `model` field of a JSON request body (so the sandbox can name the model
    whatever it likes without knowing the upstream id).

Environment:
  OPENCODE_CRED  real upstream credential (required; never logged)
  GW_TOKEN       ephemeral bearer the sandbox must present (required)
  GW_UPSTREAM    upstream base URL   (default https://opencode.ai/zen/go)
  GW_MODEL       upstream model id   (default muse-spark-1.3-contributor)
  GW_PORT        listen port         (default 4000)
  GW_SESSION     x-opencode-session value (informational)
"""

from __future__ import annotations

import json
import os
import re
import sys
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

UPSTREAM = os.environ.get("GW_UPSTREAM", "https://opencode.ai/zen/go").rstrip("/")
MODEL = os.environ.get("GW_MODEL", "muse-spark-1.3-contributor")
PORT = int(os.environ.get("GW_PORT", "4000"))
SESSION = os.environ.get("GW_SESSION", "mordent-triage")
TOKEN = os.environ["GW_TOKEN"]
CRED = os.environ["OPENCODE_CRED"]

# Hop-by-hop / length headers we must not blindly relay.
DROP_REQ = {
    "authorization",
    "host",
    "content-length",
    "connection",
    "transfer-encoding",
    "accept-encoding",  # keep upstream identity-encoded: raw relay stays simple
    "expect",
    "proxy-connection",
    "te",
    "trailer",
    "upgrade",
}
DROP_RESP = {"content-length", "transfer-encoding", "connection", "keep-alive"}

# Anything that looks like an OpenCode credential, kept out of the log stream.
_CRED_RE = re.compile(r"oc_[A-Za-z0-9_\-]{8,}")


def scrub(text: str) -> str:
    text = _CRED_RE.sub("oc_<redacted>", text)
    return text.replace(CRED, "<redacted>")


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "mordent-triage-gateway"

    def log_message(self, fmt: str, *args) -> None:  # noqa: D102
        sys.stderr.write("gw: " + scrub(fmt % args) + "\n")
        sys.stderr.flush()

    def _json_error(self, code: int, message: str) -> None:
        body = json.dumps({"error": {"message": message, "type": "gateway_error"}}).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _handle(self) -> None:
        auth = self.headers.get("Authorization", "")
        if not auth.startswith("Bearer ") or auth[7:] != TOKEN:
            self._json_error(401, "invalid gateway token")
            return
        if not self.path.startswith("/v1/") or ".." in self.path:
            self._json_error(404, "not found")
            return

        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else b""
        if body:
            try:
                doc = json.loads(body)
                if isinstance(doc, dict) and "model" in doc:
                    doc["model"] = MODEL
                    body = json.dumps(doc).encode()
            except Exception:
                pass  # non-JSON payloads relay untouched

        headers = {k: v for k, v in self.headers.items() if k.lower() not in DROP_REQ}
        headers["Authorization"] = "Bearer " + CRED
        headers["x-opencode-session"] = SESSION

        req = urllib.request.Request(
            UPSTREAM + self.path, data=body or None, headers=headers, method=self.command
        )
        try:
            resp = urllib.request.urlopen(req, timeout=900)
        except urllib.error.HTTPError as exc:
            resp = exc
        except Exception as exc:  # noqa: BLE001
            self.log_message("upstream error: %s", scrub(str(exc)))
            self._json_error(502, "upstream unreachable")
            return

        status = getattr(resp, "status", 200)
        self.send_response(status)
        for key, value in resp.headers.items():
            if key.lower() not in DROP_RESP:
                self.send_header(key, value)
        self.send_header("Transfer-Encoding", "chunked")
        self.send_header("Connection", "close")
        self.end_headers()
        try:
            while True:
                chunk = resp.read(8192)
                if not chunk:
                    break
                self.wfile.write(b"%x\r\n" % len(chunk) + chunk + b"\r\n")
                self.wfile.flush()
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
        except Exception as exc:  # noqa: BLE001
            self.log_message("relay aborted: %s", exc)
        finally:
            resp.close()

    do_GET = _handle
    do_POST = _handle


def main() -> None:
    server = ThreadingHTTPServer(("0.0.0.0", PORT), Handler)
    sys.stderr.write(f"gw: listening on 0.0.0.0:{PORT} -> {UPSTREAM} (model {MODEL})\n")
    sys.stderr.flush()
    server.serve_forever()


if __name__ == "__main__":
    main()
