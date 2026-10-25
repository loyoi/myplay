//! 系统集成：开机自启与事件日志落盘。
//!
//! 开机自启用 HKCU 的 Run 键（不需要管理员）。事件日志由独立的写线程持有句柄
//! （见 [`LogWriter`]），避免每写一行都 open/close 一次，也避免阻塞界面线程。

use std::io::Write as _;
use std::path::Path;

/// 写入/移除开机自启。
///
/// 用 `reg.exe` 而不是引入注册表 crate：只操作一个值，命令行足够且免去新依赖。
pub fn set_autostart(enabled: bool) -> anyhow::Result<()> {
    const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    let exe = std::env::current_exe()?;
    let status = if enabled {
        std::process::Command::new("reg")
            .args([
                "add",
                KEY,
                "/v",
                "myplay",
                "/t",
                "REG_SZ",
                "/d",
                &format!("\"{}\"", exe.display()),
                "/f",
            ])
            .status()?
    } else {
        std::process::Command::new("reg")
            .args(["delete", KEY, "/v", "myplay", "/f"])
            .status()?
    };
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("注册表操作失败（exit {:?}）", status.code())
    }
}

/// 常驻的文件追加写入器：句柄跨行复用，路径变化时自动重开。
#[derive(Default)]
pub struct LogWriter {
    /// 当前打开的文件路径；`None` 表示还没打开过。
    path: String,
    /// 打开着的追加句柄；写失败后置空，下次写入时重试。
    file: Option<std::fs::File>,
}

impl LogWriter {
    /// 追加一行。`path` 为空表示用户关掉了文件日志。
    pub fn write(&mut self, path: &str, line: &str) {
        if path.trim().is_empty() {
            self.file = None;
            self.path.clear();
            return;
        }
        if self.file.is_none() || self.path != path {
            self.file = Self::open(path);
        }
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if writeln!(file, "{line}").is_err() {
            // 磁盘满 / 文件被删：丢掉句柄，下一行重试。
            self.file = None;
        }
    }

    fn open(path: &str) -> Option<std::fs::File> {
        let path = Path::new(path);
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            log::warn!("无法创建日志目录（{}）：{error}", parent.display());
            return None;
        }
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            Ok(file) => Some(file),
            Err(error) => {
                log::warn!("无法打开日志文件（{}）：{error}", path.display());
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LogWriter;

    #[test]
    fn empty_path_disables_writing() {
        let mut writer = LogWriter::default();
        writer.write("", "ignored");
        assert!(writer.file.is_none());
    }
}
