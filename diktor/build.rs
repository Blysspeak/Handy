//! Where the binary finds libtranscribe and the ggml backend modules.
//!
//! Installed: `<prefix>/bin/diktor` next to `<prefix>/lib/diktor/*.so`
//! (scripts/install.sh copies them there). From the build tree (`cargo run`,
//! `cargo test`): the directory transcribe-cpp installed them into.
fn main() {
    println!("cargo:rerun-if-env-changed=DEP_TRANSCRIBE_CPP_RUNTIME_DIR");
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../lib/diktor");
    if let Some(runtime) = std::env::var_os("DEP_TRANSCRIBE_CPP_RUNTIME_DIR") {
        let runtime = std::path::PathBuf::from(runtime);
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", runtime.display());
        // scripts/install.sh copies the shared libraries from here.
        println!(
            "cargo:rustc-env=DIKTOR_BUILD_RUNTIME_DIR={}",
            runtime.display()
        );
    }
}
