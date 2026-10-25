//! 构建脚本：只保留下游需要的链接参数。
//!
//! 上游版本还会读取 `../../VERSION` / `../../INSTALLER_VERSION` 并用 Windows SDK 的
//! `rc.exe` 生成 VERSIONINFO 资源，再用 `rustc-link-arg-bins` 注入到两个 CLI 目标里。
//! 这两个可执行目标已从本工作区移除（宿主是 `crates/myplay`），exe 图标与版本资源由
//! `crates/myplay/build.rs` 独立负责，所以这里只剩 Android 的 C++ 运行时链接开关。

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android") {
        return;
    }

    // Oboe 编译为静态 C++ 库，C++ ABI 仍需要共享运行时；缺少它时 Android 在加载
    // JNI 库阶段无法解析 `__cxa_pure_virtual`。
    println!("cargo:rustc-link-lib=dylib=c++_shared");
}
