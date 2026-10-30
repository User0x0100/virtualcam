use std::env;
use std::path::PathBuf;

fn main() {
    // 库本身不链接 CUDA；该搜索路径只供自行链接 cudart.lib 的 CUDA 示例使用。
    println!("cargo:rerun-if-env-changed=CUDA_PATH");

    if env::var_os("CARGO_FEATURE_CUDA").is_none() || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let cuda_path = env::var_os("CUDA_PATH");
    let Some(cuda_path) = cuda_path else {
        return;
    };
    let architecture = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => "x64",
        Ok("x86") => "Win32",
        _ => return,
    };
    let search_path = PathBuf::from(cuda_path).join("lib").join(architecture);
    if search_path.is_dir() {
        println!("cargo:rustc-link-search=native={}", search_path.display());
    }
}
