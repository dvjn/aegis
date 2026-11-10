#[path = "../relay/mod.rs"]
mod relay;
#[path = "../relay_wire.rs"]
mod relay_wire;

#[tokio::main]
async fn main() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    match relay::main().await {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("ags: {error:#}");
            std::process::exit(1);
        }
    }
}
