# Migration Notes — Topcamp → Topcoat + DBOS + PostgreSQL + RustFS

Spec: [Instruction.md](/Users/dirghaprasad/Projects/topcamp/Instruction.md)
Tracking: [TASKS.md](/Users/dirghaprasad/Projects/topcamp/TASKS.md) · [tasks.json](/Users/dirghaprasad/Projects/topcamp/tasks.json)

> Status: **audit + design.** Reference: `basecamp/once-campfire-rust` (shallow
> copy held outside this workspace, 2026-09-29). All 13 behavioral traces
> (§2.3–§2.13) recorded; all four dependencies source-inspected and pinned.
> No implementation code exists yet — Phases 3–6 are designs in this file.

## Current architecture

Upstream is a Cargo workspace (`resolver = "3"`, edition 2024, MIT), members
`crates/*`, binary crate `topcamp` (`crates/topcamp/src/main.rs`). HTTP runs on
**Axum 0.8** (ws, multipart) + Tower 0.5 / tower-http 0.6; persistence is
**rusqlite 0.37** (bundled) behind `topcamp_db`; views are **Askama 0.14**
(`topcamp_views` + `templates/`); WebSocket/Cable is custom (`topcamp_cable`,
tokio-tungstenite 0.29). Release profile: fat LTO, single codegen unit,
tikv-jemallocator as global allocator.

Request lifecycle (to be traced): `topcamp/src/main.rs` → `app.rs` (App state) →
`controllers/*` → `topcamp_db` models → `topcamp_views` (Askama) responses;
realtime via `channels/*` + `topcamp_cable`; background work via `jobs.rs`
(+ `integrations/*`: webhooks, web_push pool, opengraph/unfurl, search).

| Crate (package) | Role in upstream |
| --------------- | ---------------- |
| `rails_compat` | Rails primitives: cookies, message verifier/encryptor, key generator, global ID, signed ID, marshal, password, JSON/encoding, turbo, golden files |
| `kit` (`topcamp_kit`) | HTTP kit over Axum: request/response, params, body, cookies, session, ctx, error/exceptions, adapter, server, clock, crypto, testing; `front/` = edge server (handler, conn, TLS, ACME, compression/cache, deflater) |
| `routes` (`topcamp_routes`) | Route table (`src/lib.rs`) |
| `db` (`topcamp_db`) | SQLite persistence: `database.rs`, `schema.rs`, `sql.rs`, `models/*` (account, user, room, membership, message, session, boost, webhook, push_subscription, ban, sound, active_storage, rich_text_record, first_run, search), `events.rs`, fixtures |
| `richtext` (`topcamp_richtext`) | Rich text: content, dom, filters, sanitizer, autolink, attachables, plain_text, uri, ruby; vendored html5ever with parsing fix |
| `storage` (`topcamp_storage`) | ActiveStorage-style blobs on local disk: blob, disk, paths, key, analyze, process, variation, vips, file_server, tables, marshal/verifier |
| `cable` (`topcamp_cable`) | Realtime: protocol, connection, socket, server, channel, pubsub, turbo, naming, json |
| `assets` (`topcamp_assets`) | Asset pipeline (build.rs, vendor/, overrides) |
| `views` (`topcamp_views`) | Askama templates + view models: rooms, messages, accounts, users, sessions, searches, layouts, helpers/*, fragment_cache, pwa |
| `topcamp` (binary) | App wiring: `app.rs`, `config.rs`, `controllers/*` (messages incl. boosts/by_bots, searches, accounts, autocompletable, unfurl_links), `channels/*`, `concerns/*`, `integrations/*` (webhook, web_push pool+encryption+vapid, opengraph, search, jobs, net/http), `jobs/*`, `active_storage.rs`, `rich_text.rs` |

Also present upstream (out of migration scope until audited): `bench/`, `parity/`,
`vectors/`, `reference/`, `reference-tools/`, `plans/`.

## Behavioral traces (audit evidence)

Reference: `/tmp/once-campfire-rust` (upstream shallow copy, 2026-09-29).

### Request lifecycle (§2.3) — traced

`main()` → `app::run()` → `serve()`: front server (`topcamp_kit::front`, the
Thruster replacement; HTTP_PORT/HTTPS_PORT) fronts one Axum `Router` on
TARGET_PORT. Middleware order mirrors Rails: `Rack::Deflater` (kit `deflater`)
→ pre-routing kit middleware (SSL, request id, `_method` override) →
`ActionDispatch::Static` (`public_files` serving `topcamp_assets`, incl.
digested `/assets`, before routing) → `/cable` mount
(`DEFAULT_MOUNT_PATH`) → single Axum catch-all (`/` + `/{*path}`) →
`dispatch_with_fragment_cache` (Rails fragment cache `Scoped`) →
`controllers::dispatch`, which picks the **first** route in `config/routes.rb`
table order whose verb+pattern match (Rails Journey semantics: `:param` =
`[^/.?]+`, `(.:format)` optional suffix; params = path over query over body +
`controller`/`action`). Shutdown: SIGTERM/SIGINT → `cable.restart()`
(`server_restart`, clients reconnect) → 10s grace → jobs shutdown, then Web
Push pool shutdown. CLI: `topcamp [server|backup]`; `backup` = SQLite online
backup into `storage/backups/` via temp-file + rename (ONCE `pre-backup` hook).

`AppState` (shared as `Arc`, reached via `c.app()`/`AppCtx`): config, secrets,
clock, db, storage, cable, broadcasts, jobs, web_push pool (None without VAPID
keys), fragment_cache. Boot side effects: `create_dirs`, `db:prepare`,
`Membership.disconnect_all` (config/puma.rb), job registry (`with_core_jobs` +
`integrations::register_jobs`: `Room::PushMessageJob`, `Bot::WebhookJob`).

### Route table (§2.3, §36) — captured, behavior to preserve

Full URL inventory lives in `topcamp_routes` (`*_path` helpers minus suffix)
and `controllers::routes()` (order-identical to `bin/rails routes`, verified by
tests against `vectors/topcamp_routes.json`). Key surfaces: `/` (root),
`/first_run`, `/session` (+`/new`, `/transfers/{id}`), `/account*` (users, bots,
join_code, logo, custom_styles), `/join/{code}`, `/qr_code/{id}`,
`/users/{id}` (`/avatar`, `/ban`, `/me/*`: sidebar, profile, push_subscriptions),
`/autocompletable/users`, `/rooms*` incl. bot-key JSON scope
(`/{room_id}/{bot_key}/messages*`), opens/closeds/directs namespaces, boosts,
`/rooms/{id}/refresh|settings|involvement|@{message}`, `/messages*`,
`/searches` (+`/clear`), `/unfurl_link`, `/webmanifest`, `/service-worker`,
`/up` (health). NOTE Rails ordering trap, preserved as behavior:
`GET /rooms/opens` matches `rooms#show` with `id="opens"` (resources drawn
before namespace). Undeclared actions answer 404 (`action_not_found`).

### Auth / sessions (§2.4) — traced

Two independent mechanisms, both behavior to preserve (§8–§9, §36):

**1. DB-backed token auth** (`concerns.rs`, `controllers/sessions.rs`,
`db/models/session.rs`). `sessions#create` (rate-limited 10/3min per IP,
fixed window, in-process map; rejection renders `:new` with 401/429 +
`flash.now alert`): `authenticate_by` looks up active user by email on a DB
reader, bcrypt-checks off-reader on the blocking pool (blank password short-
circuits to nil before lookup), then `start_new_session_for` inserts a
`sessions` row (24-char base58 `has_secure_token`, UA + IP recorded) and sets
`signed.permanent[:session_token]` (`httponly`, `SameSite=Lax`, 20y rolling).
`require_authentication` before-action: stores `return_to_after_authenticating`
in the Rails session, redirects to `new_session`; post-login redirects there
else root (`post_authenticating_url` deletes the key). `sessions#destroy`:
destroys push subscription by endpoint, destroys session row, `reset_session`,
deletes cookie, `reset_remote_connections` (errors only logged), redirects to
root. `resume_session`: refreshes `last_active_at`/UA/IP at most hourly
(`ACTIVITY_REFRESH_RATE`); only due sessions take the DB writer; the cookie is
re-signed on the same schedule, not per request. `User.none?` on `sessions#new`
redirects to `first_run`.

**2. Encrypted cookie-store session** (kit `session.rs`, key `_topcamp_session`,
`expire_after: 20.years`, httponly; Rails-readable). Lazy load; cookie written
only when data changed during the request, deleted when only `session_id`
remains; holds flash (`{discard, flashes}` round-trip semantics) + return-to
URL. `sid` = 32 hex chars. `reset_session` drops everything with a fresh sid.

**Chain** (`before_actions`, Rails include order reversed): version headers →
`Current.request` → banned-IP reject (non-GET/HEAD) → `require_authentication`
→ `deny_bots` (403 for bot-key) → CSRF check (skipped for bot-key) → browser
allowlist → controller's own chain. Per-action modifiers:
`allow_unauthenticated_access`, `require_unauthenticated_access` (+ signed-in
redirect to root), `allow_bot_access`, `skip_forgery_protection`.
`Current.user`/`Current.session`/`authenticated_by` (nothing/session/bot_key)
live as `Ctx` extensions.

Migration consequences (§8–§9): the before-action chain maps to a Topcoat
layer/context producing an authenticated user; `require_authentication` →
redirect; `deny_bots`/CSRF/browser rules stay application/domain policy, not
middleware business logic. DB sessions move to PostgreSQL (`SessionRepository`:
start/find_by_token/resume/destroy); cookie-store session maps to Topcoat
cookie primitives with identical key/expiry/Rails-compat crypto.

Small-controller behaviors (supplement): session **transfers** — signed
transfer id → active user → new session + post-auth redirect, else 400
(auto-submitting PUT form, unauthenticated). **First run** — `prevent_repeats`
redirects to root once any account exists; missing name = deliberate NOT NULL
500; email race (`RecordNotUnique`) = silent redirect to root (NOT an error).
**PWA** — manifest JSON + service worker JS at stable URLs, unauthenticated +
CSRF-skipped, exact content types (`application/json`, `text/javascript`,
both `charset=utf-8`). **Welcome** (root) — redirect to last-visited room, or
no-rooms page; rooms-but-no-last-room = deliberate 500.

Accounts/users (supplement): signup requires logged-OUT (`require_
unauthenticated_access` → signed-in users bounce to root) + correct
`join_code` (mismatch → 404; nil account → NoMethodError 500); missing name =
NOT NULL 500; duplicate email (`RecordNotUnique`) → redirect to the LOGIN
page with the email prefilled (contrast first-run's silent root redirect).
New user auto-signed-in → root. Account update is admin-only, permits
`name/logo/settings{}` (arbitrary settings hash), answers redirect + `✓`
notice. `Current.account` dereferenced nil = 500.

Accounts-admin (supplement, all admin-gated unless noted): **bots** —
`create_bot!` (name NOT NULL 500; webhook created for ANY non-nil url incl.
`""`), update writes webhook-then-bot in one tx, destroy = deactivate (NOT
delete); bot-key reset endpoint; lookups restricted to ACTIVE bots (404
otherwise). **users** — paged Turbo-Stream-only list (500/page, other formats
406); role change allowlists to member/administrator (anything else →
member); destroy = deactivate; lookups restricted to ACTIVE users. **bans** — admin-only ban/unban in one
tx each, redirect to the user page. **profiles** (self) — memberships split
direct/shared; update compacts nils, ignores blank password, stages avatar
(nil-explicit deletes, nil-absent leaves); avatar-bearing updates get the
"30 minutes to change everywhere" notice, others `✓`. **sidebar** — room list
for the turbo frame with SIGNED stream names (`rooms`, `<user-gid>:rooms`)
and `can_create_rooms` (admin OR unrestricted). **avatars** — signed token
lookup (bad signature → 404 head, valid-but-gone → 404); square webp variant,
default bot avatar, or initials SVG; ETag (user + template digest) + 30-min
public SWR cache. **push subscriptions** — create dedupes by endpoint attrs
(existing + still valid → touch + 200, invalid → 422; new validated at
creation incl. endpoint resolution, invalid → 422); sign-out destroys by
endpoint (sessions trace). **logo**
— public show (ETag on account, 5-min public + 1-week SWR cache; 192/512 PNG
variants, stock icon fallback) vs admin destroy. **join code** — admin reset
→ redirect. **custom styles** — free-text CSS-ish field, `✓` notice redirect. Sub-resources (account
users/bots, join codes, logos, custom styles, avatars, bans, profiles, push
subscriptions, sidebars) follow the same use-case shape; push-subscription
and ban specifics already traced via sessions/jobs.

Autocomplete/unfurl/refresh/involvement (supplement): **autocomplete** —
room members vs all users by `room_id` (bad id → 404), `filter` (mentions)
vs `query` (pickers) params, active-only, `%LIKE%` on name, `LOWER(name)`
order, 20/page + pagination headers, HTML (`lexxy-prompt-item`, layoutless)
or JSON. **Unfurl endpoint** — blank/missing URL → 400 (`require`), hash/
array URL → 204 (unparsable, rescued), private address → 204, timeout → 204,
valid metadata → JSON. **Refresh** — `since` in MILLISECONDS via Ruby `to_i`
(hash/array → 500, null/missing → 0), clamped to representable range;
answers Turbo Stream with created-since + updated-since (excluding new) sets.
**Involvement update** — blank (`""`/`[]`/whitespace/missing) stores nil,
unknown string → `ArgumentError` 500, then `broadcast_visibility_changes`
(nil previous → `NilInquiry` error); redirects back to the involvement URL.

### Message creation (§2.5) + DB events (§2.8) — traced

`messages#create` (`controllers/messages.rs::create`): before-actions →
`set_room` (gone room → `room_not_found` page, not 404) → strong params
(`message.require.permit(body, attachment, client_message_id)`) →
`create_message` → `broadcast_create` → `deliver_webhooks_to_bots` → 200
`TURBO_STREAM` response rendered from the fragment cache (request-less: no CSRF
tokens in forms). Update/destroy are admin-only (`can_administer?` else 403);
update answers HTML redirect or deliberate "Missing template" internal error
for JSON; destroy → `broadcast_remove`.

`create_message` ordering (behavior to preserve, §13/§19): stage upload +
canonicalize rich-text body (off-writer, on readers) → **one write tx**: staged
blob row (`keep_after_commit` pins the file past commit) + `Message::create`
(row + body `RichTextRecord` + `Attachment` + message/room touches) → commit →
`process_attachment` (analyze → hourly-touch; video → webp preview, image →
1200x800 thumb) → re-read → broadcast + webhook scheduling. `update_message`
replaces attachment without reprocessing (analyze only, via
`ActiveStorage::AnalyzeJob` after commit); old blob purged later.

`Message::create` tx contents: INSERT with client-generated-or-new-UUID
`client_message_id`, body record, attachment row, touches; `after_commit`:
search-index insert + `Room::receive` (unread memberships + push job). Event
model (`db/events.rs`): models emit `Event::{PushMessage, DisconnectUser,
RemoveBannedContent, DeliverWebhook, PurgeBlob}` at Rails-equivalent points;
the `EventSink` (production = `jobs::Jobs` queue) runs on the writer thread
and must only hand work off. This is exactly the §14 target pattern already:
**tx commit → durable scheduling** — migration keeps the boundary, swaps the
sink for outbox + DBOS. Pagination: `before`/`after`/last-page over
`find_in_room`-scoped messages; empty index → 204; ETag/`fresh_when` on
message cache keys + template digest.

### Cable / WebSocket (§2.6) — traced

`topcamp_cable` is a bespoke Action Cable implementation (Axum WS upgrade,
tokio-tungstenite present in workspace deps); app channels live in
`topcamp/src/channels/*`. Wire behavior to preserve (§24, §37):

- Mount `DEFAULT_MOUNT_PATH` = `/cable`, merged into the Axum router before the
  catch-all. Subprotocol negotiation: client's first listed protocol that is in
  `{actioncable-v1-json, actioncable-unsupported}` wins (websocket-driver hybi
  walk). Frames are ActiveSupport-JSON key-ordered hashes.
- Lifecycle frames: `{"type":"welcome"}` on connect; `{"type":"ping","message":
  <unix sec>}` every `BEAT_INTERVAL` = 3s; `{"type":"disconnect","reason":…,
  "reconnect":…}` with reasons `unauthorized|invalid_request|server_restart|
  remote` (reason null when Rails closes bare; `reconnect` passes through
  unvalidated). `confirm_subscription` / `reject_subscription` per identifier;
  broadcast frames `{"identifier":…,"message":…}` with pre-encoded payloads.
- Auth (`SessionAuthenticator`): `ApplicationCable::Connection` reads the
  signed `session_token` cookie from headers, `Session::find_by_token` →
  user; failure = `reject_unauthorized_connection`. `identified_by
  :current_user` (id + name); `connection_identifier` = user GlobalID
  (`gid://topcamp/User/<id>`), which `remote_connections.where(current_user:)`
  matches for `DisconnectUser` revocation (reconnect true on sign-out/
  membership-destroy, false on deactivate/ban).
- Channels (registered under Ruby class names): `HeartbeatChannel`,
  `PresenceChannel`, `ReadRoomsChannel`, `RoomChannel`, `RoomMessagesChannel`
  (signed-stream guarded), `TypingNotificationsChannel`, `UnreadRoomsChannel`,
  `Turbo::StreamsChannel` (signed stream names via `Turbo.signed_stream_verifier`
  + `RoomStreamsAreAuthorized`). `on_subscribe` runs after `subscribed`, before
  confirmation; Ruby-public-method actions (incl. redefined `subscribed`) are
  performable.
- Broadcasts (`Broadcasts`): stream names are GID params (`<room gid
  param>:messages`, STI-aware: `gid://topcamp/Rooms::Open/1`), per-user
  streams (`user_<id>_reads/unreads/rooms`); targets are `dom_id`s (rooms keyed
  by STI param key, messages by `client_message_id` → `message_<uuid>`).
  Actions append/replace/remove/prepend with turbo-stream markup; e.g.
  message create = append to room stream + `{roomId}` to each member's unreads;
  update = replace `[message, :presentation]` with `maintain_scroll`; even the
  Rails `nil.inquiry` NoMethodError on involvement update is reproduced as
  `Err(NilInquiry)`.
- Shutdown interplay: `cable.restart()` (`server_restart`) precedes the 10s
  drain so sockets never hold graceful shutdown open.

Migration consequences (§24–§26, §48): keep this crate's protocol/fanout
ownership intact; rewire its `Deps` (db/auth) to the new PostgreSQL
application layer and its mount to Topcoat's connection boundary. Broadcasts
stay synchronous post-commit; only durable-then-notify flows go through DBOS.

Channel behaviors (supplement): `PresenceChannel` marks membership
connected on subscribe/present, `disconnected` on unsubscribe, refreshes on
`refresh`, and tells the user's OTHER windows the room was read
(`user_<id>_reads`); gone membership → nil NoMethodError equivalents.
`TypingNotificationsChannel` broadcasts `{action: start|stop, user: {id,
name}}` to the room GID stream. `ReadRooms/UnreadRoomsChannels` are pure
per-user streams (`user_<id>_reads/_unreads`). Revocation: per-connection
`disconnect` with `reason: remote`; `reconnect: true` (membership loss,
sign-out — client replays subscriptions, channels turn away lost rooms) vs
`false` (deactivate/ban after commit — reconnect refused); closing runs
unsubscribe callbacks (presence `absent`). Cable-side session re-check guards
the Rails race where a just-authenticating connection misses an in-tx
disconnect.

Transport integration note (§24): `Server` exposes an Axum-shaped surface
(`router::<S>() -> axum::Router<S>`, `call(Request) -> Response`) and the
socket layer is hand-rolled (own handshake/deflate negotiation, reader/
writer, ping/pong/close) — NOT tungstenite at the wire level (tungstenite
is only a workspace dep). The Topcoat `/cable` mount therefore needs a
Topcoat-native entry (`connection.rs` integration): keep protocol/socket/
pubsub/channel machinery untouched, replace ONLY the Axum adapter edge.
`Config { assume_ssl }`, `restart()` (server_restart), `stream_count()`
(observability), and per-stream broadcast fanout stay as-is.

### Jobs (§2.7) — traced

In-process runner replacing Resque (`topcamp/src/jobs.rs`, 301 lines). `Jobs`
is the DB `EventSink`: each event maps to a `JobKind`
(PushMessage/DeliverWebhook/RemoveBannedContent/PurgeBlob/AdHoc;
`DisconnectUser` is NOT a job — it goes straight to the cable server as a
synchronous broadcast). Each kind has its own bounded mpsc queue (capacity
1024; full → drop + error log, never blocks the writer thread) with
`concurrency` workers (`config.job_concurrency`). `perform_later` enqueues
ad-hoc futures (used for `ActiveStorage::AnalyzeJob`). **No retries**
(`retry_on` commented out upstream); failure/panic only logged; queued work is
lost on crash (accepted upstream). Shutdown: stop intake, drain queues, abandon
past deadline with a warning.

Handlers: core registers `RemoveBannedContent` (destroys each user message in
its own tx + `broadcast_remove`) and `PurgeBlob`; integrations register
`PushMessage` (`Room::PushMessageJob`: Web Push via pool, skipped when VAPID
unconfigured) and `DeliverWebhook` (`Bot::WebhookJob`: POST payload, then
creates the bot's reply message — text canonicalized, or attachment staged +
processed — and `broadcast_create`; nil webhook = recorded error).

Migration consequences (§16–§19, §28, §46): every one of these is a DBOS
workflow candidate (external side effects + crash-recovery value), EXCEPT the
fire-and-forget `AdHoc` analyze (evaluate: DBOS vs inline). DBOS adds what the
current system deliberately lacks: retries, idempotency keys, survival across
restarts. `DisconnectUser` stays out of DBOS (realtime broadcast, §26).
Per-job disposition for §28: PushMessage → `SendNotification`-style workflow;
DeliverWebhook → `DeliverWebhook` workflow (idempotent reply creation!);
RemoveBannedContent → workflow or retained job; PurgeBlob → workflow;
AnalyzeJob → workflow or sync step of attachment pipeline (§22).

Disposition record (implemented in `crates/topcamp-workflows`):

| Old job | Disposition | Artifact | Note |
| ------- | ----------- | -------- | ---- |
| `Room::PushMessageJob` | workflow | `notifications::send_notification` | DB steps wired; HTTP sends pending TLS/VAPID client |
| `Bot::WebhookJob` | workflow | `webhooks::deliver_webhook` | Idempotency key `webhook-delivery:{message_id}`; POST pending TLS client |
| `RemoveBannedContentJob` | workflow | `moderation::remove_banned_content` | List + destroy-chunk steps wired; `broadcast_remove` pending Cable |
| `PurgeBlob` | workflow | `purge::purge_blob` | Row deletes wired; object deletes pending BlobStore |
| `ActiveStorage::AnalyzeJob` (+ ad-hoc `perform_later`) | workflow | `attachments::process_attachment` | Pending BlobStore + media pipeline |
| `DisconnectUser` | sync broadcast, NOT a job/workflow | pending `/cable` mount | Realtime revocation per §26; never enters the outbox |

The in-process runner itself (bounded mpsc queues, drop-on-full, no
retries) is deleted, not ported: no queues, no spawns, no `EventSink`
exist anywhere in this workspace.

### Search (§2.9) — traced

Two halves, both behavior to preserve (§15, §37):

**Query path** (`searches_controller` + `integrations/search.rs` + `Message::
search_*`). `q` param: non-string raises (`gsub` NoMethodError → 500); string
is sanitized by replacing every non-`[[:word:]]` char with a space — Onigmo
Unicode-15 word table (alpha, marks, digits, connector punct, join controls;
probed against reference Ruby; `café_1 日本` survives, `hello, world!` →
`hello  world `). `set_messages` runs before EVERY action (index/create/clear):
blank/absent query → empty results (no error). Non-blank → `search_reachable`
= user's reachable messages (membership join), `MATCH`, `created_at DESC LIMIT
100`, then reversed to ascending. Per-room variant orders ASC directly. Index
table `message_search_index(rowid, body)` holds **plain-text** bodies
(`plain_text_body` via rich-text lib). `match_terms`: whitespace/NUL-split,
each word double-quoted with `"` escaped (AND semantics, no query syntax, no
`NOT/AND/OR/NEAR` injection — but a query that is only non-word chars
sanitizes to spaces → `match_terms` empty → empty results, NOT an error).

**History path** (`Search` model / `searches` table): `create` records the raw
sanitized query (`find_or_create_by` + touch; nil query → deliberate NOT NULL
500) and redirects to `search_path(query)`; `clear` destroys all of the user's;
create trims to 10 most recent. Recent queries (most-recent-first) render on
the index page.

Migration consequences (§15): replace `message_search_index` FTS5 with
`tsvector` + GIN (+ ranking; upstream has NO ranking — message order only —
so ranking is new behavior to design, §15 lists it as required). Verified
live 2026-09-29 on PG 17: `plainto_tsquery` reproduces the AND semantics of
`match_terms` (`hello unrelated` → 0 rows when no message has both), and
`ts_rank` orders by term frequency (3×`world` message ranks above 1×).
`english` config confirmed as the FTS5-porter replacement (stemmer deltas go
in tests). Test matrix
from evidence: unicode (`café_1 日本`), punctuation, whitespace-only, NUL,
empty/missing `q`, non-string `q` (500), AND semantics, 100-result cap +
reversal, 10-recent trim, per-room vs reachable scoping, index sync on
create/update/destroy (after_commit).

Boosts + bot API (supplement): boosts scoped to reachable messages AND own
booster (`find_by!` else 404); nil content = deliberate NOT NULL 500;
create → redirect to boosts index, destroy → 204, dangling show/edit/update
routes → 404 (`action_not_found`). Bot API (`/:bot_key` scope, JSON default,
bots allowed): raw POST body as message body (lossy UTF-8) or top-level
multipart attachment; empty both → 422; create → 201 + `Location` (global
message URL, not room-scoped); update → JSON show or HTML redirect; destroy →
204; room miss → 404 (NOT the root+alert of HTML `set_room`). Index carries
`X-Total-Count` + RFC `Link: rel="next"` (before/after-aware) with the bot-key
URL preserved.

### Storage / uploads (§2.10) — traced

ActiveStorage-style system over a local disk service (`topcamp_storage` +
`topcamp/src/active_storage.rs`, 685 lines). Key design (behavior to
preserve, §20–§22):

- **Staged-blob protocol**: file work (copy, checksum, identify) happens
  off-writer and yields a `Staged` blob whose file is already uploaded; the row
  insert runs inside the tx; `keep_after_commit` pins the file on commit while
  `Drop` deletes it on rollback — so tx abort never orphans bytes. Transfers:
  `stage_file`/`stage_bytes` (`identify: true`), `create_and_upload!`.
- **Blob records**: key (checksum-based content addressing), filename,
  content type (declared + Marcel-style identification), byte size, checksum,
  metadata JSON (analysis merges `analyzed: true`), service name. Variants
  (`VariantWithRecord`, tracked rows) + video previews (`Preview`) processed
  via libvips/ffmpeg behind a 4-slot semaphore (`MAX_MEDIA_JOBS`); existing
  variant/preview reused (`existing_variant`, `record_variant`).
- **Serving**: redirect-vs-proxy controllers; signed blob ids (purpose
  `blob_id`, NOT the AR signed-id verifier), signed variation keys
  (`variation`), signed disk URLs (`blob_key`) via `rails_compat` message
  verifier; bad signature → 404, valid signature + missing row → 404 via
  RecordNotFound. Disk `show` sends `Cache-Control: max-age=3600, public`;
  proxy supports Range requests + `Accept-Ranges: bytes` + 100-year
  `http_cache_forever` freshness. Service URLs expire in 5 min.
  Downloads are public behind signed URLs; disk PUT + direct uploads require a
  Topcamp session. These controllers skip all Topcamp concerns (own
  `BaseController` + CSRF only).
- **Purge**: `dependent: :purge_later` → `PurgeBlob` event → purge job deletes
  row + files (variant/preview cascade).

Migration consequences (§20–§22, §47): bytes move to RustFS behind a
`BlobStore` (`put/get/head/delete/presign_get/presign_put`); the
stage→insert→keep protocol maps directly onto multipart/presigned PUT +
metadata commit; signed-URL purposes and expiry semantics must be re-expressed
with S3 presigning (5-min service URLs, 100-year cache headers preserved at
the HTTP layer). Metadata rows → PostgreSQL (`AttachmentRepository`). Disk
paths/keys become object keys; checksum verification stays. Variant/preview
processing becomes the DBOS attachment workflow (§22); the 4-slot semaphore
becomes workflow concurrency control.

### Rich text (§2.11) — traced

Self-contained Action Text pipeline (`topcamp_richtext`, ~3.3k lines, no web/
DB/DBOS deps — already satisfies §23's independence rule). Public API:
`Content::{load, wrap, to_html, to_plain_text, render,
to_rendered_html_with_layout}`, `message_presentation` /
`present_message` (rescue → empty string; unloggable → `Unrenderable`),
`to_plain_text` (feeds FTS index, push bodies, webhook plain bodies, emoji
detection, `/play`), `editable_value` (Lexxy editor round-trip; `None` when
blank), `mentioned_users` (verified-SGID user attachables, deduped),
`without_recipient_mentions`. Pipeline: vendored html5ever (parsing fix) →
own Loofah-exact allowlist sanitizer (unwrap-not-drop, foreign-drop-with-
contents, URI protocol checks, Nokogiri-identical serialization for auto_link
regexes) → content filters → autolink → attachment rendering (attachables,
galleries, custom attachments). Rails is the oracle throughout, including
Nokogiri serialize/parse round-trip shapes and Ruby-raised-error emulation
(`Error::{Parse, Raised, Unrenderable}`).

App glue (`topcamp/src/rich_text.rs`, 57 lines): `AppRichText` implements the
db `RichText` trait on the caller's own connection (never checks out another —
would deadlock the pool); render failures log and fall back to tag-stripped
text (save must not fail), mention failures log and yield empty. Canonicalize-
on-assign happens off-writer on readers (`canonicalize_body`).

Migration consequences (§23): keep as a portable domain library; only
simplify internals if genuinely simplifiable — the oracle-fidelity comments
warn against "cleanup" that changes output. DB glue trait stays but binds to
PostgreSQL connections.

### Views layer (supplement, §10)

- **Fragment cache** (`views/fragment_cache.rs`, ≈500 lines): `Rails.cache`
  equivalent — size-bounded store (default 32MB, `TOPCAMP_FRAGMENT_CACHE_MB`),
  request-scoped via `Scoped` future wrapper + thread-local `with/current`,
  keyed render-or-fetch (`message` + `presentation-v3` keys). Broadcasts and
  controller responses share it; detached rendering (`render_detached_at`)
  produces request-less HTML (no CSRF tokens) for cable payloads. Migration:
  keep ONLY with measured hit value (§40); the request-less render path is
  required regardless (broadcasts need HTML without a request).
- **JSON contract** (`views/messages/json.rs`): exact Jbuilder field shapes —
  `UserJson{id,name,role,avatar_url}`, `MessageJson{id,created_at(UTC string),
  body{plain_text,html},creator,room,url}`, `BoostJson{...}`; key order =
  Jbuilder emission order via `to_rails_json`. This is the BOT API wire
  contract — byte-shape behavior, test with golden JSON.
- **Presenters** (`controllers/presenters/*`): row→view-model mapping on a
  reader connection (`Presenter`, `DbResolver`, attachments, pagination
  `Page`, `view_context` page-or-frame). Migration: presenters become
  Topcoat page/component view models; the `page.rs` helpers (framed/bare/
  content-in-layout, `db_error` mapping) dissolve into the error/layer design.

### Integrations (§2.12) — traced

Own HTTP/1.1 stack over hyper+rustls (`integrations/net`: injectable
Resolver/Dialer/TLS `Network`, `Net::HTTP`-like exchange, no proxies).
Three clients, three policies (behavior to preserve, §27):

- **Bot webhooks** (`webhook.rs`, deliberately UNGUARDED — admin-set URLs may
  target internal services): POST JSON payload, 7s connect/read timeouts, 60s
  total deadline, ≤100MB reply; timeouts become a "Failed to respond within N
  seconds" text reply; bad URL/connection errors/unparseable MIME fail the
  delivery (job fails, no retry upstream). Reply → bot message (text or staged
  attachment) + broadcast.
- **Opengraph unfurl** (user-facing, GUARDED): every address resolved through
  the private-network guard (Surfguard default policy) and pinned; redirects
  re-checked (≤10, http(s) only); documents ≤5MB `text/html` 200 only; 10s
  total deadline, 5s connect/read; ≤16 concurrent unfurls, ≤4 concurrent
  parses off-runtime; deadline expiry → 204 No Content (not an error).
  Missing/blank `url` → controller 400.
- **Web Push** (`web_push/*`): VAPID (P-256 ECDH/ECDSA, HKDF) + payload
  encryption; pool of 50 delivery slots / 10k queue (overflow dropped +
  logged, `RejectedExecutionError` semantics); invalid-subscription handler
  destroys the subscription on its own thread; pool absent entirely without
  valid VAPID keys (`vapid_public_key` None → browsers never subscribe).
  Payloads built from plain-text bodies with recipient-mention stripping.

Migration consequences (§27, §46): all three are textbook DBOS workflows
(durable, retried, idempotent): `DeliverWebhook` (webhook+reply creation),
`SendNotification` (push), `ProcessIntegration`/`GenerateDerivedAsset`
(unfurl+metadata). Timeouts/deadlines/concurrency caps become workflow
policies; the SSRF guard stays as a domain-level precondition inside the
workflow (never middleware). VAPID keys → `WEB_PUSH` config (§33).

### Tests / behavioral guarantees (§2.13) — surveyed

Three guarantee layers, all relevant to §37–§38:

- **Golden vectors** (`vectors/*.json`, never hand-edited): generated by the
  Rails reference running in production mode in the `topcamp-reference`
  Docker image, fixed `SECRET_KEY_BASE` + frozen clock
  (2026-01-01T12:00:00Z). Cover routes (`topcamp_routes.json` — the route
  table must stay identical to `bin/rails routes`), sessions, user agents,
  rails_compat crypto, storage. Rust tests read them; some round-trip back
  through Rails verification scripts (`reference-tools/`).
- **Parity harness** (`parity/`, Playwright + TS): browser-level capture/
  compare/diff against the reference (screens inventory, determinism checks,
  coverage of templates). This is the externally-observable-behavior oracle —
  the migration's compatibility tests (§38) should target this layer, not
  unit internals.
- **Crate tests**: db model tests per model + `differential_test`,
  `callbacks_test`, fixtures from the reference (`test-support` feature,
  `fixtures.rs`); cable golden + protocol tests; kit http/front + rails
  vectors; richtext corpus tests; views tests; controller tests via `TestApp`
  boot (`presenters::test_support`), e.g. search record/clear flows. Oracle
  scripts for integrations live in `testdata/oracle` (Ruby run against the
  reference).
- `reference/` is EMPTY in the shallow copy (submodule uninitialized —
  `.gitmodules` exists); `bench/` holds loadgen + attribution docs
  (`plans/perf-attribution.md` cited for zlib-rs choice, LTO, jemalloc).

Migration consequences (§37–§38): preserve/adapt behavioral tests (parity +
vectors + controller flows), replace implementation-detail assertions with
observable-behavior equivalents. NOTE: full `cargo test` verification was not
run here (reference copy is for audit, not a build tree); the migration must
establish green `cargo check`/`cargo test` per phase (§50). Bench/loadgen +
perf-attribution feed §40 (pooling, pagination, fanout, workflow overhead).

Test strategy (§37, from `test_support.rs` evidence): the current harness
boots the WHOLE app over a private copy of the reference-built parity seed
(SQLite + files), signs in with a Rails-issued session cookie, and drives
requests through the router (`Browser` with cookie jar + CSRF handling;
skips gracefully when the seed isn't built). Named fixture IDs (David/Jason/
rooms) make behavioral assertions stable. Migration port: same shape against
PG + RustFS — boot app on migrated seed copy, drive Topcoat router, keep the
Browser/CSRF/seed-skip pattern; replace SQLite seed with `pg_dump`-based
seed + bucket fixture sync. Unit layers stay as today: model tests per
repository, cable golden/protocol tests, richtext corpus, vector tests
(routes/sessions/crypto/storage goldens). New suites: FTS matrix (§15),
outbox claim/dispatch semantics, DBOS workflow retry/idempotency (against
real PG sysdb), BlobStore contract (put/get/head/delete/presign/failures
against real RustFS).

## PostgreSQL schema plan (Phase 3 design, §11, §35)

Derived from `schema.sql` (15 tables + FTS5 + 2 Rails-internal). All queries
name columns, so column order is free; types map as below. `id` stays
`BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY` everywhere (Rust code uses
i64 throughout — no UUID/ID redesign).

| SQLite (Rails) | PostgreSQL |
| -------------- | ---------- |
| `integer PK AUTOINCREMENT` | `BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY` |
| `datetime(6) NOT NULL` | `TIMESTAMPTZ NOT NULL` (UTC; Rails 8 is tz-aware) |
| `varchar` / `text` | `TEXT` (keep `VARCHAR(16)` CHECK for `boosts.content` parity) |
| `json` (`accounts.settings`) | `JSONB` |
| `integer` role/status (`users`), STI `type` (`rooms`), `involvement` default `'mentions'` | keep integer/text codes + `CHECK` constraints (Rails values are behavior); PG enums rejected — codes are compared in app logic |
| `bigint` FKs | `BIGINT NOT NULL REFERENCES …` — graph transcribed 2026-09-29 (no `ON DELETE` actions anywhere upstream: all FKs are plain `NO ACTION`; destruction cascades in code, e.g. `message.destroy`) |

Constraints/indexes to reproduce 1:1: `sessions.token` UNIQUE,
`users.email_address` + `bot_token` UNIQUE, `accounts.singleton_guard`
UNIQUE, `active_storage_blobs.key` UNIQUE,
`memberships(room_id,user_id)` UNIQUE, `action_text_rich_texts
(record_type,record_id,name)` UNIQUE, `active_storage_attachments
(record_type,record_id,name,blob_id)` UNIQUE, `active_storage_variant_records
(blob_id,variation_digest)` UNIQUE; all single/multi-column btree indexes
listed in `schema.sql` **plus** the app-added `(room_id, created_at)` paging
index (60ms→0.02ms at 236k rows — load-bearing, §40). `messages.
client_message_id` has NO unique constraint upstream — do not add one.

FTS (§15): drop `message_search_index` FTS5 (porter tokenizer — note: PG
`english` stemmer differs; behavior deltas documented in tests); add
`messages.search_vector TSVECTOR` maintained by insert/update triggers from
plain-text bodies + GIN index; queries via `plainto_tsquery`/`phraseto_tsquery`
with `ts_rank` ranking (NEW behavior — upstream is message-order only).
Snippet shape: `ts_headline` where the app showed snippets.

Migrations (§35): fresh numbered sequence replacing all 15 Rails migrations +
`ADDITIONS`; `schema_migrations`/`ar_internal_metadata` dropped (no
Rails-compat boot checks in the new tree). `migrations/0001_init.sql`
WRITTEN and machine-checked against `schema.sql`: 15/15 domain tables, 16/16
plain indexes (+1 new GIN on `search_vector`, by design), 8/8 unique
constraints, 10/10 FKs. NOT YET RUN: no live PostgreSQL in this environment
(`cargo`/crate fetches also unreachable — crates.io proxy timeout; Docker
daemon down (OrbStack socket absent), so no scratch PG either. The
`migrations work from a clean database` gate (§56) stays open until a build
workspace with PG exists. Self-review 2026-09-29 caught and fixed one defect:
`memberships_involvement_check` omitted `'nothing'` (enum has four variants:
invisible/nothing/mentions/everything). `rooms.type` (3 STI names) and
role/status ints intentionally unconstrained. `migrations/0002_outbox.sql` adds the transactional
outbox (`push_message/deliver_webhook/remove_banned_content/purge_blob`
topics, `SKIP LOCKED` claim index) and records the app-maintained
`search_vector` decision (no trigger — canonicalization stays in the richtext
lib, mirroring upstream explicit indexing). OPEN DECISION: fresh
database vs SQLite→PG data import — upstream reads the Rails app's live
SQLite file; if the migration must carry existing data, write an explicit
offline import (per-table copy in FK order + sequence restart + FTS
backfill), never a live dual-write.

FK graph (`schema.sql`, transcribed verbatim — 10 FKs, zero `ON DELETE`
actions): `active_storage_attachments.blob_id→blobs`,
`variant_records.blob_id→blobs`, `bans/messages.creator_id/
push_subscriptions/searches/sessions/webhooks.user_id→users`,
`boosts.message_id→messages`, `messages.room_id→rooms`. NOTABLE ABSENCES
(preserve!): no FK on `boosts.booster_id`, none on `memberships`
(room/user), none on polymorphic `record_type/record_id` pairs — the app
enforces these in code. PG migrations reproduce exactly this: plain
`REFERENCES` with no cascade actions, same missing FKs.

DB concurrency → PG pooling (§40): upstream runs ONE writer thread
(`BEGIN IMMEDIATE`, bounded queue, panic-isolated) + N reader connections
(default 8, `db_readers`) + a dedicated WAL-checkpointer thread, with
`after_commit` hooks run in order off-tx. Deadlock rule: never check out a
second connection while holding one (richtext glue documents this). PG
mapping: single-writer serialization is SQLite-specific and GOES AWAY —
PostgreSQL MVCC handles concurrent writers; repositories take transactions
from an sqlx pool (sized: readers ≈ upstream count, writes unbounded by a
queue); `after_commit` → outbox insert in-tx + relay (§14 design above). The
`Timestamp`/`Clock`/`Env` seams survive (frozen-time tests); `WriterGone` and
busy-timeout handling disappear with the writer thread.

Repositories (§12, from models): UserRepository, RoomRepository,
MembershipRepository, MessageRepository (+ rich-text/attachment writes inside
its tx), SessionRepository, AttachmentRepository (blob+variant records),
WebhookRepository, SubscriptionRepository (push), SearchRepository (history),
BoostRepository, BanRepository, AccountRepository. `DatabaseService` god-type
explicitly rejected. `Timestamp`/`Clock` abstractions survive (frozen-time
tests depend on them); `rusqlite`-specific helpers (`query_row_cached`,
`sql::uuid/base58`) are replaced.

## Topcoat application-layer design (Phase 7 plan, §4–§10, §29–§30)

Target request flow (§30): HTTP → Topcoat Router → Topcoat/Tower layers →
authenticated request/context → handler → application use case → domain →
PostgreSQL → (DBOS/Cable as required) → Topcoat response. No giant
controller/service/database objects.

**Router** (`topcoat-router`, replacing both the Axum catch-all AND the
Rails-order `dispatch` table): route groups mirror `topcamp_routes` —
root/first_run, session (+transfers), account (+users/bots/join_code/logo/
custom_styles), join/qr, users (+avatar/ban/me/*), autocompletable, rooms
(+messages/boosts/bot-key JSON scope, opens/closeds/directs, refresh/
settings/involvement/@message), messages, searches (+clear), unfurl_link,
webmanifest/service-worker, `/up`, ActiveStorage redirect/proxy routes.
Rails-order trap (`/rooms/opens` → `rooms#show`) is resolved at ROUTE
DEFINITION time (explicit route precedence in the Router builder), not by a
first-match dispatcher — the Axum catch-all + Journey emulation is deleted,
not ported. Path params (`:room_id`, bot-key globs), query (`q`, pagination),
multipart forms (composer uploads), and the `(.:format)` suffix map to
`path_param.rs`/`query_param.rs`/`urlencoded.rs`/body handling.

**Layers** (replacing kit middleware + `before_actions` chain, §8–§9, §29):
request-id → logging → security headers → compression → static files
(`topcoat-asset`, before routing, as today) → session extraction
(`topcoat-cookie`/`topcoat-session`, `_topcamp_session` semantics) →
authentication (signed `session_token` → `SessionRepository::find_by_token` →
`CurrentUser`/`CurrentSession` in request context; unauthenticated → store
return-to + redirect `new_session`) → bot/CSRF/browser policy (application
policy, per-action modifiers preserved) → error pages (404/422/500/502).
Business rules stay OUT of layers (§29): `deny_bots`, allowlists, and
authorization (`can_administer?`) live in use cases/domain.

**`kit::Ctx` decomposition** (audited 2026-09-29 — the whole struct is the
migration surface): state/clock/now, typed `current<T>` extensions,
params (`param/param_str/wrap_parameters`), session/flash/reset, CSRF verify,
content negotiation (`formats/respond_to/rendered_format`, turbo-frame
detection), renderers (html/render_as/render/turbo_stream/json/head/
redirect family/`url_for`), file sending, freshness (`fresh_when/stale`,
`expires_in/expires_now/no_store`, vary), headers, live-response flag; plus
`request.*` (trusted proxies, remote_ip with spoof check, host/port/url,
UA, raw_post). Every one of these maps to a Topcoat/request primitive or an
explicit use-case dependency — `Ctx` itself is deleted, not wrapped (§45).

**Handlers → use cases**: each controller action becomes a thin handler
(params → use case → response): `CreateMessage` (stage→tx→process→broadcast→
Turbo Stream), `SearchMessages` (sanitize→MATCH→history record), session
create/destroy, room CRUD + membership revise, boosts, accounts/users admin,
PWA/manifest, unfurl (delegates to DBOS-backed workflow or sync guarded
fetch per latency budget). Presenters move to Topcoat pages/components;
Askama templates retained ONLY where they render through the new page model
without the `kit` Ctx plumbing (§10).

**State** (§32): `AppState { config, db (PG pool), blob_store (RustFS),
dbos, cable, clock }` — fragment cache and web-push pool re-evaluated (cache:
keep only with measured hit value; pool: replaced by DBOS + push sender).
No request/user/domain state in global state.

**Errors** (§31): `domain::Error` / `application::Error` / `infra::Error` →
mapped at the web boundary to Topcoat responses; deliberate behaviors
preserved (404 for bad signatures/undeclared actions, 500 for non-string `q`
and nil search record, 403 for admin/bot violations, 204 empty index,
429 rate-limit rendering). No SQL/DBOS internals leak.

## DBOS workflow detailed design (Phase 6 plan, §16–§19, §46)

API used (all verified in `dbos` 0.6.0-dev source): `step("name", || body)`
/ `step_with` + `StepOptions`/`ShouldRetry`, `select_step!` durable race,
workflow `run`/`start` builders, `WorkflowHandle`, `join_workflows`,
named queues (`register_queue`/`enqueue`), `sleep`, events/messaging.
No `@workflow` decorators anywhere — anyone writing one is inventing API.

| Current job | DBOS workflow | Steps & idempotency |
| ----------- | ------------- | ------------------- |
| `Room::PushMessageJob` | `SendNotification { message_id }` | steps: load message+subscriptions → send batch (50-wide, pool semantics) → destroy invalid subs. Idempotency: `(message_id, endpoint)` dedupe. Retry: exponential, drop after N with log (matches today's drop-and-log). Skip when VAPID off — decided at schedule time, not inside. |
| `Bot::WebhookJob` | `DeliverWebhook { bot_id, message_id }` | steps: build payload → POST (7s/60s/100MB policies as `StepOptions` timeouts) → create reply (idempotency key `webhook-delivery:{message_id}` — a retried delivery must NOT double-post the bot reply) → broadcast. Timeout→text-reply preserved as a BRANCH, not a failure. |
| `RemoveBannedContentJob` | `RemoveBannedContent { user_id }` | steps: list messages → per-message destroy+broadcast (each its own step so resume continues, not restarts). |
| `ActiveStorage::PurgeJob` | `PurgeBlob { blob_id }` | steps: delete rows → delete objects (order: rows first so retry never resurrects; object delete idempotent). |
| `AnalyzeJob` (ad-hoc) | fold into `ProcessAttachment` workflow | steps: stage→analyze→variant/preview→notify (§22); original bytes never deleted on derived failure. Concurrency cap replaces the 4-slot semaphore. |

Cross-cutting: schedule AFTER commit only (outbox pattern — the current
`EventSink` already draws this line; §14). `DisconnectUser` never enters
DBOS (synchronous cable broadcast, §26). Scheduler absence (0.6.0-dev) means
any future periodic work (none identified upstream) needs an external
trigger — flagged, not worked around with invented APIs. At-least-once
writes (no transactional step) confirm: PG tx stays outside workflows (§13),
all external effects idempotent (§18).

## Config + local dev plan (§33–§34)

**Current surface** (to be replaced, not extended): `SECRET_KEY_BASE`
(required prod), `VAPID_{PUBLIC,PRIVATE}_KEY` + `VAPID_SUBJECT`, `RAILS_ENV`
(SQLite file selector), `TOPCAMP_STORAGE_PATH/DATABASE_PATH/FILES_PATH/
BACKUPS_PATH`, `TOPCAMP_FRAGMENT_CACHE_MB` (32), Thruster `TLS_DOMAIN/
HTTP_PORT/TARGET_PORT/...`, `TOPCAMP_FROZEN_TIME`, `TOPCAMP_LOG`;
explicitly N/A: `REDIS_URL`, `WEB_CONCURRENCY`, `PORT`, `SENTRY_DSN`.

**Target `.env.example`** (§33 — new keys, documented, no secrets committed):
`DATABASE_URL` (PG; DBOS sysdb shares it per `dbos` config), `SECRET_KEY_BASE`
(retained: Rails-compat cookie crypto must verify old cookies),
`RUSTFS_ENDPOINT/ACCESS_KEY/SECRET_KEY/BUCKET/REGION` (+ path-style flag),
`VAPID_PUBLIC_KEY/VAPID_PRIVATE_KEY/VAPID_SUBJECT` (retained semantics),
`APP_URL` (replaces TLS_DOMAIN-derived defaults for absolute URLs/avatars),
`PORT` (single listener — front/Thruster split deleted), `RUST_LOG`/
`TOPCAMP_LOG`. Dropped: `RAILS_ENV` file selection, `TOPCAMP_*_PATH`
storage paths (object keys now), `REDIS_URL` (never used).

**Local dev** (§34): `compose.yml` with `postgres:17` (data volume, healthcheck,
`DATABASE_URL` wired) + `rustfs` 1.0.0 (single-node, console off, bucket
auto-created by init step). DBOS runs in-process per the Rust crate's
architecture (no separate DBOS service). Startup: `compose up -d` →
`sqlx migrate`-equivalent (`migrate` subcommand) → `cargo run` (3 commands,
documented). Tests: PG + RustFS via testcontainers-style helpers or
compose-profile; unit tests stay dependency-free (richtext model).

## Errors + observability + quality notes (§31, §39–§42)

**Current error model** (to be reshaped, behavior kept): `kit::Error`
(Rails-mapped: Halt/BadRequest/ParameterMissing/InvalidAuthenticityToken/
UnknownFormat/NotFound/MethodNotAllowed/CookieOverflow/UnsafeRedirect/
IpSpoofAttack/Status/Internal) with `status()` mapping + public HTML pages;
`db::Error` (Sqlite/RecordNotFound/RecordInvalid/WriterGone/Other).
`Halt` is control flow, not failure. Target (§31): four-layer model
(domain/application/infra/HTTP) mapped at the Topcoat boundary; the
Rails→status table above is the conformance list.

**Observability** (§39): structured `tracing` with job names, queue-full/
dropped/shutdown-abandoned events; `TOPCAMP_LOG` EnvFilter + front-server
request logging. Target adds: request IDs through layers, workflow IDs on
DBOS steps/retries, RustFS op + Cable broadcast/connection events. Hygiene
holds today (no secret/password/token/message logging seen in app paths) and
stays a review gate.

**Quality/perf notes** (§40–§42): jemalloc + THP-off + fat-LTO.
`bench/` loadgen + `plans/perf-attribution.md` quantify choices (zlib-rs,
LTO, paging index). Worker/pool caps inventoried: job concurrency (config),
50/10k push pool, 16 unfurl / 4 parse / 4 media slots, 1024-queue per job
kind. No new ORM/job framework/object-SDK-abstraction without the §42
justification (explicit SQL + focused repos + DBOS + S3-client only).

### Room-type controllers + qr_code (supplement)

- **Opens**: show remembers + redirects; create (`Room::create_for` Open,
  creator as sole member) → broadcast prepend to everyone's `shared_rooms`;
  update forces type Open then saves name+type → replace broadcast (target
  names the Open class even for ex-closed rooms). `new` pre-fills "New room".
- **Closeds**: create with selected grantees → per-member prepend; update
  runs `memberships.revise` then replace per remaining member. Edit form
  partitions active users into selected/unselected.
- **Directs**: create finds-or-creates the direct room for
  (selected ∪ self) → per-membership `direct_rooms` prepend; destroy needs NO
  admin check (every member may); directs `show` = redirect by id (the 500
  variant belongs to the inherited no-`set_room` path). Edit hides self when
  >1 member.
- **qr_code** (unauthenticated): urlsafe-base64 `id` → QR SVG
  (`image/svg+xml`, 1-year public cache); malformed base64 → 500, too much
  data → 422. Strict Ruby-compatible base64 semantics (padding fix-up,
  `-_` mapping, nonzero-leftover-bit rejection).

### Rooms + memberships + authorization (domain trace supplement)

Authorization rule (single predicate, `User::can_administer`): administrator
OR record creator OR new-record — used for rooms AND messages (`ensure_can_
administer`, 403 otherwise). Room creation additionally gated by account
setting `restrict_room_creation_to_administrators`.

- `room_scope` per controller family: `All` (rooms), `WithoutDirects`
  (opens/closeds), `Directs` (directs). `set_room` misses (or out-of-scope)
  → redirect to root WITH alert "Room not found or inaccessible" (not 404).
  `index` with no rooms raises (`room_url(nil)`); `show` remembers last room
  visited (feeds search page `return_to_room_id`); `@message` variant pages
  around the message else last page.
- Inherited-callback nil traps (deliberate 500s, preserve!): opens/closeds
  `destroy` and directs `show` run WITHOUT `set_room` (subclass replaced the
  callback) and raise NoMethodError equivalents.
- `Room::destroy` (one tx): memberships deleted WITHOUT callbacks, each
  message destroyed (its own index/purge side effects), room row deleted →
  `room_remove` broadcast + redirect root. `revise(granted, revoked)`:
  `grant_to` (insert_all, skip existing, room-type default involvement) /
  `revoke_from` (per-membership destroy → `DisconnectUser` reconnect after
  commit). New open rooms grant all active users after commit.

### Rails crypto compat — carried over, not deleted (§1, §53, §54)

`rails_compat` (~1.5k lines) is byte-compatible Rails signing/encryption,
verified BOTH ways against the reference (golden vectors in, Rails re-checks
Rust output). The migration keeps it as a small `crypto` module:

- `Secrets { key_generator }` from `SECRET_KEY_BASE` (PBKDF2 key derivation
  per name); `app_verifier(name)` defaults: 64-byte key, HMAC-SHA1, strict
  Base64, `_rails` envelope, `json_allow_marshal`.
- Envelopes: modern `{"_rails":{"data","exp","pur"}}` vs legacy dual-
  serialized (`message` base64, always carrying exp/pur even null) for cookie
  jars; rotation falls back ONLY on format/serialization errors (expired and
  purpose-mismatch STOP — `rotates()`).
- Consumers that must keep working byte-identically: signed `session_token`
  cookie, encrypted `_topcamp_session`, signed blob ids (`blob_id` purpose),
  variation keys, disk URLs, signed stream names, transfer/avatar tokens,
  GlobalIDs. Golden tests (`vectors/rails_compat.json`) port 1:1.
- Drops: `marshal.rs` (Ruby-marshal fallback — keep ONLY if goldens require),
  `golden.rs` test harness shape, `turbo.rs` if Topcoat signs streams
  differently (it must still VERIFY old signatures).

## Phase 1 audit — status

All 13 §2 traces complete except dependency source inspection (Topcoat, DBOS
Rust, RustFS/S3, Tokio WS — versions unpinned, §52 gate holds: no framework
code may be written yet). Remaining Phase-1 items: `identify-tests` recorded
above; pin tasks stay open until real dependency sources are inspected.

## Target architecture

Per Instruction.md §3:

- **Topcoat** — HTTP/application boundary (router, requests, responses, pages, layers)
- **Application/domain** — use cases, auth context, authorization, orchestration (Topcoat-free domain)
- **PostgreSQL** — relational source of truth, transactions, full-text search
- **DBOS** — durable workflows, jobs, retries, scheduling, integrations (post-commit only)
- **RustFS** — S3-compatible blob/object store; metadata stays in PostgreSQL
- **Cable/Tokio** — WebSocket + Action Cable protocol (`/cable`), realtime fanout

## Dependency audit (source-inspected 2026-09-29, §2 Step 2, §52)

Reference copies (outside this workspace, NOT published):
`/tmp/topcoat`, `/tmp/dbos-transact-rust`. RustFS consumed as an S3 endpoint
per §20 (no source clone needed); Tokio WS already pinned in upstream.

| Dependency | Pinned version | Source |
| ---------- | -------------- | ------ |
| Topcoat | 0.9.0 (workspace.package) | `tokio-rs/topcoat`, edition 2024 |
| DBOS Transact Rust | 0.6.0-dev (`dbos` + `dbos-macros` crates, always same version) | `dbos-inc/dbos-transact-rust`, edition 2024, rust-version 1.95 |
| RustFS | 1.0.0 GA (2026-09-16), Apache-2.0, S3-compatible | `rustfs/rustfs` release; consumed via S3 API only |
| Tokio WebSocket | tokio-tungstenite 0.29 (as pinned upstream) | upstream `Cargo.toml`; re-verify at implementation |

### Topcoat capability map (§5 — grounded in actual source)

`topcoat-router/src/` confirmed: `router.rs`, `builder.rs` (`RouterBuilder::
route/discover_routes/page/discover_pages/layout/discover_layouts`),
`route/`, `path.rs`, `path_param.rs`, `query_param.rs`, `urlencoded.rs`
(forms), `request.rs`, `response/`, `body.rs` + `body_limit.rs`, `methods.rs`,
`layer/` + `tower.rs` (Tower integration), `connection.rs`, `listener.rs`,
`service.rs`, `proxy.rs`, `error/`, `trailing_slash.rs`, `origin.rs`,
`href.rs`, `page.rs`, `content/`, `compression.rs`. Sibling crates:
`topcoat-cookie`, `topcoat-session`, `topcoat-asset`, `topcoat-view`,
`topcoat-ui`, `topcoat-core` (+macro/grammar), `topcoat-cli`. Entry:
`topcoat::start(service)` with `Router::builder().discover().build()`.
Server-rendered, no-WASM framework — matches Topcamp's Askama SSR shape,
which favors keeping templates where they integrate (§10).

Existing→Topcoat mapping: routes→Router (replace); path/query params→
dedicated modules (replace); forms→urlencoded/body (replace); `kit`
Ctx/request/response/cookies/session→Topcoat primitives (simplify/redesign);
redirects→response primitive; middleware→Layer/Tower; errors→Topcoat error;
assets→topcoat-asset; views→Topcoat pages/components where suitable
(redesign, keep Askama only where it integrates cleanly); auth→layer/context;
`/cable`→Topcoat connection integration; Axum router→removed.

### DBOS Rust API (authoritative, §17 — no cross-language invention)

Crate API is function-based, NOT decorator-based: `dbos::step("name", ||
body)` / `step_with` (+ `StepOptions`, `ShouldRetry`), `dbos::select_step!`
durable race (sole proc macro; expansion names `::dbos` absolutely — do NOT
rename the dependency), `DBOS` instance/`Executor`, `WorkflowHandle`,
`join_workflows`/`select_workflow`, queues (`register_queue`, `enqueue`),
events/messaging, `sleep`, recovery, sysdb on Postgres. Workflow builders:
`run`/`run_with`/`start`/`start_with` (+ `RunOptions`/`StartOptions`).
**Limitations that shape design**: the scheduler is NOT yet implemented (lib.rs
"under construction"); Rust has NO transactional step yet — workflow writes
are at-least-once, so external side effects MUST be idempotent (§18) and the
PostgreSQL tx boundary stays outside DBOS (§13). Version pinning: `dbos`⇔
`dbos-macros` locked with `=`; `main` carries `-dev` (git deps read as
unreleased) — pin a RELEASED version when implementing, never `main`.

### RustFS / Tokio notes

RustFS 1.0.0 GA is the blob endpoint (`RUSTFS_ENDPOINT/ACCESS_KEY/
SECRET_KEY/BUCKET/REGION`, §33); app code uses only the S3-compatible surface
(§20). Tokio WS stays on the tungstenite line already vetted upstream; Cable
protocol code is transport-agnostic today (Axum upgrade) and moves to
Topcoat's `connection.rs` integration.

## Crate migration map (draft, §43)

Direction: `web → application → domain`, infrastructure injected at boundaries
(§44). Merging/deleting crates is expected (§1). Concrete per-crate disposition
still needs trace evidence before finalizing.

| Upstream crate | Provisional target |
| -------------- | ------------------ |
| `topcamp` (binary: controllers, channels, app, config, concerns) | Split: HTTP handling → Topcoat `web/`; orchestration → `application/`; business rules → `domain/` |
| `kit` (Axum HTTP kit + `front/` edge server) | Largely replaced by Topcoat primitives + layers (§4–§5); `front/` edge duties re-evaluated, not ported blindly |
| `routes` | Replaced by Topcoat Router (§6) |
| `db` (rusqlite) | Replaced by PostgreSQL schema + focused repositories (§11–§14); `events.rs` callbacks → post-commit outbox + DBOS (§14) |
| `richtext` | Kept as portable domain library, decoupled from frameworks (§23) |
| `storage` (local disk) | Bytes → RustFS `BlobStore`; metadata → PostgreSQL (§20–§22) |
| `cable` | Rewired to Topcoat connections + new application/PostgreSQL layer, protocol preserved (§24–§26) |
| `views` (Askama) | Reassessed against Topcoat pages/components; templates kept only where they integrate cleanly (§10) |
| `rails_compat` | Deleted where Topcoat/PostgreSQL provide the primitive; retained only for behavior-compat shims with justification (§1, §53) |
| `assets` | Replaced by Topcoat static-asset integration (§4) |
| `topcamp` `jobs.rs` + `integrations/*` | Re-evaluated job-by-job → DBOS workflows or synchronous code (§28, §46) |

## Deleted abstractions (draft — final confirmation at implementation, §1, §53)

- Axum Router + catch-all + Journey-emulation dispatcher (§6; replaced by
  Topcoat Router with precedence resolved at definition time)
- `kit` HTTP plumbing: Ctx/params/body/cookies/session/response wrappers,
  pre-routing middleware, front/Thruster edge server (Topcoat + layers)
- `routes` path-helper crate as routing authority (survives only as URL
  inventory for tests, if at all)
- `topcamp_db` rusqlite backend + FTS5 index (§11, §15)
- In-process job runner `jobs.rs` (DBOS workflows, §28)
- `rails_compat` except the byte-compat crypto contracts (see below —
  carry over as a small `crypto` module, NOT deleted)
- Local-disk `DiskService` backend (RustFS `BlobStore`, §20)
- `DatabaseService`-style god types (forbidden from the start, §12)
- Compatibility adapters are temporary only; none survive (§53)

## Behavioral compatibility decisions (from trace evidence, §36, §54)

Preserve: full route inventory incl. `/rooms/opens` precedence trap;
auth flows + cookie names/expiry/crypto; 10/3min login rate limit; hourly
session resume; Turbo Stream create/update/destroy shapes; pagination +
204-empty + ETag semantics; FTS AND-semantics + sanitizer + history trim;
staged-blob no-orphan guarantee; signed-URL purposes/expiry/cache headers;
Action Cable wire protocol + stream/dom_id naming + `NilInquiry` quirk;
webhook-vs-unfurl guard asymmetry; push pool overflow + VAPID-off behavior;
deliberate errors (non-string `q` 500, nil-search 500, JSON-update 500,
bad-signature 404). Do NOT preserve: crate/module names, old traits,
controllers, `kit` Ctx, job APIs, Axum structure, SQLite specifics, Thruster
front split. New-by-design: PG ranking, DBOS retries/durability, S3 presigned
URLs (same expiry semantics).

## Known limitations

1. Reference source is a shallow copy held outside this workspace (`/tmp`,
   not published); all 13 behavioral traces are recorded but the crate map
   and deletion list stay draft until implementation confirms them.
2. Dependencies source-inspected and pinned (Topcoat 0.9.0, DBOS 0.6.0-dev
   API, RustFS 1.0.0 GA, tungstenite 0.29); implementation must pin RELEASED
   DBOS, never `main`.
3. Live verification 2026-09-29 (Docker via OrbStack): `compose up -d`
   brought up postgres:17 + rustfs:1.0.0 (both `healthy`; RustFS self-
   reports 1.0.0 ready:true). 0001+0002 applied with ON_ERROR_STOP on a
   fresh database: 16 tables. Verified live: all four involvement values
   accepted / `bogus` rejected; outbox `push_message` accepted / `nope`
   rejected; membership uniqueness enforced; FKs enforced (message insert
   without room rejected); `@@ plainto_tsquery` match + `ts_rank` work.
   NOTE: scratch rows were inserted during verification — re-run migrations
   on a fresh volume for a pristine check (`docker compose down -v`).
4. RustFS S3-surface verified live 2026-09-29 (SigV4 round-trip from the
   compose network, stdlib-only probe): create-bucket 200, put 200, get 200
   with byte-identical body, head 200, delete 204 — ROUNDTRIP OK against
   RustFS 1.0.0. The §20 BlobStore contract (put/get/head/delete/presign)
   rests on a confirmed-working endpoint. (Host-side checks are impossible
   from this sandbox: outbound TCP denied + empty no_proxy routes localhost
   through the dead proxy; in-network checks via `docker exec` are the
   pattern.)
5. Presigned-URL flow verified live 2026-09-29: SigV4 query-auth URL
   (300s expiry) fetched the object with HTTP 200 and byte-correct body,
   no `Authorization` header — the public-download-behind-signed-URL and
   5-minute service-URL semantics in the BlobStore design are confirmed
   against RustFS 1.0.0.
6. Pristine re-run 2026-09-29 (`compose down -v`, fresh volumes): 0001+0002
   apply with ON_ERROR_STOP → 16 tables. Outbox claim pattern verified:
   `... WHERE done_at IS NULL AND claimed_at IS NULL ORDER BY id LIMIT n
   FOR UPDATE SKIP LOCKED` returns pending rows in order and the plan shows
   `Index Scan using index_outbox_pending`. Presigned flow re-verified on
   the fresh stack (put 200 → presigned GET 200, bytes exact).
7. Rust integration suite `crates/topcamp-db/tests/live.rs` (involvement /
   outbox / FTS, all in rolled-back txns): COMPILES green against real
   sqlx 0.9, but cannot RUN from this sandbox — the sandbox denies the test
   binary's TCP connect to PG (`Operation not permitted`; same restriction
   that blocks host curl to published ports). Run outside the sandbox:
   `DATABASE_URL=... cargo test -p topcamp-db`. SQL-level equivalents of
   all three tests were executed live via `docker exec psql` (items 3–5).
