//! Dev tool: bundle `serve`'s embedded web assets into `target/debug/assets`.
//!
//! Stand-in for `topcoat asset bundle --bin serve -p topcamp-web` (the CLI
//! is not installed in this environment). Run after changing anything under
//! `crates/topcamp-web/assets/`, then restart `serve`:
//!
//! ```sh
//! cargo build -p topcamp-web --bin serve
//! cargo run -p topcamp-web --bin bundle_assets
//! ```

fn main() {
    let exe = std::env::current_exe().expect("current exe resolves");
    let dir = exe.parent().expect("exe has a parent dir");
    let serve = dir.join("serve");
    let bytes = std::fs::read(&serve).expect("build serve first");
    let out = dir.join("assets");
    let cache = dir.join("asset-cache");
    let config = topcoat_asset::BundlerConfig::new().cache_dir(cache);
    topcoat_asset::Bundler::new(&config)
        .bundle(&bytes, &out)
        .expect("bundle writes");
    println!(
        "bundled {} assets",
        out.read_dir().map(|d| d.count()).unwrap_or(0)
    );
}
