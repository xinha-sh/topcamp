# AGENTS.md — contributing to Topcamp

Topcamp is a slopfork of `basecamp/once-campfire-rust`, rebuilt on the
[Topcoat](https://docs.rs/topcoat) web framework. Our motto: **use Topcoat
to the fullest**. Server-side routing is a deliberate choice — responses are
fast enough that full-page renders feel instant — and view transitions cover
the rest of the UX gap. There is **zero hand-written JavaScript** in the
product UI; keep it that way.

## Repo map

| Path | What lives there |
| ---- | ---------------- |
| `Instruction.md` | The migration specification (58 sections) — read before changing behavior |
| `TASKS.md` / `tasks.json` | Task tracking (human-readable + machine-readable mirror, keep in sync) |
| `TASKS-UI.md` | UI/UX parity program vs upstream, tracked per slice |
| `MIGRATION_NOTES.md`, `TRACES_SUPPLEMENT.md` | Audit evidence, pins, schema and design plans |
| `migrations/` | PostgreSQL migrations (`0001_init` … `0005_cable_fanout`), applied in order |
| `crates/topcamp-domain` | Dependency-free domain rules |
| `crates/topcamp-db` | sqlx repositories + transactional outbox |
| `crates/topcamp-storage` | S3-compatible blob store (RustFS) |
| `crates/topcamp-web` | Topcoat routes, pages, assets (`assets/css|js|images`), `serve`/`seed`/`bundle_assets` bins |
| `crates/topcamp-workflows` | DBOS workflows + outbox relay + worker binary |
| `crates/topcamp-cable` | Action Cable codec, streams, broadcast broker |
| `.agents/skills/` | Contributor skills (see below) |

## Toolchain

- rustc ≥ 1.98 (Topcoat 0.10 MSRV) via rustup **stable first on PATH**.
  Homebrew cargo (1.96) is too old — if `cargo --version` surprises you,
  your PATH is wrong.
- `topcoat` CLI 0.10 (`cargo install topcoat --version 0.10`) for the asset
  bundle step. Tests self-bundle web assets at runtime; only `serve` needs
  `topcoat asset bundle --bin serve -p topcamp-web`.
- Services: `docker compose up -d` (PostgreSQL 17 + RustFS).
  Default dev URL: `postgres://topcamp:topcamp@localhost:5432/topcamp`.

## Gates (all must be green)

```sh
cargo fmt --check
cargo clippy --all-targets   # zero warnings
cargo test --workspace
```

## Database hygiene

- Live test suites each use an **isolated `topcamp_test_*` database**
  (see the header of each file under `crates/topcamp-web/tests/`):
  create it, apply `migrations/*.sql` in order, run, drop it after.
- Never run tests against the shared dev database; never leave probe data
  behind. Fresh probe DBs are cheap — prefer them over debugging pollution.
- `cargo run -p topcamp-web --bin seed` writes demo data: point
  `DATABASE_URL` at a scratch/video database first, never the dev default
  blindly.

## UI rules (the no-JS contract)

- No `<script>` of our own, no Turbo/Stimulus/data-controller markup. The
  only JS shipped is the Topcoat runtime and the verbatim
  `assets/js/service_worker.js` (served at `/service-worker[.js]`).
- Per-page UI state goes in **additive GET params**, never cookies:
  `?confirm=<form-id>` (delete dialogs), `?reply_to=` (composer prefill),
  `?before=` (history paging). Params are self-clearing and don't bleed
  across tabs. Cookies are only for persistent prefs (`topcamp_theme`,
  via `POST /account/theme`).
- Preserve upstream methods, paths, params, statuses, and flash copy.
- Styling is CSS-only: motion lives exclusively inside
  `prefers-reduced-motion: no-preference`; no purple/indigo gradients.
- Cross-document view transitions + `view-transition-name`s make
  server renders feel continuous (Chromium morphs; Safari/Firefox snap —
  graceful, not broken).

## Topcoat pitfalls we already paid for

- `signal()` **panics outside render scope** — derive UI state from
  query params / headers at the route and pass plain values down.
- Hoist dialog `Slot`s **before** `view!`; leaf views return
  `BoxView<'static>`. `view!` bodies are deferred — don't borrow
  across them.
- `cookies(cx)` panics where the cookie layer is absent (e.g. session
  routes) — read the raw `Cookie` header there instead.
- Keep the upstream fixture strings verbatim: `once-campfire-rust`
  references, `http://campfire.test` test vectors, and
  `vectors/topcamp_user_agents.json`. Renames must not touch them.
- Don't import Topcoat UI staging components wholesale (they drag
  Tailwind copies) — vendor the structure (e.g. `alert_dialog`) into
  our CSS instead. See `crates/topcamp-web/src/confirm.rs`.
- Screenshot every visual change (see `docs/shots/capture.py`): two
  real bugs hid in plain sight — the `?confirm=` dialog rendered below
  the fold until `.dialog[open]` got explicit fixed centering, and
  `contain: inline-size` on the composer form collapsed it to zero
  width. Textarea autogrow is `field-sizing: content` + `lh` units
  with a `max-block-size` cap, and only on sound layout.

## Skills

Load the project skill before doing the work, not after:

- `topcoat-nojs` — recipes for zero-JS UI (dialog confirms, param state,
  popover drawer, theme cookie, CSS-only patterns, lifetime rules).
- `topcamp-verify` — the exact verify loop: toolchain, gates, isolated
  DBs, seed/serve.

## Known limits (documented, out of scope)

- WebPush payload encryption (RFC 8291) is not implemented — push POSTs
  carry plaintext JSON and require TLS endpoints.
- Non-modal `<dialog open>` has no focus trap; PWA install and the
  rich-text toggle are unreachable without JS — accepted, not attempted.
