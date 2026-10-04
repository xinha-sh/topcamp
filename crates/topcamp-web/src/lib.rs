//! Topcamp HTTP boundary on Topcoat (§4–§10 of the migration spec).
//!
//! Thin handlers over application use cases; no business rules here.
//! Evidence: MIGRATION_NOTES.md "Topcoat application-layer design".

pub mod accounts;
pub mod assets;
pub mod auth;
pub mod autocompletable;
pub mod boosts;
pub mod bots;
pub mod cable;
pub mod confirm;
pub mod csrf;
pub mod first_run;
pub mod flash;
pub mod involvements;
pub mod join;
pub mod layers;
pub mod live;
pub mod messages;
pub mod not_found;
pub mod pages;
pub mod push_subscriptions;
pub mod pwa;
pub mod qr_code;
pub mod richtext;
pub mod room_live;
pub mod room_show;
pub mod rooms;
pub mod rooms_typed;
pub mod routes;
pub mod searches;
pub mod sidebar;
pub mod signed_id;
pub mod state;
pub mod transfer;
pub mod unfurl;
pub mod user_agent;
pub mod users;
pub(crate) mod variants;

use state::AppState;
use topcoat::{
    asset::{AssetBundle, RouterBuilderAssetExt},
    context::Cx,
    cookie::RouterBuilderCookieExt,
    router::{Body, Method, RouteFn, RouteFuture, Router, response::Response},
    session::{RouterBuilderSessionExt, SessionConfig},
};

fn health(_cx: &Cx, _body: Body) -> RouteFuture<'_> {
    Box::pin(async move {
        Ok(Response::builder()
            .status(200)
            .body(Body::from("ok"))
            .expect("static 200 response builds"))
    })
}

/// Health route, factored for testability.
pub fn health_route() -> RouteFn {
    RouteFn::new(&[Method::GET], "/up", health)
}

/// Application router: app state in context, cookie + session layers, then
/// routes. Route precedence is resolved at definition time — the Rails-order
/// first-match dispatcher is deleted, not ported.
fn asset_bundle() -> Option<AssetBundle> {
    if let Ok(bundle) = AssetBundle::load() {
        return Some(bundle);
    }
    let exe = std::env::current_exe().ok()?;
    let parent = exe.parent()?;
    // Test binaries (in `deps/`) self-bundle: asset ids embed the build
    // OUT_DIR, which varies with feature unification (`-p` vs
    // `--workspace`), so a prebuilt `target/debug/assets` cannot match
    // every test binary. Bundling the running binary into a scratch dir
    // always matches. Release binaries always keep the prebuilt bundle.
    if cfg!(debug_assertions)
        && parent.file_name().is_some_and(|name| name == "deps")
        && let Some(bundle) = self_bundle_for_tests(&exe)
    {
        return Some(bundle);
    }
    let dir = parent.join("../assets");
    AssetBundle::load_dir(dir).ok()
}

/// Bundle the running test binary's embedded assets into a scratch dir
/// keyed by executable name (stable per fingerprint, once per process)
/// and load that. Any failure returns `None` so the caller falls back to
/// the prebuilt bundle.
fn self_bundle_for_tests(exe: &std::path::Path) -> Option<AssetBundle> {
    use std::sync::OnceLock;
    static BUNDLE: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    let dir = BUNDLE
        .get_or_init(|| {
            let bytes = std::fs::read(exe).ok()?;
            let name = exe.file_name()?.to_string_lossy();
            let scratch = std::env::temp_dir().join(format!("topcamp-test-assets-{name}"));
            // OUT_DIR is unset at test runtime, so the bundler's default
            // cache resolution would panic — point it at temp explicitly
            // (shared across runs: fonts download once).
            let cache = std::env::temp_dir().join("topcamp-test-asset-cache");
            let config = topcoat_asset::BundlerConfig::new().cache_dir(cache);
            topcoat_asset::Bundler::new(&config)
                .bundle(&bytes, &scratch)
                .ok()?;
            Some(scratch)
        })
        .as_ref()?;
    AssetBundle::load_dir(dir).ok()
}

pub fn router(state: AppState) -> Router {
    use topcoat::runtime::RouterBuilderRuntimeExt as _;
    let builder = Router::builder()
        .app_context(state)
        .cookies()
        .sessions(SessionConfig::default())
        .layer(layers::request_scope)
        .layer(layers::browser_gate)
        .layer(layers::not_found_normalizer())
        // Browser runtime (UI-05r): page/shard reruns + the runtime WS.
        // Pathless layers first, so reruns rewrite to GET before they run.
        .runtime();
    // Asset bundle next to the executable (`topcoat asset bundle`), falling
    // back to `target/debug/assets`. Test binaries self-bundle instead
    // (see `asset_bundle`). Rendering the stylesheet link requires a
    // config, so tests would panic without one.
    let builder = match asset_bundle() {
        Some(bundle) => builder.assets(bundle),
        None => builder,
    };
    builder
        .layout(pages::document)
        .route(rooms::welcome)
        .route(pages::login)
        .route(auth::demo_create)
        .route(first_run::show)
        .route(first_run::create)
        .route(rooms::rooms_new)
        .route(rooms::index)
        .route(rooms::destroy)
        .route(users::avatar_show)
        .route(users::avatar_destroy)
        .route(users::show)
        .route(users::ban)
        .route(users::unban)
        .route(users::profile_show)
        .route(users::profile_update)
        .route(users::profile_modify)
        .route(push_subscriptions::index)
        .route(push_subscriptions::create)
        .route(push_subscriptions::destroy)
        .route(push_subscriptions::create_test)
        .route(sidebar::sidebar_show)
        .route(rooms_typed::opens_index)
        .route(rooms_typed::opens_new)
        .route(rooms_typed::opens_create)
        .route(rooms_typed::opens_show)
        .route(rooms_typed::opens_edit)
        .route(rooms_typed::opens_modify)
        .route(rooms_typed::closeds_index)
        .route(rooms_typed::closeds_new)
        .route(rooms_typed::closeds_create)
        .route(rooms_typed::closeds_show)
        .route(rooms_typed::closeds_edit)
        .route(rooms_typed::closeds_modify)
        .route(rooms_typed::directs_index)
        .route(rooms_typed::directs_new)
        .route(rooms_typed::directs_create)
        .route(rooms_typed::directs_show)
        .route(rooms_typed::directs_edit)
        .route(rooms_typed::directs_modify)
        .route(involvements::show)
        .route(involvements::update)
        .route(involvements::modify)
        .route(searches::index)
        .route(searches::create)
        .route(searches::clear)
        .route(searches::modify_clear)
        .route(autocompletable::index)
        .route(unfurl::create)
        .route(join::new)
        .route(join::create)
        .route(transfer::show)
        .route(transfer::update)
        .route(transfer::modify)
        .route(qr_code::show)
        .route(pwa::manifest)
        .route(pwa::manifest_json)
        .route(pwa::service_worker)
        .route(pwa::service_worker_js)
        .route(accounts::logo_show)
        .route(accounts::destroy)
        .route(accounts::modify_logo)
        .route(not_found::catch_all)
        .route(health_route())
        .route(routes::show_room)
        .route(routes::list_messages)
        .route(routes::create_message)
        .route(messages::new_message)
        .route(messages::show)
        .route(messages::edit)
        .route(messages::attachment)
        .route(messages::update)
        .route(messages::replace)
        .route(messages::destroy)
        .route(messages::modify)
        .route(boosts::index)
        .route(boosts::new)
        .route(boosts::create)
        .route(boosts::destroy)
        .route(boosts::modify)
        .route(bots::api_index)
        .route(bots::api_create)
        .route(bots::api_update)
        .route(bots::api_destroy)
        .route(bots::api_boost_create)
        .route(bots::api_boost_destroy)
        .route(bots::bots_index)
        .route(bots::bots_new)
        .route(bots::bots_create)
        .route(bots::bots_edit)
        .route(bots::bots_update)
        .route(bots::bots_destroy)
        .route(bots::bots_modify)
        .route(bots::bots_key_update)
        .route(bots::bots_key_modify)
        .route(accounts::edit)
        .route(accounts::update)
        .route(accounts::replace)
        .route(accounts::users_index)
        .route(accounts::update_user)
        .route(accounts::replace_user)
        .route(accounts::destroy_user)
        .route(accounts::modify_user)
        .route(accounts::modify)
        .route(accounts::create_join_code)
        .route(accounts::set_theme)
        .route(live::post_message)
        .route(live::boost_message)
        .route(live::typing_start)
        .route(live::typing_stop)
        .route(live::presence_present)
        .route(live::presence_absent)
        // Keep `show_at` after every `/rooms/*` route: its second
        // segment is a catch-param that 404s non-`@` requests, and
        // first-registered wins.
        .route(room_show::show_at)
        .route(routes::search)
        .route(auth::sessions)
        .route(cable::cable_mount)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use topcamp_cable::Cable;
    use topcamp_db::PgDb;
    use topcoat::router::{Body, RouterBuilder, to_bytes};

    #[test]
    fn router_registers_all_routes() {
        let builder = RouterBuilder::new()
            .layer(layers::request_scope)
            .route(rooms::welcome)
            .route(pages::login)
            .route(first_run::show)
            .route(first_run::create)
            .route(rooms::rooms_new)
            .route(rooms::index)
            .route(rooms::destroy)
            .route(users::avatar_show)
            .route(users::avatar_destroy)
            .route(users::show)
            .route(users::ban)
            .route(users::unban)
            .route(users::profile_show)
            .route(users::profile_update)
            .route(users::profile_modify)
            .route(sidebar::sidebar_show)
            .route(rooms_typed::opens_index)
            .route(rooms_typed::opens_new)
            .route(rooms_typed::opens_create)
            .route(rooms_typed::opens_show)
            .route(rooms_typed::opens_edit)
            .route(rooms_typed::opens_modify)
            .route(rooms_typed::closeds_index)
            .route(rooms_typed::closeds_new)
            .route(rooms_typed::closeds_create)
            .route(rooms_typed::closeds_show)
            .route(rooms_typed::closeds_edit)
            .route(rooms_typed::closeds_modify)
            .route(rooms_typed::directs_index)
            .route(rooms_typed::directs_new)
            .route(rooms_typed::directs_create)
            .route(rooms_typed::directs_show)
            .route(rooms_typed::directs_edit)
            .route(rooms_typed::directs_modify)
            .route(involvements::show)
            .route(involvements::update)
            .route(involvements::modify)
            .route(searches::index)
            .route(searches::create)
            .route(searches::clear)
            .route(searches::modify_clear)
            .route(autocompletable::index)
            .route(unfurl::create)
            .route(join::new)
            .route(join::create)
            .route(transfer::show)
            .route(transfer::update)
            .route(transfer::modify)
            .route(qr_code::show)
            .route(pwa::manifest)
            .route(pwa::manifest_json)
            .route(pwa::service_worker)
            .route(pwa::service_worker_js)
            .route(accounts::logo_show)
            .route(accounts::destroy)
            .route(accounts::modify_logo)
            .route(accounts::users_index)
            .route(health_route())
            .route(routes::show_room)
            .route(routes::list_messages)
            .route(routes::create_message)
            .route(messages::new_message)
            .route(messages::show)
            .route(messages::edit)
            .route(messages::attachment)
            .route(messages::update)
            .route(messages::replace)
            .route(messages::destroy)
            .route(messages::modify)
            .route(boosts::index)
            .route(boosts::new)
            .route(boosts::create)
            .route(boosts::destroy)
            .route(boosts::modify)
            .route(bots::api_index)
            .route(bots::api_create)
            .route(bots::api_update)
            .route(bots::api_destroy)
            .route(bots::api_boost_create)
            .route(bots::api_boost_destroy)
            .route(bots::bots_index)
            .route(bots::bots_new)
            .route(bots::bots_create)
            .route(bots::bots_edit)
            .route(bots::bots_update)
            .route(bots::bots_destroy)
            .route(bots::bots_modify)
            .route(bots::bots_key_update)
            .route(bots::bots_key_modify)
            .route(room_show::show_at)
            .route(routes::search)
            .route(auth::sessions);
        assert!(!builder.is_empty());
        let _ = builder.build();
    }

    /// Test router with a lazy pool: no connection is opened, so any test
    /// reaching the database would fail — every test below stays on paths
    /// that respond before any query runs.
    fn test_router() -> Router {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused")
            .expect("lazy pool builds offline");
        router(AppState::new(PgDb::new(pool), Cable::new()))
    }

    fn get(uri: &str) -> http::Request<Body> {
        http::Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .expect("test request builds")
    }

    async fn body_text(response: topcoat::router::response::Response) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body reads");
        String::from_utf8(bytes.to_vec()).expect("response is UTF-8")
    }

    #[tokio::test]
    async fn health_returns_ok_with_request_id() {
        let response = test_router().handle(get("/up")).await;
        assert_eq!(response.status(), 200);
        assert!(response.headers().contains_key("x-request-id"));
        assert_eq!(body_text(response).await, "ok");
    }

    #[tokio::test]
    async fn unknown_path_is_404() {
        let response = test_router().handle(get("/nope")).await;
        assert_eq!(response.status(), 404);
    }

    #[tokio::test]
    async fn root_redirects_to_sign_in() {
        let response = test_router().handle(get("/")).await;
        assert_eq!(response.status(), 303);
        let location = response
            .headers()
            .get("location")
            .expect("redirect carries Location");
        assert_eq!(location, "/session/new");
    }

    /// Self-minted double-submit pair (any well-formed equal pair verifies).
    fn csrf_pair() -> (String, String) {
        let token = "ab".repeat(32);
        (
            format!("csrf_token={token}"),
            format!("authenticity_token={token}"),
        )
    }

    #[tokio::test]
    async fn logout_redirects_home_without_session() {
        let (cookie, field) = csrf_pair();
        let request = http::Request::builder()
            .method("POST")
            .uri("/session")
            .header("cookie", cookie)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(format!("{field}&_method=delete")))
            .expect("test request builds");
        let response = test_router().handle(request).await;
        assert_eq!(response.status(), 303);
    }

    #[tokio::test]
    async fn logout_without_csrf_token_is_forbidden() {
        let request = http::Request::builder()
            .method("POST")
            .uri("/session")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "authenticity_token={}&_method=delete",
                "ab".repeat(32)
            )))
            .expect("test request builds");
        let response = test_router().handle(request).await;
        assert_eq!(response.status(), 403);
    }

    #[tokio::test]
    async fn delete_session_without_session_redirects_home() {
        let (cookie, field) = csrf_pair();
        let request = http::Request::builder()
            .method("DELETE")
            .uri("/session")
            .header("cookie", cookie)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(field))
            .expect("test request builds");
        let response = test_router().handle(request).await;
        assert_eq!(response.status(), 303);
    }

    #[tokio::test]
    async fn malformed_room_id_is_400_before_any_database_touch() {
        let response = test_router().handle(get("/rooms/abc")).await;
        assert_eq!(response.status(), 400);
    }

    #[tokio::test]
    async fn unauthenticated_requests_are_401() {
        for response in [
            test_router().handle(get("/rooms/1")).await,
            test_router().handle(get("/rooms/1/messages")).await,
            test_router().handle(get("/search?q=hello")).await,
        ] {
            assert_eq!(response.status(), 401);
        }
    }

    #[tokio::test]
    async fn unauthenticated_post_is_401() {
        let request = http::Request::builder()
            .method("POST")
            .uri("/rooms/1/messages")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"client_message_id":"c1","body":"hi"}"#.as_bytes().to_vec(),
            ))
            .expect("test request builds");
        let response = test_router().handle(request).await;
        assert_eq!(response.status(), 401);
    }
}
