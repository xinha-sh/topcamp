//! Vendored upstream design-system assets (see `assets/NOTICE`).
//!
//! One function per file: `asset!` expands a `pub static ENCODED_ASSET`
//! at its call site, so two invocations cannot share one scope. Paths
//! are manifest-relative, hence stable across builds (unlike `$OUT_DIR`
//! inputs, whose ids vary with feature unification).
//!
//! Every declared asset is referenced by [`stylesheet_links`] or a page:
//! unreferenced declarations may be stripped from the binary and missed
//! by `topcoat asset bundle`.

macro_rules! asset_fn {
    ($name:ident, $dir:literal, $file:literal) => {
        pub fn $name() -> topcoat::asset::Asset {
            topcoat::asset::asset!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/assets/",
                $dir,
                "/",
                $file
            ))
        }
    };
}

// Stylesheets, in upstream link order (trix/lexxy editor sheets live
// only on editor pages upstream; nothing here needs them yet).
asset_fn!(css_reset, "css", "_reset.css");
asset_fn!(css_actiontext, "css", "actiontext.css");
asset_fn!(css_animation, "css", "animation.css");
asset_fn!(css_autocomplete, "css", "autocomplete.css");
asset_fn!(css_avatars, "css", "avatars.css");
asset_fn!(css_base, "css", "base.css");
asset_fn!(css_boosts, "css", "boosts.css");
asset_fn!(css_buttons, "css", "buttons.css");
asset_fn!(css_code, "css", "code.css");
asset_fn!(css_colorize, "css", "colorize.css");
asset_fn!(css_colors, "css", "colors.css");
asset_fn!(css_composer, "css", "composer.css");
asset_fn!(css_embeds, "css", "embeds.css");
asset_fn!(css_filters, "css", "filters.css");
asset_fn!(css_flash, "css", "flash.css");
asset_fn!(css_inputs, "css", "inputs.css");
asset_fn!(css_layout, "css", "layout.css");
asset_fn!(css_lightbox, "css", "lightbox.css");
asset_fn!(css_messages, "css", "messages.css");
asset_fn!(css_nav, "css", "nav.css");
asset_fn!(css_panels, "css", "panels.css");
asset_fn!(css_separators, "css", "separators.css");
asset_fn!(css_sidebar, "css", "sidebar.css");
asset_fn!(css_signup, "css", "signup.css");
asset_fn!(css_spinner, "css", "spinner.css");
asset_fn!(css_utilities, "css", "utilities.css");

// Icons used by the sign-in shell + first-run nametag (more arrive
// with later slices).
asset_fn!(img_alert, "images", "alert.svg");
asset_fn!(img_art, "images", "art.svg");
asset_fn!(img_bot, "images", "bot.svg");
asset_fn!(img_crown, "images", "crown.svg");
asset_fn!(img_globe, "images", "globe.svg");
asset_fn!(img_email, "images", "email.svg");
asset_fn!(img_password, "images", "password.svg");
asset_fn!(img_arrow_right, "images", "arrow-right.svg");
asset_fn!(img_lifebuoy, "images", "lifebuoy.svg");
asset_fn!(img_remove, "images", "remove.svg");
asset_fn!(img_download, "images", "download.svg");
asset_fn!(img_share, "images", "share.svg");
asset_fn!(img_common_file_text, "images", "common-file-text.svg");
asset_fn!(img_topcamp_icon, "images", "topcamp-icon.png");
asset_fn!(img_lanyard, "images", "lanyard.svg");
asset_fn!(img_camera, "images", "camera.svg");
asset_fn!(img_default_avatar, "images", "default-avatar.svg");
asset_fn!(img_person, "images", "person.svg");
asset_fn!(img_messages_empty, "images", "messages-empty.svg");
asset_fn!(img_messages_add, "images", "messages-add.svg");
asset_fn!(img_add, "images", "add.svg");
asset_fn!(img_menu, "images", "menu.svg");
asset_fn!(img_settings, "images", "settings.svg");
asset_fn!(img_everyone, "images", "everyone.svg");
asset_fn!(img_check, "images", "check.svg");
asset_fn!(img_login_keys, "images", "login-keys.svg");
asset_fn!(img_arrow_left, "images", "arrow-left.svg");
asset_fn!(img_trash, "images", "trash.svg");
asset_fn!(img_remove_circle, "images", "remove-circle.svg");
asset_fn!(img_arrow_down, "images", "arrow-down.svg");
asset_fn!(img_arrow_up, "images", "arrow-up.svg");
asset_fn!(img_search, "images", "search.svg");
asset_fn!(img_broom, "images", "broom.svg");
asset_fn!(img_messages_outlined, "images", "messages-outlined.svg");
asset_fn!(img_key, "images", "key.svg");
asset_fn!(img_web, "images", "web.svg");
asset_fn!(img_default_bot_avatar, "images", "default-bot-avatar.svg");
asset_fn!(img_attachment, "images", "attachment.svg");
asset_fn!(img_text_options, "images", "text-options.svg");
asset_fn!(
    img_menu_dots_horizontal,
    "images",
    "menu-dots-horizontal.svg"
);
asset_fn!(
    img_notification_bell_loading,
    "images",
    "notification-bell-loading.svg"
);
asset_fn!(
    img_notification_bell_alert,
    "images",
    "notification-bell-alert.svg"
);
asset_fn!(
    img_notification_bell_mentions,
    "images",
    "notification-bell-mentions.svg"
);
asset_fn!(
    img_notification_bell_everything,
    "images",
    "notification-bell-everything.svg"
);
asset_fn!(
    img_notification_bell_nothing,
    "images",
    "notification-bell-nothing.svg"
);
asset_fn!(
    img_notification_bell_invisible,
    "images",
    "notification-bell-invisible.svg"
);
asset_fn!(img_boost, "images", "boost.svg");
asset_fn!(img_reply, "images", "reply.svg");
asset_fn!(img_link, "images", "link.svg");
asset_fn!(img_pencil, "images", "pencil.svg");
asset_fn!(img_person_add, "images", "person-add.svg");
asset_fn!(img_qr_code, "images", "qr-code.svg");
asset_fn!(img_copy_paste, "images", "copy-paste.svg");
asset_fn!(img_refresh, "images", "refresh.svg");
asset_fn!(img_minus, "images", "minus.svg");
asset_fn!(img_bio, "images", "bio.svg");
asset_fn!(img_laptop, "images", "laptop.svg");
asset_fn!(img_transfer, "images", "transfer.svg");
asset_fn!(img_mobile_phone, "images", "mobile-phone.svg");
asset_fn!(img_browser_safari, "images/browsers", "safari.svg");
asset_fn!(img_browser_chrome, "images/browsers", "chrome.svg");
asset_fn!(img_browser_firefox, "images/browsers", "firefox.svg");
asset_fn!(img_browser_opera, "images/browsers", "opera.svg");
// PWA install prompt + manifest (UI-12). `install-edge.svg` sits at
// the images root, the path the upstream partial references.
asset_fn!(img_external_install, "images/external", "install.svg");
asset_fn!(img_external_share, "images/external", "share.svg");
asset_fn!(img_external_web, "images/external", "web.svg");
asset_fn!(img_external_gear, "images/external", "gear.svg");
asset_fn!(img_external_switch, "images/external", "switch.svg");
asset_fn!(img_external_sliders, "images/external", "sliders.svg");
asset_fn!(img_install_edge, "images", "install-edge.svg");
asset_fn!(img_menu_dots_vertical, "images", "menu-dots-vertical.svg");
asset_fn!(img_disclosure, "images", "disclosure.svg");
asset_fn!(
    img_screenshot_chat,
    "images/screenshots",
    "android-chat.png"
);
asset_fn!(
    img_screenshot_sidebar,
    "images/screenshots",
    "android-sidebar.png"
);
asset_fn!(
    img_screenshot_dark_mode,
    "images/screenshots",
    "android-dark-mode.png"
);
asset_fn!(img_lock, "images", "lock.svg");
asset_fn!(img_logout, "images", "logout.svg");
asset_fn!(img_messages, "images", "messages.svg");
asset_fn!(img_cancel, "images", "cancel.svg");

/// Stock account logo bytes (`/account/logo` when the account has no
/// custom logo), matching upstream's `send_stock_icon` fallback.
pub static STOCK_ACCOUNT_LOGO: &[u8] = include_bytes!("../assets/images/app-icon.png");
