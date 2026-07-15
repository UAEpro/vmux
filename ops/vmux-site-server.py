#!/usr/bin/env python3
"""Small hardened static origin for vmux.sh, intended to sit behind Cloudflare."""

from __future__ import annotations

import argparse
from http import HTTPStatus
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit


class SiteHandler(SimpleHTTPRequestHandler):
    server_version = "vmux"
    sys_version = ""
    extensions_map = {
        **SimpleHTTPRequestHandler.extensions_map,
        ".webmanifest": "application/manifest+json",
        ".xml": "application/xml",
        ".ttf": "font/ttf",
    }

    def redirect_www(self) -> bool:
        host = self.headers.get("Host", "").partition(":")[0].lower()
        if host != "www.vmux.sh":
            return False
        self.send_response(HTTPStatus.PERMANENT_REDIRECT)
        self.send_header("Location", f"https://vmux.sh{self.path}")
        self.send_header("Content-Length", "0")
        self.end_headers()
        return True

    def do_GET(self) -> None:
        if not self.redirect_www():
            super().do_GET()

    def do_HEAD(self) -> None:
        if not self.redirect_www():
            super().do_HEAD()

    def end_headers(self) -> None:
        path = urlsplit(self.path).path
        if path.startswith("/assets/"):
            self.send_header("Cache-Control", "public, max-age=3600, must-revalidate")
        else:
            self.send_header("Cache-Control", "public, max-age=300, must-revalidate")
        self.send_header("Content-Security-Policy", "default-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'; font-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'; upgrade-insecure-requests")
        self.send_header("Cross-Origin-Opener-Policy", "same-origin")
        self.send_header("Permissions-Policy", "camera=(), microphone=(), geolocation=(), payment=(), usb=()")
        self.send_header("Referrer-Policy", "strict-origin-when-cross-origin")
        self.send_header("Strict-Transport-Security", "max-age=31536000; includeSubDomains")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("X-Frame-Options", "DENY")
        super().end_headers()

    def send_error(self, code: int, message: str | None = None, explain: str | None = None) -> None:
        if code != HTTPStatus.NOT_FOUND:
            return super().send_error(code, message, explain)

        body_path = Path(self.directory) / "404.html"
        try:
            body = body_path.read_bytes()
        except OSError:
            return super().send_error(code, message, explain)
        self.send_response(HTTPStatus.NOT_FOUND)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--directory", required=True)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=4173)
    args = parser.parse_args()

    handler = lambda *handler_args, **handler_kwargs: SiteHandler(  # noqa: E731
        *handler_args,
        directory=args.directory,
        **handler_kwargs,
    )
    server = ThreadingHTTPServer((args.host, args.port), handler)
    server.serve_forever()


if __name__ == "__main__":
    main()
