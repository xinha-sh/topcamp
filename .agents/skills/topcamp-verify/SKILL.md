---
name: topcamp-verify
description: Verify the topcamp workspace — toolchain, fmt/clippy/test gates, isolated test databases, seed and serve.
---

# Verify Topcamp

Run this loop after any change. Every gate must be green before the
work is called done.

## 1. Toolchain first

Topcoat 0.9 needs rustc ≥ 1.98. Homebrew cargo (1.96) is too old:

```sh
cargo --version   # expect ≥ 1.98 from rustup stable first on PATH
```

If the version surprises you, fix `PATH` before anything else —
mysterious build failures below are usually this.

## 2. Services up

```sh
docker compose up -d   # PostgreSQL 17 + RustFS
```

Dev default (never test against it):
`postgres://topcamp:topcamp@localhost:5432/topcamp`.

## 3. Gates

```sh
cargo fmt --check
cargo clippy --all-targets   # zero warnings tolerated
cargo test --workspace
```

`cargo test` needs no prebuilt bundle (tests self-bundle web assets);
`serve` does: `topcoat asset bundle --bin serve -p topcamp-web`.

## 4. Database hygiene (non-negotiable)

- Each live suite uses its own `topcamp_test_*` database — the header
  of each file under `crates/topcamp-web/tests/` names it.
- Flow per suite: `CREATE DATABASE <name>;` → apply
  `migrations/*.sql` in order (`psql "$DATABASE_URL" -v
  ON_ERROR_STOP=1 -f`) → run → **drop the probe database after**.
- Never run tests against the shared dev database. Fresh probe DBs are
  cheap; debugging pollution is not.
- If a live test fails on stale-state errors (missing rows, other
  suites' debris), suspect a polluted DB first and rerun on a fresh probe
  before touching code.
- `cargo run -p topcamp-web --bin seed` writes demo data — always point
  `DATABASE_URL` at a scratch database first.

## 5. Serve smoke

```sh
PORT=3000 cargo run -p topcamp-web --bin serve    # HTTP + /cable
cargo run -p topcamp-workflows --bin worker       # DBOS + outbox relay
```

## 6. Fixture strings are sacred

Renames must not touch: `once-campfire-rust` references,
`http://campfire.test` test vectors,
`vectors/topcamp_user_agents.json`. If QR/signed-ID/user-agent tests
break after a rename, you edited a fixture — revert it.
