# Traces Supplement (reference: /tmp/once-campfire-rust, read-only)

## 1. kit front/ edge server (Thruster-in-process)

Behavior: in-process edge proxy in front of the app listener. HTTP/1.1 on HTTP_PORT (+ cleartext H2 with H2C_ENABLED); with TLS_DOMAIN, HTTPS+H2 on HTTPS_PORT via ACME certs while HTTP only answers HTTP-01 and 301-redirects to HTTPS (421 for non-TLS hosts). Per request (outermost first): request logging, MAX_REQUEST_BODY limit, compression layer, X-Request-Start, in-memory response cache, X-Forwarded-* proxy headers. Connection handling mirrors Go http.Server: HTTP_READ/WRITE/IDLE_TIMEOUTS, Date header, graceful 5s stop; WebSocket upgrades skip deadlines. App also listens alone on TARGET_PORT loopback (Puma-behind-Thruster shape). Not carried: X-Sendfile, BAD_GATEWAY_PAGE, RSA certs.
Evidence: crates/kit/src/front.rs (serve/serve_upstream), crates/kit/src/front/{handler.rs, conn.rs, tls.rs, acme.rs, compression.rs, cache.rs, config.rs}.
Migration consequence: workspace has only crates/topcamp-domain — no front server ported. Replaced by deployment platform (reverse proxy / TLS termination / CDN cache/compression); domain crate exposes no HTTP listener, ports, timeouts, or ACME config.

## 2. Assets pipeline (Propshaft precompile, embedded)

Behavior: build.rs digests/compiles reference/app/{assets,javascript}, vendor/javascript and gem assets exactly as Propshaft assets:precompile (SHA1 digest in filename, CSS/JS URL rewriting, sourcemap handling), renders import map from config/importmap.rb, embeds output + reference/public into binary. Runtime: helpers (asset_path/image/javascript/stylesheet over manifest; MissingAssetError on unknown), head tags (stylesheet_link_tag, javascript_importmap_tags + Link preload header), serve() = ActionDispatch::Static over embedded public/ + public/assets: GET/HEAD only, .html/index fallback, precompressed .br/.gz negotiation, Cache-Control "public, max-age=2592000", manifest at /assets/.manifest.json under PREFIX /assets.
Evidence: crates/assets/{build.rs, build/propshaft.rs, build/importmap.rs, src/{lib.rs, helpers.rs, tags.rs, serve.rs}}.
Migration consequence: no assets crate in workspace. Replaced by static frontend hosting/CDN + bundler output; domain crate carries no manifest, digest, or static-file serving.

## 3. cable turbo.rs + naming.rs

Behavior: turbo.rs = turbo-rails Turbo::StreamsChannel: subscribe via verified signed_stream_name (Turbo.signed_stream_verifier), optional Topcamp RoomStreamsAreAuthorized guard prepend (guard sees "" for failed verify; match => stream_from, else reject); non-string name raises ChannelError; Action enum + <turbo-stream> tag builders and broadcast_*_to helpers. naming.rs = broadcast/stream name derivation: gid_param = unpadded URL-safe Base64 of gid://app/Model/id (STI names, e.g. Rooms::Open); channel_name = strip "Channel", :: → :, underscore; broadcasting_for = channel:broadcastables; stream_name_from = join(":") (e.g. <room-gid-param>:messages).
Evidence: crates/cable/src/{turbo.rs, naming.rs} (+ rails_compat::turbo verifier).
Migration consequence: no cable/WebSocket crate in workspace. Replaced by platform realtime channel (or out-of-scope); stream-name formats above are the interop contract if re-added.

## 4. rails_compat password.rs + global_id.rs / signed_id.rs

Behavior: password.rs = has_secure_password: bcrypt $2a$, COST 12 (MIN_COST 4 for tests), 72-byte truncation inherited from bcrypt; verify returns false on malformed digest. global_id.rs: gid://topcamp/Model/id parse/display (query dropped); attachable_sgid = verifier(salt signed_global_ids, HMAC-SHA1, urlsafe-b64 padded, json_allow_marshal envelope) over "gid?...?expires_in" purpose attachable; sgid/locate_signed with purpose+expiry checks incl. Rails 7 Marshal-legacy fallback and signature-ignoring User-mention fallback. signed_id.rs: ActiveRecord::SignedId generate (SHA256/JSON/urlsafe) + verify fallback (SHA1/json_allow_marshal/strict b64); purpose = base-class-underscore[/purpose] (user/avatar, rooms/open).
Evidence: crates/rails_compat/src/{password.rs, global_id.rs, signed_id.rs, message_verifier.rs, key_generator.rs}.
Migration consequence: password/global-id/signed-id verifiers not ported to topcamp-domain. Replaced by host auth framework + opaque IDs; cross-compat with Rails cookies/SGIDs/signed_ids requires re-porting these exact formats (bcrypt cost, HMAC digests, envelopes, purpose strings).
