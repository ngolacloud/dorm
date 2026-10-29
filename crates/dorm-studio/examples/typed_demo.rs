//! Stand DORM Studio up over the ORM's own example models — the in-workspace
//! stand-in for the `src/bin/studio.rs` a project generates. It links the models
//! (so the registry is populated) and calls [`dorm_studio::serve`]; everything
//! else is the library.
//!
//! ```sh
//! DELONIX_ORM_DSN='host=/var/run/postgresql user=me dbname=dev' \
//!   cargo run -p dorm-studio --example typed_demo -- --write --port 5590
//! ```

use dorm_core::meta::ModelMeta;
use dorm_core::model::Model;
use dorm_models::{Application, Deployment, Plan, Product, Tenant};

/// Name each model's metadata so the linker keeps the crate that registers it.
fn link_models() {
    let _linked: &[&'static ModelMeta] = &[
        <Tenant as Model>::meta(),
        <Plan as Model>::meta(),
        <Application as Model>::meta(),
        <Deployment as Model>::meta(),
        <Product as Model>::meta(),
    ];
}

#[tokio::main]
async fn main() {
    link_models();
    let args: Vec<String> = std::env::args().collect();
    let writable = args.iter().any(|a| a == "--write");
    let port = args
        .iter()
        .position(|a| a == "--port")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(5555);
    if let Err(e) = dorm_studio::serve(port, writable).await {
        eprintln!("studio: {e}");
        std::process::exit(1);
    }
}
