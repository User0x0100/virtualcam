#![cfg(windows)]

use std::{env, path::PathBuf, process::Command};

#[test]
#[ignore = "requires cl.exe from a Visual Studio developer environment"]
fn msvc_cpp_layout_matches_rust_assertions() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source = root.join("tests").join("abi_conformance.cpp");
    let output = root.join("target").join("abi_conformance.exe");
    let compiler = env::var_os("CXX").unwrap_or_else(|| "cl.exe".into());
    let status = Command::new(compiler)
        .current_dir(&root)
        .args(["/nologo", "/std:c++17", "/EHsc"])
        .arg(&source)
        .arg(format!("/Fe:{}", output.display()))
        .status()
        .expect("launch MSVC C++ compiler");
    assert!(status.success(), "C++ ABI helper failed to compile");
    let status = Command::new(output).status().expect("run C++ ABI helper");
    assert!(status.success(), "C++ ABI helper assertions failed");
}
