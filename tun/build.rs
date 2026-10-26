use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    // The git checkout lives one level up (workspace root), not inside the package dir.
    let workspace_dir = manifest_dir.parent().unwrap().to_path_buf();

    // Rerun when the commit or branch tip changes
    println!(
        "cargo:rerun-if-changed={}/.git/HEAD",
        workspace_dir.display()
    );
    println!(
        "cargo:rerun-if-changed={}/.git/refs/heads/",
        workspace_dir.display()
    );

    let hash = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=GIT_COMMIT_HASH={hash}");

    let target = env::var("TARGET").unwrap();

    if target == "aarch64-linux-android" {
        // Force-keeps and exports the JNI symbols libwebrtc.a needs via
        // -Wl,--undefined=/--version-script (see configure_jni_symbols'
        // own doc comment). Emitted here rather than from webrtc-sys's own
        // build.rs so `cargo:rustc-link-arg-cdylib` applies to *this*
        // crate's cdylib output directly — anazoa-tun is the actual cdylib
        // (see [lib] above), webrtc-sys is only a dependency of it, so
        // Cargo prints a "this package does not contain a cdylib target"
        // warning (~225 times, one per kept symbol) when webrtc-sys emits
        // it instead. It still works either way — Cargo allows the
        // directive to propagate from anywhere in the dependency graph to
        // the final cdylib as a kept-for-compatibility quirk (see
        // https://github.com/rust-lang/cargo/issues/9562) — but emitting it
        // from the crate it actually describes avoids relying on that.
        webrtc_sys_build::configure_jni_symbols().unwrap();
    }
}
