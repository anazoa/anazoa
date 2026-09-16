use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    // raylib lives one level up (workspace root), not inside the package dir.
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

    let raylib_dir = workspace_dir.join("raylib");
    let raylib_src_dir = raylib_dir.join("src");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let build_dir = out_dir.join("raylib-memory-build");

    println!(
        "cargo:rerun-if-changed={}",
        raylib_dir.join("CMakeLists.txt").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        raylib_dir.join("CMakeOptions.txt").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        raylib_src_dir.join("CMakeLists.txt").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        raylib_src_dir.join("config.h").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        raylib_src_dir.join("raylib.h").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        raylib_src_dir.join("rcore.c").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        raylib_src_dir.join("platforms/rcore_memory.c").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        raylib_src_dir.join("rshapes.c").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        raylib_src_dir.join("rtextures.c").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        raylib_src_dir.join("rtext.c").display()
    );

    let target = env::var("TARGET").unwrap();
    let host = env::var("HOST").unwrap();

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

    let mut cmake_configure = Command::new("cmake");
    cmake_configure
        .arg("-S")
        .arg(&raylib_dir)
        .arg("-B")
        .arg(&build_dir)
        .arg("-DCMAKE_BUILD_TYPE=Release")
        .arg("-DBUILD_EXAMPLES=OFF")
        .arg("-DBUILD_SHARED_LIBS=OFF")
        .arg("-DPLATFORM=Memory")
        .arg("-DOPENGL_VERSION=Software")
        .arg("-DCUSTOMIZE_BUILD=ON")
        .arg("-DSUPPORT_MODULE_RAUDIO=OFF")
        .arg("-DSUPPORT_MODULE_RMODELS=OFF");

    if target != host {
        let (system_name, system_processor, c_compiler) = cmake_cross_args(&target);
        cmake_configure
            .arg(format!("-DCMAKE_SYSTEM_NAME={system_name}"))
            .arg(format!("-DCMAKE_SYSTEM_PROCESSOR={system_processor}"))
            .arg(format!("-DCMAKE_C_COMPILER={c_compiler}"));
    }

    run(&mut cmake_configure);

    run(Command::new("cmake")
        .arg("--build")
        .arg(&build_dir)
        .arg("--config")
        .arg("Release")
        .arg("--target")
        .arg("raylib"));

    let lib_dir = raylib_library_dir(&build_dir);
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=raylib");
    println!("cargo:include={}", raylib_src_dir.display());
}

fn run(command: &mut Command) {
    let status = command.status().unwrap_or_else(|error| {
        panic!("failed to run {:?}: {error}", command);
    });

    if !status.success() {
        panic!("{:?} failed with status {status}", command);
    }
}

fn raylib_library_dir(build_dir: &Path) -> PathBuf {
    for relative in ["raylib", "raylib/Release", "src", "src/Release", "."] {
        let candidate = build_dir.join(relative);
        if candidate.join(static_library_name()).exists() {
            return candidate;
        }
    }

    panic!(
        "could not find {} under {}",
        static_library_name(),
        build_dir.display()
    );
}

fn cmake_cross_args(target: &str) -> (&'static str, &'static str, String) {
    // Must come before the generic "aarch64-*" arm on desktop targets below —
    // aarch64-linux-android also starts with "aarch64-", and matching that
    // first previously pointed raylib's CMake build at the desktop
    // aarch64-linux-gnu-gcc (glibc) cross-compiler even when actually
    // targeting Android/Bionic. The two are close enough at the machine-code
    // level to link without error, but pull in glibc-only symbols like
    // __isoc99_sscanf that Bionic's libc.so never exports, which only shows
    // up as a dlopen failure on-device, not at build time.
    if target == "aarch64-linux-android" {
        // The versioned NDK clang wrapper already embeds the right
        // --target=/--sysroot flags, so it's a drop-in cross-compiler here —
        // same shape as the aarch64-linux-gnu-gcc case below, just for
        // Bionic. Sourced from the same env var scripts/build-android.sh
        // exports for cargo's own linker selection, so the two stay in sync.
        let cc = std::env::var("CC_aarch64_linux_android").unwrap_or_else(|_| {
            panic!(
                "CC_aarch64_linux_android must be set when cross-compiling raylib for {target} \
                 (see scripts/build-android.sh, which exports it to the NDK's aarch64-linux-androidNN-clang)"
            )
        });
        return ("Linux", "aarch64", cc);
    }

    if target.starts_with("aarch64-") {
        ("Linux", "aarch64", "aarch64-linux-gnu-gcc".to_string())
    } else if target.starts_with("armv7-") || target.starts_with("arm-") {
        ("Linux", "arm", "arm-linux-gnueabihf-gcc".to_string())
    } else {
        panic!("no cmake cross-compilation config for target {target}");
    }
}

fn static_library_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "raylib.lib"
    } else {
        "libraylib.a"
    }
}
