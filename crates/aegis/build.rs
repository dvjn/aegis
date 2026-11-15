use std::{env, fs, path::PathBuf};

use sha2::{Digest, Sha256};

const FONT_SOURCE: &str = "src/web/views/assets/fonts/inter/inter-latin-wght-normal.woff2";
const STYLES_SOURCE: &str = "src/web/views/assets/styles.css";

fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn read(path: &str) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|_| panic!("failed to read browser asset {path}"))
}

fn constant(name: &str, asset: &str, hash: &str, extension: &str) -> String {
    format!("pub const {name}: &str = \"/assets/{asset}.{hash}.{extension}\";\n")
}

fn main() {
    for path in [FONT_SOURCE, STYLES_SOURCE] {
        println!("cargo:rerun-if-changed={path}");
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));

    let font_path = format!(
        "/assets/inter-latin-wght-normal.{}.woff2",
        hash(&read(FONT_SOURCE))
    );

    let mut generated = String::new();
    generated.push_str(&constant(
        "STYLES_PATH",
        "styles",
        &hash(&read(STYLES_SOURCE)),
        "css",
    ));
    generated.push_str(&format!("pub const FONT_PATH: &str = \"{font_path}\";\n"));
    fs::write(out.join("browser_asset_paths.rs"), generated)
        .expect("failed to generate browser asset paths");
}
