fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let windows_msvc = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");

    if windows_msvc {
        // FFmpeg 9's Schannel backend uses these Windows system APIs. The
        // ffmpeg-sys-next 9.0.1 vcpkg path omits them from its static link metadata.
        println!("cargo:rustc-link-lib=ncrypt");
        println!("cargo:rustc-link-lib=crypt32");
    }
}
