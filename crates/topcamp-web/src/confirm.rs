//! Delete confirmations with zero JavaScript: the trigger is a plain
//! link that reloads the page with `?confirm=<form-id>`, and the server
//! renders a native `<dialog open>` for that form. Confirm submits the
//! original form by id (`form=`); Cancel links back without the param.
//! Without the param no dialog opens, so every render is a plain,
//! bookmarkable GET — there is no separate no-JS fallback path because
//! the confirm itself is server-rendered. The dialog structure mirrors
//! Topcoat UI's `alert_dialog` (role, title, description, footer
//! actions), re-skinned to this app's `.dialog` CSS — the registry ships
//! staging-only copies, so it is vendored here rather than imported, and
//! without its Tailwind classes. Client signals were tried first and
//! abandoned: `signal()` panics outside a render scope (route handlers
//! and plain sync view functions never run in one), and nesting a
//! signal-bearing `Slot` inside `view!` forces a `&Cx` borrow that
//! breaks the `BoxView<'static>` leaves (`Slot`, `async_into_response`)
//! demand. The non-modal `open` dialog has no focus trap or Escape
//! handling, the same limitation the registry documents.

use topcoat::{
    context::Cx,
    router::query_params,
    view::{BoxView, ViewExt as _, view},
};

/// `?confirm=<form-id>`: which delete form the page renders open.
/// Absent or naming no form on the page, every dialog renders closed.
#[query_params(error = bad_request)]
pub(crate) struct ConfirmQuery {
    pub confirm: Option<String>,
}

/// True when `?confirm=` names `form_id`.
pub(crate) fn confirming(confirm: Option<&str>, form_id: &str) -> bool {
    confirm == Some(form_id)
}

/// Confirm dialog for the form with `form_id`.
///
/// The dialog itself contains no `<form>`: Confirm submits the original
/// form via `form=`, Cancel is a link to `cancel_href` (the same page
/// without `?confirm=`). Rendered with `open` only when `open` is true.
pub(crate) fn delete_dialog_view(
    cx: &Cx,
    form_id: &str,
    confirm_label: &str,
    title: &str,
    message: &str,
    open: bool,
    cancel_href: &str,
) -> BoxView<'static> {
    let form_id = form_id.to_string();
    let confirm_label = confirm_label.to_string();
    let title = title.to_string();
    let message = message.to_string();
    let cancel_href = cancel_href.to_string();
    let title_id = format!("{form_id}-title");
    let description_id = format!("{form_id}-description");
    view! {
        cx =>
        <dialog open=(open) class="dialog pad center center-block border-radius border shadow" role="alertdialog" aria-labelledby=(title_id.clone()) aria-describedby=(description_id.clone())>
            <h2 id=(title_id)>(title)</h2>
            <p id=(description_id)>(message)</p>
            <div class="flex align-center gap">
                <button type="submit" class="btn btn--negative" form=(form_id)>(confirm_label)</button>
                <a class="btn" href=(cancel_href)>"Cancel"</a>
            </div>
        </dialog>
    }
    .boxed()
}
