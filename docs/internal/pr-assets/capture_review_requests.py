#!/usr/bin/env python3
"""Capture the built review-requests route with deterministic API fixtures."""

from __future__ import annotations

import json
import mimetypes
import threading
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlparse

from playwright.sync_api import sync_playwright


ASSET_DIR = Path(__file__).resolve().parent
REPOSITORY_ROOT = ASSET_DIR.parents[2]
WEB_DIST = REPOSITORY_ROOT / "apps" / "fabro-web" / "dist"

AUTH_FIXTURE = {
    "user": {
        "login": "maecapozzi",
        "name": "Mae Capozzi",
        "email": "mae@example.com",
        "idpIssuer": "https://github.com",
        "idpSubject": "12345",
        "avatarUrl": "https://avatars.githubusercontent.com/u/1?v=4",
        "userUrl": "https://github.com/maecapozzi",
    },
    "provider": "github",
    "demoMode": False,
}

REVIEW_REQUESTS_FIXTURE = {
    "data": [
        {
            "owner": "maecapozzi",
            "repository": "fabro",
            "number": 418,
            "title": "Add a personal review queue",
            "author": "octocat",
            "html_url": "https://github.com/maecapozzi/fabro/pull/418",
            "draft": False,
            "request_kind": "direct_and_team",
            "teams": ["maecapozzi/platform"],
            "updated_at": "2026-11-13T19:58:00Z",
        },
        {
            "owner": "acme",
            "repository": "platform",
            "number": 91,
            "title": "Harden webhook signature verification",
            "author": "hubot",
            "html_url": "https://github.com/acme/platform/pull/91",
            "draft": False,
            "request_kind": "direct",
            "teams": [],
            "updated_at": "2026-11-13T18:42:00Z",
        },
        {
            "owner": "workos",
            "repository": "backstage",
            "number": 1204,
            "title": "Update checkout retry policy",
            "author": "alex",
            "html_url": "https://github.com/workos/backstage/pull/1204",
            "draft": False,
            "request_kind": "team",
            "teams": ["workos/reviewers"],
            "updated_at": "2026-11-13T16:10:00Z",
        },
        {
            "owner": "acme",
            "repository": "payments",
            "number": 73,
            "title": "Rotate checkout credentials",
            "author": "security-bot",
            "html_url": "https://github.com/acme/payments/pull/73",
            "draft": True,
            "request_kind": "team",
            "teams": ["acme/security"],
            "updated_at": "2026-11-12T17:25:00Z",
        },
    ],
    "meta": {
        "reviewer": "maecapozzi",
        "fetched_at": "2026-11-13T20:00:00Z",
        "incomplete": False,
        "team_coverage": "complete",
    },
}


class FixtureHandler(SimpleHTTPRequestHandler):
    def log_message(self, _format: str, *_args: object) -> None:
        pass

    def send_json(self, value: object) -> None:
        body = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:  # noqa: N802 - inherited HTTP handler name
        path = urlparse(self.path).path
        if path == "/api/v1/auth/me":
            self.send_json(AUTH_FIXTURE)
            return
        if path == "/api/v1/review-requests":
            self.send_json(REVIEW_REQUESTS_FIXTURE)
            return

        requested = WEB_DIST / path.lstrip("/")
        if path == "/" or not requested.is_file():
            requested = WEB_DIST / "index.html"
        body = requested.read_bytes()
        self.send_response(200)
        self.send_header(
            "content-type",
            mimetypes.guess_type(requested.name)[0] or "application/octet-stream",
        )
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def capture() -> None:
    if not (WEB_DIST / "index.html").is_file():
        raise SystemExit("Build the web app first: cd apps/fabro-web && bun run build")

    server = ThreadingHTTPServer(("127.0.0.1", 0), FixtureHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    url = f"http://127.0.0.1:{server.server_port}/review-requests"

    try:
        with sync_playwright() as playwright:
            browser = playwright.chromium.launch()
            for name, viewport in [
                ("review-requests-desktop.png", {"width": 1440, "height": 900}),
                ("review-requests-mobile.png", {"width": 390, "height": 844}),
            ]:
                page = browser.new_page(
                    viewport=viewport,
                    device_scale_factor=1,
                    color_scheme="dark",
                )
                page.goto(url, wait_until="networkidle")
                page.get_by_role("heading", name="Ready for review").wait_for()
                page.screenshot(path=ASSET_DIR / name, full_page=True)
                page.close()
            browser.close()
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


if __name__ == "__main__":
    capture()
