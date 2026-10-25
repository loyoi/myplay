//! 把图标嵌入 exe 资源：exe 文件图标与 GPUI 窗口（标题栏 / 任务栏）图标都取自这里。
//!
//! GPUI 的 Windows 后端没有设置窗口图标的 API，它是在启动时用
//! `LoadImageW(module, PCWSTR(1), ...)` 取 **编号 1** 的 ICON 资源注册成窗口类图标；
//! 所以 `assets/myplay.rc` 里的资源 ID 必须是数字 `1`（用名字声明它取不到，标题栏就退化成默认图标）。
//! 本文件只在 Windows 上做事。

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
