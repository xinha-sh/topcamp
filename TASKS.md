# Topcamp Migration — Task Tracking

Source spec: [Instruction.md](/Users/dirghaprasad/Projects/topcamp/Instruction.md)
Machine-readable mirror: [tasks.json](/Users/dirghaprasad/Projects/topcamp/tasks.json)

Status legend: `[ ]` open · `[x]` done. Checkboxes mirror §56 Definition of Done
plus the §49 migration order. Section refs (e.g. `§11`) point into Instruction.md.

## Phase 1 — Repository + dependency audit (§2, §49 Step 1–2)

- [x] Trace main request lifecycle (§2.3)
- [x] Trace authentication / session handling (§2.4)
- [x] Trace message creation (§2.5)
- [x] Trace Cable / WebSocket handling (§2.6)
- [x] Trace jobs (§2.7)
- [x] Trace database writes and events (§2.8)
- [x] Trace search (§2.9)
- [x] Trace storage / uploads (§2.10)
- [x] Trace rich text (§2.11)
- [x] Trace external integrations (§2.12)
- [x] Identify existing tests and behavioral guarantees (§2.13)
- [x] Trace room-type controllers + qr_code (supplemental domain coverage)
- [x] Inspect actual Topcoat source/API and pin version (§2, §52)
- [x] Inspect actual DBOS Transact Rust source/API and pin version (§17, §52)
- [x] Inspect actual RustFS / S3-compatible API and pin version (§2, §52)
- [x] Inspect actual Tokio WebSocket ecosystem and pin version (§2, §52)

## Phase 2 — MIGRATION_NOTES.md (§2, §49 Step 1)

- [x] Current architecture
- [x] Target architecture (§3)
- [x] Dependency versions (pinned from actual sources, §2, §52)
- [x] Crate migration map (§43)
- [x] Deleted abstractions (§1, §53)
- [x] Behavioral compatibility decisions (§36, §54)
- [x] Known limitations

## Phase 3 — Topcoat-native architecture (§4–§10, §29–§30, §45, §49 Step 7–8)

- [x] Topcoat is the primary HTTP framework (§56)
- [x] Axum removed from application architecture (§6, §56)
- [x] Topcoat routing (incl. nested routes, path/query params) used (§4, §56)
- [x] Topcoat request/response primitives used (§4, §56)
- [x] Topcoat/Tower layers used appropriately (§29, §56)
- [x] Redundant HTTP abstractions removed (controllers, wrappers, middleware, route/form/query/path parsing, §49 Step 8, §56)
- [x] Application boundary: HTTP does not leak into domain; domain does not depend on Topcoat (§7, §44)
- [x] Auth via Topcoat layer/context → session → user → use case → authorization (§8)
  - Done 2026-09-29: `POST /login` (bcrypt verify, dummy-digest timing shield, session start + 303, 401 re-render with banner); `current_user` (token → live session → active user, hourly resume via `session_resume_due`); logout destroys row. Evidence: 4 auth unit + 3 web live tests, 12/12 db live green, browser E2E (login → cookie → authed search 200; wrong password → banner).
- [x] Sessions/cookies via Topcoat primitives, persisted in PostgreSQL (§9)
- [x] Pages/views via Topcoat page/component model; HTML semantics, forms, links, redirects, CSRF, errors, pagination preserved (§10)
  - Done 2026-09-29: Topcoat UI theme (Geist + Tailwind, dark tokens), shared document shell, login/landing/404 pages; double-submit CSRF (`csrf_token` cookie + `authenticity_token` field, constant-time verify, 403 on mismatch) on login/logout forms; redirects (303), errors (401 banner, 403, styled 404); pagination in list/search routes. Evidence: 24 web unit + 5 web live tests green, browser E2E (login → landing → logout; tampered token → 403).

## Phase 4 — PostgreSQL + full-text search (§11–§15, §49 Step 3–4)

- [x] SQLite removed; PostgreSQL is source of truth (§11, §56)
- [x] Schema designed natively (types, keys, constraints, indexes, cascading, locking, §11, §35)
- [x] Focused repositories (no giant DatabaseService, §12)
- [x] Transactions correct; message creation atomic (§13, §56)
- [x] Post-commit durable scheduling (outbox, no pre-commit side effects, §14)
- [x] Migrations work from a clean database (§35, §56)
- [x] SQLite FTS5 removed; PostgreSQL FTS (tsvector/tsquery/GIN/ranking) implemented (§15, §56)
- [x] Search behavior preserved; unicode/punctuation/malformed/empty/AND/ranking/pagination/snippets tested (§15, §37)

## Phase 5 — DBOS durable workflows (§16–§19, §27–§28, §46, §49 Step 6)

- [x] Old job system removed (§28, §56)
- [x] Durable jobs are DBOS workflows, used only where durability is valuable (§16, §56)
- [x] Workflows retry-safe, deterministic, idempotent (§18, §56)
  - Done 2026-09-29: 12 live tests (`topcamp-workflows/tests/live.rs`) execute all 5 workflows on a real DBOS executor: purge rerun no-op + same-ID rejoin, webhook reply exactly-once under redelivery, variant digest reuse, moderation rerun empty, relay claim/dispatch/done + release-on-failure. Deterministic `outbox:{id}` workflow IDs, claim expiry (5 min), `webhook_deliveries` idempotency keys.
- [x] Only real DBOS Rust APIs used; gaps designed around, never fabricated (§17, §52)
- [x] Synchronous request path stays sync (respond after commit, schedule durable work after, §19)
- [x] Integrations (webhooks/email/push/third-party) moved onto DBOS with idempotent side effects (§27, §56)
  - Done 2026-09-29: `DeliverWebhook` (real POST, 7s/60s/100MB policies, timeout branch, idempotent reply) + `SendNotification` (real POST, per-endpoint outcomes) live-tested vs stub HTTP. Email: no upstream mailer exists per audit (§28 re-evaluation — nothing to migrate). Third-party/external callbacks ride the same reqwest POST path. Known gap: push payload encryption (RFC 8291) — plaintext POSTs recorded honestly per-endpoint.
- [x] Every old job re-evaluated (keep/sync/workflow/merge/delete, §28)

## Phase 6 — RustFS + Cable + verify + publish (§20–§26, §31–§44, §47–§48, §49 Step 5, 9–11, §55–§57)

- [x] RustFS is the blob store via small BlobStore abstraction (put/get/head/delete/presign, §20, §56)
  - Done 2026-09-29: new `topcamp-storage` crate — `BlobStore` trait + `S3BlobStore` (rust-s3, path-style, 5-min presign). Evidence: 5/5 contract tests vs live RustFS (roundtrip, head facts, idempotent delete, presigned URL shape, missing-key None).
- [x] Blob metadata in PostgreSQL (`AttachmentRepository`: insert/find/list/attach/variant/remove + 4 column tests, §21, §56)
- [x] Blob bytes in RustFS (live RustFS §20, §56)
  - Done 2026-09-29: bytes flow through `S3BlobStore` to the live `topcamp` bucket (put/get verified in contract tests). Purge/attachment workflow wiring closed with §22 (same date).
- [x] Attachment processing durable via DBOS; originals survive processing failure (§22, §56)
  - Done 2026-09-29: `ProcessAttachment` = analyze (size-verify, sniff, metadata merge) → derive (1200x800 PNG thumb, digest-idempotent) → finalize (`processed` + `attachment_ready` outbox). Purge collects keys → deletes rows → deletes objects via `S3BlobStore`. Originals proven intact for PNG + junk inputs; GIF/WebP analyzed but skipped (no decoder wired — documented, not faked).
- [x] Action Cable protocol frames + stream naming preserved (`topcamp-cable` codec, §24, §56)
- [x] `/cable` endpoint mount + socket/pubsub fanout over Tokio (§24, §56)
- [x] Cable uses new application/database layer, not SQLite models (§25)
- [x] Realtime broadcasts direct on commit; DBOS only for durable-then-broadcast (§26)
- [x] Error model coherent across domain/application/infrastructure/HTTP; no SQL leaks (§31)
- [x] Config documented (.env.example, DATABASE_URL/RUSTFS_*, no secrets, §33)
- [x] Local dev reproducible (Postgres + RustFS compose, startup/test docs, §34)
- [x] Observability (structured logs with request/workflow IDs; no secret/message logging, §39)
- [x] Rust quality gates (no unsafe/unwrap-on-runtime-paths/giant fns/utils dumps; explicit errors, §41)
- [x] Dependencies justified; no compensating frameworks/ORMs/job layers (§42)
- [x] `cargo check` passes (§50, §56)
- [x] `cargo test` + integration tests pass; behavioral regressions fixed (§37–§38, §50, §56)
- [x] Final architecture review passes incl. "actually simpler than original" (§55)
- [x] Publishable artifacts prepared (this file + tasks.json; §56 evidence)
