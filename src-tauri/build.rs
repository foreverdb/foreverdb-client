use std::fs;

/// Token for addon updates, compiled in: `FOREVERDB_GITHUB_TOKEN` from the build
/// environment wins, otherwise the contents of `github-token` next to this file.
fn main() {
    println!("cargo:rerun-if-env-changed=FOREVERDB_GITHUB_TOKEN");
    println!("cargo:rerun-if-changed=github-token");
    let from_env = std::env::var("FOREVERDB_GITHUB_TOKEN").is_ok_and(|value| !value.trim().is_empty());
    if !from_env {
        if let Ok(token) = fs::read_to_string("github-token") {
            let token = token.trim();
            if !token.is_empty() {
                println!("cargo:rustc-env=FOREVERDB_GITHUB_TOKEN={token}");
            }
        }
    }
    tauri_build::build()
}
