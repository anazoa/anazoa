use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // Rerun when the commit or branch tip changes
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads/");

    let hash = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=GIT_COMMIT_HASH={hash}");

    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let raylib_dir = manifest_dir.join("raylib");
    let raylib_src_dir = raylib_dir.join("src");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let build_dir = out_dir.join("raylib-memory-build");

    println!("cargo:rerun-if-changed=raylib/CMakeLists.txt");
    println!("cargo:rerun-if-changed=raylib/CMakeOptions.txt");
    println!("cargo:rerun-if-changed=raylib/src/CMakeLists.txt");
    println!("cargo:rerun-if-changed=raylib/src/config.h");
    println!("cargo:rerun-if-changed=raylib/src/raylib.h");
    println!("cargo:rerun-if-changed=raylib/src/rcore.c");
    println!("cargo:rerun-if-changed=raylib/src/platforms/rcore_memory.c");
    println!("cargo:rerun-if-changed=raylib/src/rshapes.c");
    println!("cargo:rerun-if-changed=raylib/src/rtextures.c");
    println!("cargo:rerun-if-changed=raylib/src/rtext.c");

    let target = env::var("TARGET").unwrap();
    let host = env::var("HOST").unwrap();

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
        .arg("raylib")
        );

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

fn cmake_cross_args(target: &str) -> (&'static str, &'static str, &'static str) {
    if target.starts_with("aarch64-") {
        ("Linux", "aarch64", "aarch64-linux-gnu-gcc")
    } else if target.starts_with("armv7-") || target.starts_with("arm-") {
        ("Linux", "arm", "arm-linux-gnueabihf-gcc")
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
