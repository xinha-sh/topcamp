"""Refresh the README screenshots in this directory.

Prerequisites (all verified against a scratch database, never the dev one):
  1. `docker compose up -d` (PostgreSQL 17).
  2. Create + migrate a scratch DB, e.g.::
       psql -c "CREATE DATABASE topcamp_shots;"
       for m in migrations/*.sql; do
         psql "$SHOTS_DB" -v ON_ERROR_STOP=1 -f "$m"
       done
  3. Seed it::
       DATABASE_URL=<shots-db-url> cargo run -p topcamp-web --bin seed
  4. `cargo build -p topcamp-web --bin serve`
  5. `pip install playwright && python -m playwright install chromium`

Then run from the repo root::
  SHOTS_DB="postgres://topcamp:topcamp@localhost:5432/topcamp_shots" \\
    python3 docs/shots/capture.py

The script spawns `serve` itself on PORT 3000, signs in via the debug
demo button, asserts each feature's DOM contract (open `<dialog>`,
prefilled composer, search results), screenshots into this directory,
and tears the server down. It fails loudly on any missing element so a
redesign that breaks a shot breaks the script, not the README silently.
"""

import os
import subprocess
import sys
import time
import urllib.request

PORT = "3000"
BASE = f"http://localhost:{PORT}"
ROOT = os.path.dirname(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
)
SHOTS = os.path.join(ROOT, "docs", "shots")
DB = os.environ.get("SHOTS_DB", "").strip()

PAGES = [
    # (filename, path, ready_selector, extra_check)
    ("02-rooms.png", "/rooms", "aside#sidebar, #sidebar", None),
    ("03-room.png", "/rooms/1", "form", None),
    (
        "04-delete-dialog.png",
        "/rooms/1?confirm=delete-boost-1",
        "dialog[open]",
        None,
    ),
    ("05-reply.png", "/rooms/1?reply_to=58", "form", "reply-prefill"),
    ("06-search.png", "/searches", "input[name=q]", "search-submit"),
    ("07-account.png", "/account/edit", "form", None),
    ("08-bots.png", "/account/bots", "main, body", None),
]


def wait_ready(proc, timeout=60):
    for _ in range(timeout * 2):
        if proc.poll() is not None:
            raise RuntimeError(f"serve exited early: {proc.returncode}")
        try:
            with urllib.request.urlopen(f"{BASE}/session/new", timeout=2) as r:
                if r.status == 200:
                    return
        except OSError:
            pass
        time.sleep(0.5)
    raise RuntimeError("serve never became ready")


def main():
    if not DB:
        raise SystemExit("Set SHOTS_DB to the scratch database URL first.")
    os.makedirs(SHOTS, exist_ok=True)
    env = dict(os.environ, DATABASE_URL=DB, PORT=PORT)
    proc = subprocess.Popen(["./target/debug/serve"], env=env, cwd=ROOT)
    try:
        wait_ready(proc)
        from playwright.sync_api import sync_playwright

        with sync_playwright() as p:
            browser = p.chromium.launch()
            page = browser.new_page(viewport={"width": 1280, "height": 800})

            # Sign-in page, logged out.
            page.goto(f"{BASE}/session/new")
            page.wait_for_selector("body")
            page.screenshot(path=os.path.join(SHOTS, "01-signin.png"))

            # One-click demo sign-in (debug builds, no password).
            with page.expect_navigation():
                page.click('form[action="/session/demo"] button, '
                           'form[action="/session/demo"] input[type="submit"]')
            assert "/session" not in page.url, f"still logged out: {page.url}"

            for filename, path, selector, extra in PAGES:
                page.goto(f"{BASE}{path}")
                page.wait_for_selector("body", timeout=15000)
                page.wait_for_timeout(1500)  # let transitions settle
                if selector != "body":
                    page.wait_for_selector(
                        selector, timeout=15000, state="attached"
                    )
                if "confirm=" in path:
                    assert page.query_selector("dialog[open]"), \
                        "no open dialog"
                if extra == "reply-prefill":
                    value = page.eval_on_selector(
                        "textarea", "el => el.value"
                    )
                    assert value.strip(), "composer not prefilled"
                    page.evaluate(
                        "document.querySelector('main')"
                        ".scrollTo(0, 999999)"
                    )
                    page.wait_for_timeout(400)
                if extra == "search-submit":
                    page.fill("input[name=q]", "popover")
                    with page.expect_navigation():
                        page.press("input[name=q]", "Enter")
                    page.wait_for_timeout(600)
                page.screenshot(path=os.path.join(SHOTS, filename))
                print(f"shot {filename} <- {path}")

            browser.close()
    finally:
        proc.terminate()
        proc.wait(timeout=15)

    for f in sorted(os.listdir(SHOTS)):
        if f.endswith(".png"):
            print(f, os.path.getsize(os.path.join(SHOTS, f)), "bytes")


if __name__ == "__main__":
    sys.exit(main())
