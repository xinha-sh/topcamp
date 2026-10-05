# UI/UX parity backlog (ours vs `basecamp/once-campfire-rust`)

Goal: same routes, same pages, same behavior — Topcoat-native
implementation, upstream design system. Reference sources: upstream
route table (`crates/routes/src/lib.rs` in `/tmp/once-campfire-rust`),
view goldens (`crates/views/tests/golden/a/*.html`), Rails templates
(`reference/app/views`, pinned `90b3300`), vendored CSS
(`crates/topcamp-web/assets`, MIT — see `assets/NOTICE`).

Method per slice (proven by UI-01): port routes → markup from goldens
→ vendor newly-needed images → behavior parity (statuses, redirects,
flashes) → move/extend tests → side-by-side browser screenshots.

## Done

- [x] UI-01 session routes + sign-in page (`GET /session/new`,
  `POST`+`DELETE /session`, `_method` dispatch, `email_address` param,
  401 + `Too many requests or unauthorized.` flash + panel shake,
  10/3min IP rate limit → 429, no-users → `/first_run` redirect,
  `/account/logo` stock icon, full document shell). Pixel-verified vs
  upstream. Tests: web lib 28, live 6, cable e2e 1.
  Follow-up (user request, intentional parity deviation): debug
  builds replace the help-contact mailto with a `POST
  /session/demo` one-click login as the first administrator
  (CSRF-gated; release builds keep upstream's mailto and 404 the
  route). Tests: demo 3.
- [x] UI-02 first-run setup (`GET`+`POST /first_run`: nametag form,
  multipart `user[...]` params, account + administrator + `All Talk`
  room + grant, avatar upload to RustFS + blob/attachment rows +
  `process_attachment` outbox event, `prevent_repeats` → `/`, missing
  name → 500, race → redirect). Pixel-verified vs upstream (setup
  form identical; browser submit signs in and lands on `/`). Tests:
  web lib 31 (+3), first_run 6, live 6, cable e2e 1; workspace 132
  green; clippy 0, fmt clean. Toolchain: rustc 1.98.1 required
  (topcoat 0.9.0 `int_format_into`); use the rustup shim
  (`~/.cargo/bin/cargo`), not Homebrew cargo (1.96).

## Backlog (sequenced)

- [x] UI-02 first-run setup (`GET`+`POST /first_run`, account creation
  + admin user + avatar upload; `repeat_visits_redirect_home`,
  empty-users login redirect). Verified complete (`first_run`
  6/6 green); unblocks fresh installs.
- [x] UI-03 rooms home: `GET /` welcome (redirect to last-visited
  room via `last_room` cookie + original fallback; `No rooms yet`
  empty page), `GET /rooms` redirect (500 when roomless),
  `DELETE`/`POST+_method` `/rooms/:id` destroy (admin/creator only,
  membership+message cascade, redirect `/`), shell upgrades
  (current-user metas, logo `?v=`, `admin` body class). NOTE:
  `/rooms/new`, `POST /rooms`, `/rooms/:id/edit`, `PATCH/PUT`
  `/rooms/:id` are 404 upstream (no such actions); room creation
  lives under `/rooms/opens|closeds|directs` (UI-03c). set_room
  failure redirects `/` (alert flash text lands with UI-14).
  Pixel-verified vs upstream (empty state identical; sidebar delta
  is UI-03b). Message destroy now cascades (boosts, rich text,
  attachment + `purge_blob`, room touch). Tests: web lib 33, rooms
  12, live 6, first_run 6, cable e2e 1; workspace 146 green;
  clippy 0, fmt clean. Compare recipe: `-e SECRET_KEY_BASE=$(...)
  -e DISABLE_SSL=1` (origin check rejects plain-http POSTs
  otherwise). Test isolation: first_run runs in
  `topcamp_test_firstrun` (own DB; see file header); seeds use
  run-unique emails.
- [x] UI-03b sidebar (`GET /users/me/sidebar` content rendered
  inline in the app shell: directs strip, shared rooms with unread,
  new-room button gating, toggle, profile/account tools) + avatar
  route (`/users/:signed/avatar`: signed token, blob serve,
  initial-SVG fallback) + images (`messages-add`, `add`, `menu`,
  `settings`). Done: `sidebar.rs` (assemble+frame+show), `users.rs`
  wired (show+destroy), shell `aside` + `Option<Slot>` frame,
  epoch-ms fix, `contents=""` (view! needs valued attrs), route
  param unified to `{avatar_token}`, avatar test ids fixed to the
  golden fixture (127326141). Tests: sidebar 11, web lib 41;
  workspace 165 green; clippy 0, fmt clean. Browser-verified vs
  upstream (room page + invite flow): Ping strip, placeholder
  pings, room pills, + button, profile/gear tools all match.
  Live lists landed in UI-05 (no cable tags). Deferred: `:square`
  webp variant (UI-07).
- [x] UI-03c typed rooms CRUD (`/rooms/opens|closeds|directs`:
  new/create/edit/update/show/destroy incl. `destroy_without_room`
  500s, direct placeholder forms, `POST /rooms/directs`
  `user_ids[]`). Done: `rooms_typed.rs` (18 routes; shared
  set_room/guards/remember-cookie; per-kind modify dispatcher
  mirroring Rack method-override-before-routing; bracket-aware raw
  body parsing since `serde_urlencoded` maps structs by literal
  key), DB `FormUser`/`active_ordered`/`where_ids`/`grant_to`/
  `revoke_from`/`update`/`find_direct_for`, shell nav slot + owned
  titles, 5 icons vendored. Form pages carry no `body.sidebar`
  (only room page + welcome do) — caught by browser compare.
  Tests: rooms_typed 21 (isolated `topcamp_test_typed` DB;
  serial locks around the restriction flip; placeholder needle
  uses the oldest user), web lib 44; workspace 189 green (x3
  runs); clippy 0, fmt clean. Browser-verified vs upstream:
  opens/new + directs/new pixel-identical. Live fanout landed in
  UI-05 (bus + procedures). Deferred: alert flashes (UI-14).
- [x] UI-04 room show (`GET /rooms/:id` + `/rooms/:id/@:mid`
  permalink): nav/composer/invitation/message area, paged fragments
  (`before`/`after`, 204 empty, ETag/304) + message CRUD (nested
  `/rooms/:id/messages/:id`, `/edit`, `PATCH`/`PUT`/`DELETE`, Rack
  `_method` dispatch, turbo-stream append/remove, JSON update 500s,
  room miss 404s except create's not-found page). `richtext.rs`
  (sanitize+autolink+present+all_emoji, no regex dep); server
  pre-formats `message--formatted/me/first-of-day` + UTC timestamps
  (upstream JS does this client-side); composer/edit lexxy-editors
  nest a working textarea; quick-boost forms carry CSRF tokens per
  golden. HTML/JSON negotiation preserves the API. Isolated
  `topcamp_test_messages` DB; lib 60, integration 19; workspace 224
  green; clippy 0, fmt clean. Browser-verified: post/edit/show loop,
  invitation, message DOM skeleton identical to golden (except
  content autolink). Deferred: boost routes (UI-06), attachments
  (UI-07), mentions/embeds (UI-09), QR/regenerate routes (UI-12/UI-07),
  involvement bell routes + dialog (UI-15/UI-12), flashes (UI-14),
  `/play` sounds (UI-06).
- [x] UI-05 live room updates, Topcoat-native (no Turbo anywhere in
  the UI): `live!`/`emit!` regions over an in-process `LiveBus`
  (`live.rs`: room/user channels, typing TTL, 5 `#[procedure]`s) —
  per-message regions (edit/remove), cons-tail appends (exactly-once,
  catch-up included), typing indicator, presence lifecycle in the
  tail (present/refresh/disconnect + read marking), live sidebar
  lists (pips, membership changes), signal-driven composer posting
  via procedure, `room.js` (scroll pinning, older-page paging over
  `GET ?before=`, boundary re-thread, `data-confirm`). Deleted:
  `live.js`, `broadcasts.rs`, `/refresh` + markup, all Turbo tags
  (frames → divs, ids kept), stream signing use; form POSTs redirect
  (303). `/cable` stays as tested API surface (workflows relay
  depends on its naming); its e2e was trimmed to in-process truth
  (subscribes, typing/presence actions, storage, revocation —
  broadcasts need the bridge+worker). Fixes along the way:
  `HoistView` shell wrap (regions need a collection scope),
  `wants_html` takes the view branch for runtime WS renders (no
  `Accept` on handshakes), `topcoat::runtime::SCRIPT` client
  include, `bundle_assets` dev bin (no `topcoat` CLI here — run
  after asset changes). Evidence: lib 63, procedures e2e 4 (HTTP →
  procedure → db + bus fanout), messages 19, sidebar 11, cable 1,
  workspace 190+ green, clippy 0, fmt clean; two-window +
  cross-user ego-browser verify (append/edit/remove/typing/pip,
  zero console errors). Isolated `topcamp_test_procedures` DB
  added. Known edge: paged-in older messages are static (no
  regions) until reload.
- [x] UI-06 message extras + boosts. Scope corrected against
  upstream: forward/quote-as-route/pin/bookmark/plain-body have no
  upstream routes or templates (reference `_actions` is
  boosts/reply/copy-link/edit only), so they stay out. Shipped:
  `GET` index/`new`, `POST` create (303 room),
  `DELETE`+`_method` destroy (204, own-boost scoping) for
  `/messages/:id/boosts`, reachability 404s; `BoostsChanged` live
  event + `boost_message` procedure (quick-boost without reload);
  `room.js` reply (plaintext quote), copy-link (+flash),
  boost-delete (optimistic + reveal). At-page (`show_at` +
  `page_around`) and permalink menu already existed — covered by
  `show_at_pages_around_the_message`. (`boosts` 10/10, lib 65,
  browser-verified real clicks, serve log clean.) Known upstream
  quirk (identical CSS): the open menu's left column clips at
  main's edge for short own-messages.
- [x] UI-07 accounts (`GET`+`PATCH /account`, logo upload with
  `?v=` on `updated_at`, 192/512 PNG variants, avatar WebP,
  message attachments). Users list, join-code regenerate, logo
  show/destroy (ETag, self-healing variants); multipart
  `message[attachment]` (FileUploader shape, `X-CSRF-Token`),
  atomic post+attach commit, serve route (inline/`?disposition=`,
  serve-as-binary, ETag/304); video/image-lightbox/file-link
  presentation with Ruby-number dims; `room.js` composer file
  queue (pick/paste/drop + per-file XHR + progress chips);
  attachment-only edit page (no editor). (`accounts` 13/13,
  `attachments` 10/10, richtext 12, browser-verified uploads +
  live appends, serve log clean, DB back to fixtures.) No
  upstream `/account/transcodings` route exists (out of scope);
  thumbs/posters serve original bytes until the §22 worker
  records variants. Fixed en route: live-tail max-id cursor
  (concurrent posts re-appended), edit-frame `display: contents`
  (upstream turbo-frame rule; un-squeezed all bubbles).
- [x] UI-08 users (`/users/:id` show + ban/unban,
  `/users/me/profile` show/update, avatar upload/delete,
  `/account/users` paged fragment, involvement bells, destroy
  deactivates). Verified: 13 users tests + 294 workspace green,
  clippy 0, fmt clean; browser-verified crown toggle, profile
  rewrite vs upstream ERB, avatar round-trip, bell fetch-swap,
  ban/unban; DB back to fixtures (2 users, 3 messages, 0
  attachments, 0 blobs). Fixed en route: fragment next-link
  targets `/account/edit?page=` (no-JS fallback); missing
  `.position-absolute` utility; SVG icon blowup (bare flex imgs
  need explicit dims); profile rewritten to upstream
  `show.html.erb` (avatar__form, input--actor rows,
  colorize--black, membership card); bell glue posts
  urlencoded (FormData multipart 404'd); POST ban dispatches
  `_method=delete` to unban (UI unban was re-banning).
- [x] UI-09 searches page (`GET /searches` empty/results/chip/
  recents/clear, verbatim query sanitizer + 10-recents trim/retouch)
  + autocomplete (`/autocompletable/users` lexxy-prompt HTML + JSON,
  active-only, room scope, 404 for outsiders) + mentions (verified
  sgids re-render live, `message--mentioned` flag) + unfurl
  (`POST /unfurl_link` golden-parity: og:url fallback, image HEAD
  gate, fxtwitter rewrite, case-sensitive content-type, libxml entity
  decode, strip+sanitize pipeline, Surfguard tables, mailto 500) +
  og embed cards. Verified: 17 searches + 98 lib + 333 workspace
  green, clippy 0, fmt clean; browser-verified searches empty +
  results, mention tint, embed card, live github/tweet unfurls; DB
  back to fixtures (2 users, 3 messages, 0 searches, 0 blobs).
  Notes: FTS `plainto_tsquery` mints signed-number lexemes for
  hyphenated tags (tests use one alphanumeric token); `docker exec`
  needs `-i` for stdin; ego-browser `js/gotoUrl/waitForLoad` hang
  around navigations (split calls, navigate via `location.href`).
  Interactive @ dropdown needs Lexxy JS (not shipped; textarea
  composer carries the inert `lexxy-prompt`); unfurl JSON key order
  is alphabetical, not first-assigned (parsed-equal).
- [x] UI-10 join flow (`GET/POST /join/:code` → signup nametag;
  upstream has no bare `/join`, so the tracker line overstated).
  Signed-in bounce to `/`, missing account 500, wrong code 404,
  create auto-logs-in to `/`, taken email to `/session/new` with
  `?email_address=` prefilled (login now reads it), missing name
  500, empty password → NULL digest, avatar upload reuses the
  first-run pipeline (`UserForm`/`stage_avatar`/`attach_avatar`
  shared). New `DomainError::Conflict` (23505) rescued by join,
  500 unrescued like upstream. Verified: 13 join + 99 lib + 347
  workspace green, clippy 0, fmt clean; browser-verified nametag
  render + live signup w/ auto-login; DB back to 2-user fixture.
  Note: new `asset_fn!` needs `bundle_assets` before serve sees
  it (unbundled icon panics the renderer); HttpOnly session
  cookie can't clear via JS (log out through the app).
- [x] UI-11 session transfer (`GET /session/transfers/:id` show +
  `PUT/PATCH` update + `POST _method` modify; the tracker line's
  `/session/transfer` singular overstated) + `GET /qr_code/:id` SVG
  + incompatible-browser page + gate.
  `signed_id` port (`user/transfer`, 4h, PBKDF2-SHA256/1000,
  byte-for-byte vs an independent Python vector); transfer show
  renders the auto-submit PUT form signed out or in; update logs
  the active user in to `/`, 400 otherwise, 403 on CSRF miss, POST
  without put/patch 404s. Profile pages carry the `_transfer`
  fieldset (own page always; `users/show` admin-only on active
  users, recovery label for others). QR is the ported rqrcode gem
  (byte-identical SVG, `max-age=31556952, public`, `(.:format)`
  accepted, malformed 500, over-version-40 422). `allow_browser`
  runs as a layer (old UA → block page 200 on app routes incl.
  POSTs; `/_topcoat/*`, `/live/*`, `/cable`, `/up` bypass; layers
  run pre-session/cookies so the page renders the anonymous shell
  and `csrf::issue` falls back to an unpersisted token there).
  room.js gains auto-submit, lightbox open/reset, web-share files
  + `canShare` visibility (with a live-append observer), and
  upstream's copy reset/add success flash. Verified: 17 transfer
  + 8 signed_id + 5 user_agent (258 vectors) + 3 qr/rqrcode goldens
  + 118 lib + 383 workspace green, clippy 0, fmt clean, 92 assets;
  browser-verified profile fieldset, QR lightbox render, and a
  real-token redeem (303 → Demo profile); attachments' lightbox
  negative scoped to the file-link block (shell dialog carries
  the hooks now, like upstream). Notes: signed-out old browsers
  get the block page directly instead of upstream's 302-to-login
  first (converges on the next hop); `qrcode` crate dropped for
  the dep-free port; OrbStack died mid-slice (`orb start` revived
  it); ego-browser's installed API is `ego.helpers`, not the
  skill's `taskSpace` sugar.
- [x] UI-12 PWA (`GET /webmanifest[.json]`, `GET
  /service-worker[.js]`, iOS install prompt, manifest head link).
  Real upstream paths are `/webmanifest` + `/service-worker` (the
  tracker's `/manifest.json` overstated); there is no
  `/offline.html` and no icon beyond `apple-touch-icon` upstream,
  and unfurl landed whole in UI-09 (auth redirect, 400 blank,
  204 collection — diffed, no work).
  Manifest renders byte-exact vs upstream's template (raw `&`,
  valid JSON whatever the account is called; relative logo icons,
  absolute shortcut/screenshot asset URLs, relative fallback
  without Host). Worker served verbatim (`text/javascript`).
  Profile carries the platform-branched install prompt (10 golden
  UAs mapped; Chrome/Firefox-mac/modern-Edge render nothing; the
  `chrome && android` elif is dead upstream and mirrored; legacy
  Edge gets the address-bar branch; bots/curl get the fallback).
  room.js mirrors `pwa_install_controller` (beforeinstallprompt →
  prompting class, prompt on demand, appinstalled hides).
  Verified: 8 pwa + 3 unit (manifest golden, branches) + 394
  workspace green, clippy 0, fmt clean, 100 assets; live
  manifest/SW/head-link over HTTP. Browser-visual left to the
  user (they held the browser); Apple Messages previews hit the
  gate with the "Topcamp" title (old Safari, not a gem-bot).
  Bell `_browser_settings`/`_system_settings` stay UI-15 (push).
- [x] UI-13 bots + API keys (`/account/bots`, bot JSON API under
  `/rooms/:room_id/:bot_key/...`). Tracker overstated: no
  `/account/apikeys`, no `/integrations/...`, no webhooks management
  UI upstream — webhooks live only in the bot form's URL field.
  Done: session-or-bot-key auth (bad keys 303 like the app's other
  gates), `deny_bots` 403 on interactive routes (session wins),
  messages index/create/update/destroy + boosts create/destroy JSON
  (Jbuilder key order, `X-Total-Count` + `Link` paging, millis
  timestamps), webhook delivery trigger (`deliver_webhook` outbox
  rows; direct rooms all bots, else mentions, minus creator), bots
  index/new/create/edit/update/destroy/key-reset pages, `boosts.
  content` → TEXT (0006, SQLite ignores varchar(16)), deactivate
  keeps NULL emails (was `''`, unique-collided on the 2nd bot).
  15 bots integration + 4 JSON unit tests; live-verified index/
  create/update/boost/destroy/deny + index/edit screenshots.
- [x] UI-14 404 page parity (`not_found` markup) + flash-across-
  redirect helper (notices after create/update/destroy).
  Done: upstream `public/404.html` vendored byte-identical
  (`not_found.html`, `include_str!`), served standalone outside
  the layout by `#[route(* "/{*rest}")]` catch-all + explicit
  handlers (`rooms#new`); explicit-JSON 404
  (`{"status":404,"error":"Not Found"}`); pathless
  `not_found_normalizer` layer maps router 405
  (`MethodNotAllowedError`) to the 404 page — `#[layer]`
  layers never see miss terminals; avatars keep the empty 404
  (`head :not_found`). Flash via `flash` cookie (`n`/`a`,
  consumed once per render): account/profile update notices,
  missing-room alert redirect, 429 alert render.
  Verified: 9 flash/404 integration + 425 workspace green,
  clippy 0, fmt clean; live /nope byte-identical + PUT 404 +
  JSON 404 over HTTP; browser screenshot of the rendered page.
- [x] UI-15 push subscription repository (logout already accepts
  `push_subscription_endpoint`; wire destroy-by-endpoint).
  Done: `PushSubscriptionRepository` (user-scoped find/list/
  find-by-params/create/touch/destroy/destroy-by-endpoint);
  `/users/me/push_subscriptions` index (dev-mode panel, UA line,
  test/delete buttons) + create (JSON bell sync + form, touch or
  200/422) + destroy (303 index) + test_notifications (user-
  scoped find, 404 page when foreign, best-effort delivery
  skipped without VAPID keys, 303 index); endpoint validation
  (presence, HTTPS/443, 5 vendor hosts + subdomains, public-IP
  DNS via the unfurl guard, `TOPCAMP_TEST_PUSH_DNS` stub seam
  in debug builds); logout removes the session endpoint; bell
  not-allowed dialog with `pwa/browser_settings` +
  `pwa/system_settings` (platform branches) + install partial;
  profile "Push Notifications Dev Mode" link (debug only, like
  upstream's development gate); `vapid-public-key` layout meta.
  Verified: 13 push integration + 2 unit + 440 workspace green,
  clippy 0, fmt clean, 108 assets; live index/create/destroy/
  bell-dialog over HTTP + index/dialog screenshots (dialog
  opens live with the Chrome/macOS disclosures).
- [x] UI-16 remove the Tailwind pipeline + generic `components/`
  once no page uses them (build.rs, `styles.css`, font wiring).
  Done: deleted `build.rs` (tailwind `BuildConfig`), `styles.css`
  (Neutral theme), `components.toml` (`topcoat ui` state),
  `src/components.rs` + `src/components/` (button/card/field/
  input/label, unreferenced), `pub mod components`,
  `.discover_fonts()` + `font::RouterBuilderFontExt`, the
  `tailwind` + `font-fontsource` topcoat features, the direct
  `topcoat-font` dep (unused), and `[build-dependencies]`.
  Verified zero Tailwind users first (`justify-center` et al are
  Topcamp's own `utilities.css`; system font stacks only, no
  fontsource). 440 workspace green, clippy 0, fmt clean, 108
  assets bundled; live login/room + css/svg asset 200s, zero
  "font" references in served HTML, no font routes (browser
  backend wedged, screenshot skipped — deletion verified
  unreferenced by grep + byte-identical served pages).
- [x] UI-17 custom styles page (`GET /account/custom_styles/edit`,
  `PATCH`/`PUT /account/custom_styles`, `POST` + `_method`
  dispatch, `GET /account/custom_styles.css`). Done: the nav's dead
  "Custom styles" button now opens an admin-only textarea form
  (prefilled, CSRF-gated, 64 KiB cap, ✓ flash) persisting to the
  existing `accounts.custom_styles` column (no migration — present
  since `0001_init`); the CSS serves install-wide as a plain
  `text/css` file (public, ETag/304) linked from the document shell,
  so a stray `</style>` can never break markup. Tests: accounts 22
  (5 new: form render, update round-trip + 304 + prefill, member/
  signed-out gates, inert `</style><script>` passthrough, shell
  link); workspace 462 green; clippy 0, fmt clean.

## Standing notes

- Behavior contract per route: same methods, paths, params
  (`email_address`, `_method`, `authenticity_token`), statuses (401
  re-render vs 303 redirect), and flash copy. Rate limits where
  upstream has them (`sessions`, `unfurl_links`).
- No Turbo/Stimulus ports: dynamic behavior goes through Topcoat UI
  primitives (`live!`/`emit!`, `#[memoize]`, Shards). No-JS
  experiment: Turbo frames, `data-turbo-*`, `data-controller`, and
  the Stimulus `data-action`/`data-*-target/value/class` companions
  are stripped from rendered markup, and the hand-written `room.js`
  (plus the message `<script type="text/template">` block and the
  shell lightbox `<dialog>`) is deleted — only Topcoat's own runtime
  script ships. Forms that auto-submitted via Stimulus now carry
  plain submit buttons. Server pre-renders JS-added state that
  gates visibility/content (`message--formatted/me/first-of-day`,
  local-time text) so pages work without JS. Deletes confirm through
  server-rendered native `<dialog>` (`crates/topcamp-web/src/confirm.rs`,
  vendored from Topcoat UI `alert_dialog` structure): the trigger is a
  plain link reloading the page with `?confirm=<form-id>`, Confirm
  submits the original form via `form=`, Cancel links back — same
  methods/paths/params, zero JS and zero runtime dependence. Client
  signals were tried and abandoned: `signal()` panics outside a render
  scope (route handlers and plain sync view fns never run in one), and
  passing `&Cx` into a nested `Slot::new(...)` inside `view!` breaks
  the `BoxView<'static>` the router leaves demand — hoist dialog
  `Slot`s before `view!`, and note `view!` bodies run deferred in
  scope (the composer builds its signals inside the body). Reply works
  the same way (`?reply_to=<id>` pre-fills the composer textarea with
  a server-rendered quote, which also fixes no-JS posting since the
  textarea previously rendered empty); copy-link is a plain anchor to
  the message permalink. Composer attachments post without JS too: the
  file input is named `message[attachment]` and the form posts
  multipart (the create route already parsed both shapes; `multiple`
  dropped since the server keeps one file per message). History pages
  server-side (`?before=<id>` "Load older messages", unknown cursors
  fall back to the last page). Dead copy buttons became readonly
  selectable inputs. Mobile sidebar is a `popover="auto"` drawer
  (top layer, Esc + outside-click dismiss free, slide via
  `@starting-style` + `allow-discrete`); desktop force-shows the
  unopened popover as persistent nav. Chosen over the checkbox hack
  per the popover + anchor-positioning references: better dismiss UX,
  no state leaking across navigations.
  Theme is a `topcamp_theme` cookie (`light`/`dark`/`system`) read
  from the raw `Cookie` header — `cookies(cx)` panics where the cookie
  layer isn't installed — rendering `data-theme` on `<html>` with a
  `prefers-color-scheme` fallback. Demo/video data:
  `DATABASE_URL=... cargo run -p topcamp-web --bin seed`
  (migrated DB required; idempotent; demo button or
  `demo@example.com` / `topcamp`).
- Dev fixtures: `demo@example.com` is administrator; singleton
  account row (`Topcamp`) seeded — both required by the sign-in page.
