# Session log: Topcoat-native UI + open-source prep

How this repo got here — the working transcript behind the commits, kept so
future contributors can see *why* things are the way they are. Condensed from
the agent session; prompts are the user's own words.

## 1. The brief

> "I am testing how far we can take Topcoat natively. So don't want any
> javascript"

Campfire (37signals' `once-campfire-rust` port) rebuilt as **Topcamp**: same
product, Topcoat router/layers/session at the boundary, `live!`/`procedure`
regions, **zero hand-written JavaScript**. Server-side routing is a deliberate
choice (responses are fast; view transitions cover the UX gap).

> "get rid of turbo-fram / data-turbo / data-controller markup"

Removed: Turbo, Stimulus/`data-controller` markup, `room.js`. Kept: the
Topcoat runtime script and the verbatim `service_worker.js` (served at
`/service-worker[.js]` — registered, not unregistered).

## 2. Deletes without JS

> "I think delete could be managed. Why can't we use html dialog … and
> [live.html]"

First attempt — client `signal()` in confirm handlers — **panics outside
render scope**, plus `BoxView<'static>` borrow fights. Abandoned.

What shipped instead (`crates/topcamp-web/src/confirm.rs`): server-rendered
native `<dialog>` opened via `?confirm=<form-id>`. Confirm submits the real
form through `form=`, Cancel is a link back without the param. Structure
vendored from Topcoat UI's `alert_dialog` (not imported — the staging
components drag Tailwind copies).

Rule learned: **`signal()` only inside render scope.** Derive UI state from
query params/headers at the route, pass plain values down. Hoist dialog
`Slot`s before `view!`; leaves return `BoxView<'static>`.

## 3. State in params, prefs in cookies

> "can we use cookies … instead of using search params"

Decision: **params for per-page state, cookies for persistent prefs.**
`?confirm=`, `?reply_to=` (composer quote prefill), `?before=` (history
paging) — self-clearing, no cross-tab bleed, no extra setter hop. Cookies
only for `topcamp_theme` (`POST /account/theme`).

Trap found: `cookies(cx)` **panics where the cookie layer is absent**
(session routes, old-browser gate) — read the raw `Cookie` header there.

## 4. Motion, tooltips, drawer

> "Maybe we can use view transitions and animate stuff so that it doesn't
> feel like a full page transition"

Cross-document view transitions + `view-transition-name`s (rooms, bots,
avatar, inputs). Motion lives **only** inside
`prefers-reduced-motion: no-preference`. Chromium morphs; Safari/Firefox
snap gracefully.

> "Go through https://purecss.com/ and check if we can use some of them
> here?"

Verdict: PureCSS is a pattern gallery, not a framework for us. Adopted
exactly two patterns: `data-tip` CSS tooltips and `:user-invalid` form
validation. No PureCSS dependency vendored.

> Sidebar toggle videos → "whichever provides better UX stays"

Popover drawer won over the checkbox hack and anchor positioning:
`<button popovertarget="sidebar">` + `<aside popover="auto">` — top layer,
Esc + light-dismiss free, no state leak. Desktop force-shows the unopened
popover via CSS.

## 5. Seed data, disk, rename

> "lets add some seed data as well. I want to make a video of this later"

`cargo run -p topcamp-web --bin seed` → 4 users, 3 rooms, 58 messages,
2 boosts, 1 bot. Sign in with the debug demo button or
`demo@example.com` / `topcamp`. **Seed scratch databases only**, never the
shared dev DB.

> "Is rust causing the disk space to be full?" → "remove pounce-mono
> related worktrees" → "clear peppyhop worktrees as well"

`target/` (20–24 GB) dwarfs everything, but it's regenerable — space came
from stale worktrees (9.2 GB pounce-mono folder, peppyhop checkouts).
Removed those; `cargo clean` deliberately not run (cold rebuilds cost
more than the disk is worth mid-task). Note: `target/` is git-ignored.

> "rename the project to topcamp … make it clear that it is slopfork of
> Camfire rust using TopCoat"

Crates, cookie, env, and DB names became `topcamp-*`. Folder stayed
`campfire/` (an early full-directory rename broke the agent session's
file access mid-task — renamed back). Upstream fixture strings are
**sacred**: `once-campfire-rust` refs, `http://campfire.test` vectors,
`vectors/topcamp_user_agents.json` — renames must not touch them
(three lib tests broke on exactly this; reverted).

## 6. Open-source prep

> "Make this repo ready for opensource … Use TopCoat to fullest is our
> moto."

- `AGENTS.md`: motto, toolchain (rustup stable ≥ 1.98 first on PATH —
  Homebrew's 1.96 is too old for Topcoat 0.9), gates
  (`fmt --check`, `clippy --all-targets` zero warnings,
  `cargo test --workspace`), isolated `topcamp_test_*` DB hygiene,
  the pitfalls above.
- `.agents/skills/topcoat-nojs`: zero-JS recipes (?param state table,
  popover drawer, theme cookie, CSS patterns, lifetime rules,
  layout traps).
- `.agents/skills/topcamp-verify`: the exact verify loop (toolchain →
  gates → probe DBs → seed/serve → sacred fixtures).
- `git init`, `.gitignore` (`/target/`, `/.env`, tool caches),
  pushed public to `xinha-sh/topcamp` (the `xinhash` org exists but the
  account isn't a member — personal account until access lands).

## 7. Screenshots (and the bugs they caught)

> "run the project, take screenshots of each page / feature and add
> that too in the repo and Readme"

`superset browser` CLI isn't installed in this environment, so captures
run through headless Chromium (Playwright) via `docs/shots/capture.py`:
scratch `topcamp_shots` DB → migrate → seed → `serve` on :3000 →
demo-button sign-in → 8 shots with DOM assertions per feature.

Looking at the shots found two real bugs:

1. **Delete dialog invisible** — `.dialog { position: relative }`
   (anchors `.dialog__close`) defeated the UA's modal centering, so the
   `?confirm=` dialog rendered in-flow below the fold. Fix: explicit
   `position: fixed; inset: 0; margin: auto` for `.dialog[open]`.
2. **Composer collapsed** — `contain: inline-size` on `form#composer`
   wrapping a `display: contents` fieldset squeezed the form to zero
   width; the content-sized textarea exploded to ~900px tall and
   blanketed the message list. Fix: dropped `contain`, gave
   `#composer-frame` `flex-item-grow`.

> "https://www.youtube.com/shorts/Z1d428X-6rU to make textareas cool"

Kevin Powell's two textarea features: the **`lh` unit** + 
**`field-sizing: content`**. Applied as `min-height: 3lh`,
autogrow, capped at `max-block-size: 12lh` — but only *after* the
layout fix; `field-sizing` on the collapsed layout was the explosion.
Firefox/Safari ignore it and fall back to `rows=1`. Same markup
everywhere.

## 8. Verification status

`fmt` clean, `clippy` zero warnings, messages/bots/rooms_typed suites
60/60 green on isolated DBs, rooms suite 11/12 — the one failure
(`welcome_empty_page_matches_upstream_markup`, missing
`account/logo?v=`) fails identically with all changes stashed:
**pre-existing**, needs the singleton account row, left for a later
pass. Probe databases dropped after each run.

## Open threads (not yet done)

- `docker compose down -v && up -d` still wanted: the volume carries
  the old `campfire` role/DB; recreate as `topcamp`, then migrate +
  seed the video database.
- Old `campfire_*` databases still in PostgreSQL — drop after
  confidence.
- PWA install prompt and rich-text toggle are unreachable without JS:
  accepted limits, documented in `AGENTS.md`.
- The `?confirm=` dialog `::backdrop` CSS is aspirational: plain `open`
  (non-modal) never paints `::backdrop` without `showModal()`, which
  needs JS. The dialog centers over the page; dimming would need a
  CSS-only backdrop layer.
