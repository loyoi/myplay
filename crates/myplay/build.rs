//! 把图标嵌入 exe 资源：Windows 的任务栏/标题栏图标取自 exe 的第一个 ICON 资源。
//!
//! gpui 没有设置窗口图标的 API，所以走资源嵌入这条路；本文件只在 Windows 上做事。

fn main() {
    println!("cargo:rerun-if-changed=assets/myplay.rc");
    println!("cargo:rerun-if-changed=assets/myplay.ico");

    #[cfg(windows)]
    {
        // embed-resource 需要 `rc.exe`：把 Windows SDK 的 x64 目录补进 PATH。
        if let Some(dir) = sdk_rc_dir() {
            let path = std::env::var("PATH").unwrap_or_default();
            // SAFETY: 构建脚本单线程执行，此处修改环境变量不影响其它线程。
            unsafe {
                std::env::set_var("PATH", format!("{};{path}", dir.display()));
            }
        }
        let _ = embed_resource::compile("assets/myplay.rc", embed_resource::NONE);
    }
}

/// 找到最新一份 Windows SDK 的 x64 二进制目录（含 rc.exe）。
#[cfg(windows)]
fn sdk_rc_dir() -> Option<std::path::PathBuf> {
    let root = std::path::Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut versions: Vec<_> = std::fs::read_dir(root)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.join("x64").join("rc.exe").is_file())
        .collect();
    versions.sort();
    versions.pop().map(|path| path.join("x64"))
}
