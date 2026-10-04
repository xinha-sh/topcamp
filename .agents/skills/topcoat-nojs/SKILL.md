---
name: topcoat-nojs
description: Build zero-JavaScript UI with Topcoat — dialog confirms, query-param state, popover drawer, cookie prefs, CSS-only patterns.
---

# Topcoat Without JS

Topcamp ships no hand-written JavaScript: no Turbo/Stimulus, no
`data-controller` markup, no bespoke `<script>`. (The Topcoat runtime and
the verbatim `assets/js/service_worker.js` are the only exceptions.)
Every interactive behavior below is server rendering + HTML + CSS.
Follow these recipes instead of reaching for client script.

## UI state lives in additive GET params

Per-page toggles are query params the route reads and the template
reflects. They self-clear on navigation and never bleed across tabs:

| Param | Behavior | Implementation |
| ----- | -------- | -------------- |
| `?confirm=<form-id>` | Delete/destructive action opens a native `<dialog open>`; Confirm submits the real form via `form=`, Cancel links back without the param | `src/confirm.rs` (`ConfirmQuery`, `confirming()`, `delete_dialog_view`), `src/room_live.rs` (`confirm_form`) |
| `?reply_to=<id>` | Composer renders prefilled with a quote of the message | `src/room_show.rs` (`ReplyQuery`, `reply_draft`) |
| `?before=<cursor>` | Room history pages older messages | `src/room_show.rs` (`BeforeQuery`) |

Rules: params are additive — never rename paths/methods or change
statuses/flash copy to accommodate them. Cookies are only for
persistent prefs, never per-page state.

## Mobile drawer is a popover

The sidebar toggle is `<button popovertarget="sidebar">` + `<aside
id="sidebar" popover="auto">` (`src/pages.rs`, `src/sidebar.rs`).
Top-layer placement gives Esc + light-dismiss free with no state leak;
desktop force-shows the unopened popover via CSS. Do not reintroduce a
checkbox hack or JS toggle.

## Theme is a cookie + POST

`topcamp_theme` (`light`/`dark`/`system`) is read in `document_shell`
and written by `POST /account/theme` (`src/pages.rs`, `src/accounts.rs`).
Where the cookie layer is absent (session routes), `cookies(cx)` panics —
read the raw `Cookie` request header instead.

## CSS-only patterns

- Tooltips: `data-tip` attributes, pure CSS (`assets/css/`).
- Form validation: `:user-invalid` — no JS classes.
- Motion only inside `prefers-reduced-motion: no-preference`;
  cross-document view transitions + `view-transition-name`s
  (`assets/css/base.css`) make server renders feel continuous.
  No purple/indigo gradients, ever.
- Multipart `<form>` posts handle attachments; give the file input a
  `name` so it works with zero script (`src/room_show.rs`).

## Lifetimes and render scope (hard-won)

- `signal()` panics outside render scope. Derive state from
  params/headers at the route; pass plain values down.
- Hoist dialog `Slot`s **before** `view!`; leaf views return
  `BoxView<'static>`. `view!` bodies are deferred — don't borrow across
  them.
- Don't import Topcoat UI staging components wholesale (they drag
  Tailwind copies); vendor the structure (e.g. `alert_dialog`) into
  our CSS.

## Layout traps (found via screenshots)

- `.dialog[open]` needs explicit centering (`position: fixed; inset:
  0; margin: auto`). Our `.dialog` class sets `position: relative`
  (it anchors `.dialog__close`), which defeats the UA's modal
  centering — without the override a `?confirm=` dialog renders
  in-flow below the fold, invisible.
- Never put `contain: inline-size` on a form wrapping a
  `display: contents` fieldset: size containment collapses the form to
  zero width, and a content-sized textarea inside then explodes in
  height and blankets the page. `#composer-frame` needs
  `flex-item-grow` to fill the composer row.
- Cool textareas, no JS (Kevin Powell's two features): `lh` units for
  sizing (`min-height: 3lh`, no magic numbers) + `field-sizing:
  content` for autogrow, always with a `max-block-size` cap (e.g.
  `12lh`) so growth can never cover content. Only after the layout
  above is sound. Firefox/Safari ignore `field-sizing` and fall back
  to `rows=1` — same markup everywhere.

## Out of reach without JS (accepted limits)

PWA install prompt, rich-text toggle, and non-modal dialog focus trap.
Do not attempt them; document any new one in `AGENTS.md`.
