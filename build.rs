#[cfg(target_os = "windows")]
fn main() {
    println!("cargo:rerun-if-changed=assets/codex-account-switcher.ico");
    winresource::WindowsResource::new()
        .set_icon("assets/codex-account-switcher.ico")
        .compile()
        .expect("failed to embed Windows icon resource");
}

#[cfg(not(target_os = "windows"))]
fn main() {}
