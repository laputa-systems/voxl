fn main() {
    let x264 = pkg_config::Config::new()
        .statik(true)
        .probe("x264")
        .expect("x264 (install libx264 with its pkg-config file)");
    let mut b = bindgen::Builder::default()
        .header_contents("wrap.h", "#include <stdint.h>\n#include <x264.h>\n")
        .allowlist_function("x264_.*")
        .allowlist_type("x264_.*")
        .allowlist_var("X264_.*")
        .layout_tests(false);
    for p in &x264.include_paths {
        b = b.clang_arg(format!("-I{}", p.display()));
    }
    for (k, v) in &x264.defines {
        b = match v {
            Some(v) => b.clang_arg(format!("-D{k}={v}")),
            None => b.clang_arg(format!("-D{k}")),
        };
    }
    let mut code = b.generate().expect("bindgen x264").to_string();
    // x264.h renames x264_encoder_open after the library's build number (a macro
    // bindgen can't follow); alias whichever version this x264 has.
    let open = code
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .find(|w| {
            w.strip_prefix("x264_encoder_open_")
                .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()))
        })
        .expect("x264_encoder_open_<build> in x264.h")
        .to_string();
    code.push_str(&format!("\npub use {open} as x264_encoder_open;\n"));
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    std::fs::write(out.join("x264.rs"), code).unwrap();
    println!("cargo:rerun-if-changed=build.rs");
}
