//! 系统集成：开机自启与事件日志落盘。
//!
//! 开机自启用 HKCU 的 Run 键（不需要管理员）。事件日志由独立的写线程持有句柄
//! （见 [`LogWriter`]），避免每写一行都 open/close 一次，也避免阻塞界面线程。
//! 另外提供一条诊断查询：[`foreground_window`]（当前前台窗口是谁）。

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

/// 当前前台窗口的「类名 / 标题」，只给抢失败时的日志用。
fn foreground_window() -> String {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetForegroundWindow, GetWindowTextW,
    };

    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        return "（无前台窗口）".to_owned();
    }

    let mut class = [0_u16; 128];
    let mut title = [0_u16; 256];
    let class_len = unsafe { GetClassNameW(hwnd, &mut class) };
    let title_len = unsafe { GetWindowTextW(hwnd, &mut title) };

    let text = |buffer: &[u16], len: i32| {
        let len = len.clamp(0, buffer.len() as i32) as usize;
        String::from_utf16_lossy(&buffer[..len])
    };
    format!("{} / {}", text(&class, class_len), text(&title, title_len))
}

/// 把标题为 `title` 的窗口强行切到前台。
///
/// Windows 只允许「前台进程 / 刚收到输入」的进程抢前台：从终端或 IDE 启动时两个条件
/// 都不满足，`SetForegroundWindow` 会被拒，表现就是窗口闪一下就退到后面。
/// 这里把本线程挂到当前前台线程的输入队列上（`AttachThreadInput`）借它的权限，
/// 拿到后立刻解除挂接；实在拿不到就临时 topmost 把窗口顶到 Z 序最前，至少让人看得见。
///
/// 返回窗口是否已经成为前台。
pub fn force_foreground(title: &str) -> bool {
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, FindWindowW, GetForegroundWindow, GetWindowThreadProcessId, HWND_NOTOPMOST,
        HWND_TOPMOST, IsIconic, SW_RESTORE, SWP_NOMOVE, SWP_NOSIZE, SetForegroundWindow,
        SetWindowPos, ShowWindowAsync,
    };
    use windows::core::PCWSTR;

    let name: Vec<u16> = title.encode_utf16().chain([0]).collect();
    let hwnd = match unsafe { FindWindowW(PCWSTR::null(), PCWSTR(name.as_ptr())) } {
        Ok(hwnd) => hwnd,
        Err(error) => {
            log::warn!("抢前台：找不到标题为「{title}」的窗口（{error}）");
            return false;
        }
    };

    let foreground = unsafe { GetForegroundWindow() };
    if foreground == hwnd {
        return true;
    }
    if unsafe { IsIconic(hwnd).as_bool() } {
        let _ = unsafe { ShowWindowAsync(hwnd, SW_RESTORE).ok() };
    }

    let foreground_thread = unsafe { GetWindowThreadProcessId(foreground, None) };
    let this_thread = unsafe { GetCurrentThreadId() };
    let attached = foreground_thread != 0
        && foreground_thread != this_thread
        && unsafe { AttachThreadInput(this_thread, foreground_thread, true).as_bool() };
    let activated = unsafe { SetForegroundWindow(hwnd).as_bool() };
    if attached {
        let _ = unsafe { AttachThreadInput(this_thread, foreground_thread, false).ok() };
    }
    if activated || unsafe { GetForegroundWindow() } == hwnd {
        return true;
    }

    // 仍然被拒：临时 topmost 把窗口顶到前面（不解激活，只是不躲在后面）。
    unsafe {
        BringWindowToTop(hwnd).ok();
        SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE).ok();
        SetWindowPos(hwnd, Some(HWND_NOTOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE).ok();
    }
    log::warn!(
        "抢前台被拒（前台窗口仍是：{}），已把窗口顶到最前但没拿到焦点",
        foreground_window()
    );
    false
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
