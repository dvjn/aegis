#[path = "../ags/mod.rs"]
mod ags;
#[path = "../ags_wire.rs"]
mod ags_wire;

#[tokio::main]
async fn main() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    match ags::main().await {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("ags: {error:#}");
            std::process::exit(1);
        }
    }
}
