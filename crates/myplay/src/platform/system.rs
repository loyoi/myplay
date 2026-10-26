//! 系统集成：开机自启与事件日志落盘。
//!
//! 开机自启用 HKCU 的 Run 键（不需要管理员），**直接调 Win32 注册表 API**：
//! 早先的实现启动 `reg.exe` 子进程，一次往返几十到几百毫秒（Defender 实时扫描下更久），
//! 还可能闪一下控制台，直接调 advapi32 是微秒级且无窗口。
//! 事件日志由独立的写线程持有句柄（见 [`LogWriter`]），避免每写一行都 open/close 一次，
//! 也避免阻塞界面线程。另外提供一条诊断查询：[`foreground_window`]（当前前台窗口是谁）。

use std::io::Write as _;
use std::path::Path;

/// 自启项的宿主键：`HKCU` 下不需要管理员权限。
const AUTOSTART_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// 自启项的值名。
const AUTOSTART_VALUE: &str = "myplay";

/// 打开自启项所在的 Run 键，用完由调用方 `RegCloseKey`。
#[cfg(windows)]
fn open_run_key(
    access: windows::Win32::System::Registry::REG_SAM_FLAGS,
) -> anyhow::Result<windows::Win32::System::Registry::HKEY> {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, RegOpenKeyExW};
    use windows::core::HSTRING;

    let path = HSTRING::from(AUTOSTART_KEY);
    let mut key = HKEY::default();
    let status = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, &path, None, access, &mut key) };
    if status != ERROR_SUCCESS {
        anyhow::bail!("无法打开注册表键 {AUTOSTART_KEY}（{status:?}）");
    }
    Ok(key)
}

/// 读取自启项当前登记的启动命令；`None` 表示没有这一项。
///
/// 与 [`set_autostart`] 一样直接调注册表：界面显示的状态必须来自注册表，
/// 不能只信 `settings.json`——用户可能在其它工具里删过这一项。
#[cfg(windows)]
pub fn autostart_command() -> anyhow::Result<Option<String>> {
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{
        KEY_QUERY_VALUE, REG_SZ, RegCloseKey, RegQueryValueExW,
    };
    use windows::core::HSTRING;

    let key = open_run_key(KEY_QUERY_VALUE)?;
    let name = HSTRING::from(AUTOSTART_VALUE);
    let mut kind = REG_SZ;
    let mut size = 0_u32;
    // 第一次只问长度（`lpdata` 传 None），避免按猜测开一个固定大小的缓冲。
    let status =
        unsafe { RegQueryValueExW(key, &name, None, Some(&mut kind), None, Some(&mut size)) };
    if status == ERROR_FILE_NOT_FOUND {
        let _ = unsafe { RegCloseKey(key) };
        return Ok(None);
    }
    if status != ERROR_SUCCESS {
        let _ = unsafe { RegCloseKey(key) };
        anyhow::bail!("读取自启项失败（{status:?}）");
    }

    let mut buffer = vec![0_u8; size.max(2) as usize];
    let status = unsafe {
        RegQueryValueExW(
            key,
            &name,
            None,
            Some(&mut kind),
            Some(buffer.as_mut_ptr()),
            Some(&mut size),
        )
    };
    let _ = unsafe { RegCloseKey(key) };
    if status != ERROR_SUCCESS {
        anyhow::bail!("读取自启项内容失败（{status:?}）");
    }

    // 注册表字符串以 NUL 结尾：截到第一个 0 再按 UTF-16 解。
    let units: Vec<u16> = buffer
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .take_while(|unit| *unit != 0)
        .collect();
    Ok(Some(String::from_utf16_lossy(&units)))
}

/// 自启项是否真的存在（只看有无，不校验路径）。
#[cfg(windows)]
pub fn is_autostart_enabled() -> anyhow::Result<bool> {
    Ok(autostart_command()?.is_some())
}

/// 写入/移除开机自启（直接调注册表，不做子进程往返）。
#[cfg(windows)]
pub fn set_autostart(enabled: bool) -> anyhow::Result<()> {
    if enabled {
        return write_autostart();
    }
    remove_autostart()
}

#[cfg(windows)]
fn write_autostart() -> anyhow::Result<()> {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{KEY_SET_VALUE, REG_SZ, RegCloseKey, RegSetValueExW};
    use windows::core::HSTRING;

    let exe = std::env::current_exe()?;
    // 带引号：路径里有空格时 `Run` 才能正确切分命令与参数。
    let command = format!("\"{}\"", exe.display());
    let name = HSTRING::from(AUTOSTART_VALUE);
    // `RegSetValueExW` 按字节切片收数据，含结尾的 NUL 一起写。
    let data: Vec<u8> = command
        .encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect();

    let key = open_run_key(KEY_SET_VALUE)?;
    let status = unsafe { RegSetValueExW(key, &name, None, REG_SZ, Some(&data)) };
    let _ = unsafe { RegCloseKey(key) };
    if status != ERROR_SUCCESS {
        anyhow::bail!("写入自启项失败（{status:?}）");
    }
    Ok(())
}

#[cfg(windows)]
fn remove_autostart() -> anyhow::Result<()> {
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{KEY_SET_VALUE, RegCloseKey, RegDeleteValueW};
    use windows::core::HSTRING;

    let key = open_run_key(KEY_SET_VALUE)?;
    let name = HSTRING::from(AUTOSTART_VALUE);
    let status = unsafe { RegDeleteValueW(key, &name) };
    let _ = unsafe { RegCloseKey(key) };
    // 值本来就不存在也算「已关闭」：这一步是幂等的。
    if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    anyhow::bail!("移除自启项失败（{status:?}）");
}

/// 非 Windows 目标不该走到这里；保留一处显式失败，别让调用方以为成功了。
#[cfg(not(windows))]
pub fn set_autostart(_enabled: bool) -> anyhow::Result<()> {
    anyhow::bail!("开机自启只支持 Windows")
}

/// 非 Windows 目标的自启状态查询。
#[cfg(not(windows))]
pub fn is_autostart_enabled() -> anyhow::Result<bool> {
    Ok(false)
}

/// 非 Windows 目标的启动命令查询。
#[cfg(not(windows))]
pub fn autostart_command() -> anyhow::Result<Option<String>> {
    Ok(None)
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
        BringWindowToTop, FindWindowW, GetForegroundWindow, GetWindowThreadProcessId,
        HWND_NOTOPMOST, HWND_TOPMOST, IsIconic, SW_RESTORE, SWP_NOMOVE, SWP_NOSIZE,
        SetForegroundWindow, SetWindowPos, ShowWindowAsync,
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
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE,
        )
        .ok();
        SetWindowPos(
            hwnd,
            Some(HWND_NOTOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE,
        )
        .ok();
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

    /// 自启写的 UTF-16 字节流能被读回同一句话（不碰真实注册表：只测编码往返）。
    #[test]
    fn run_value_roundtrips_as_utf16() {
        let command = "\"C:\\Program Files\\myplay\\myplay.exe\"";
        let units: Vec<u16> = command.encode_utf16().chain([0]).collect();
        let data: Vec<u8> = units.iter().flat_map(|unit| unit.to_le_bytes()).collect();

        let decoded: Vec<u16> = data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .take_while(|unit| *unit != 0)
            .collect();
        assert_eq!(String::from_utf16_lossy(&decoded), command);
    }
}
