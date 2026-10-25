//! 应用单实例：进程级文件锁 + 「唤醒」标记文件。
//!
//! 数据流：`main` 一进来就 [`claim`] 抢 `%LOCALAPPDATA%\myplay\instance.lock` 的排他锁。
//! 抢到 → 本进程是唯一实例，锁句柄活到进程退出（进程崩了由系统释放）；
//! 抢不到 → 已有实例在跑，[`signal_existing`] 写下 `instance.wake` 标记后直接退出。
//! 已有实例的托盘线程每轮（100 ms）检查这个标记，见到就删掉并送一条
//! `TrayCommand::ShowWindow`——窗口关着也会被重新拉起来，于是「重复启动」等于「叫出已有实例」。
//!
//! 选文件而不是命名互斥量 / 隐藏窗口：`windows` 依赖里没有互斥量所需的 feature，
//! 也不为一条唤醒信号再引依赖。标记文件先写后读，两边时序不会丢信号；
//! 写标记的进程还握着句柄时删除会失败，托盘线程下一轮重试即可。

use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};

use crate::core::config::data_dir;

/// 锁文件名（数据目录内）。
const LOCK_FILE: &str = "instance.lock";
/// 唤醒标记文件名（数据目录内）。
const WAKE_FILE: &str = "instance.wake";

/// 单实例守卫：存活期间其他进程抢不到锁。
pub struct Instance {
    /// 排他锁句柄；`None` 表示降级运行（锁文件都建不出来，本次不保证单实例）。
    _guard: Option<File>,
    /// 唤醒标记文件路径。
    wake: PathBuf,
}

impl Instance {
    /// 交给托盘线程的唤醒标记（托盘线程轮询它）。
    pub fn wake_signal(&self) -> WakeSignal {
        WakeSignal { path: self.wake.clone() }
    }
}

/// 抢单实例锁。
///
/// 返回 `None` 表示已有实例在跑，调用方应 [`signal_existing`] 后退出。
pub fn claim() -> Option<Instance> {
    let wake = wake_path();
    let guard = match open_lock(&lock_path()) {
        Ok(file) => file,
        Err(error) => {
            log::warn!("无法创建单实例锁（{error}），本次不保证只有一个实例");
            return Some(Instance {
                _guard: None,
                wake,
            });
        }
    };
    match guard.try_lock() {
        Ok(()) => {
            // 上一次异常退出可能留下唤醒标记：先清掉，免得一启动就把窗口弹出来。
            let _ = fs::remove_file(&wake);
            Some(Instance {
                _guard: Some(guard),
                wake,
            })
        }
        Err(TryLockError::WouldBlock) => None,
        Err(TryLockError::Error(error)) => {
            log::warn!("单实例锁不可用（{error}），本次不保证只有一个实例");
            Some(Instance {
                _guard: Some(guard),
                wake,
            })
        }
    }
}

/// 请已经在跑的实例把窗口显示出来：写唤醒标记，然后由调用方退出。
pub fn signal_existing() {
    let path = wake_path();
    let result = path
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| fs::write(&path, b""));
    if let Err(error) = result {
        log::warn!("无法唤醒已有实例（{error}）：{}", path.display());
    }
}

/// 「请显示窗口」标记：托盘线程每轮检查一次，读到就删除。
pub struct WakeSignal {
    /// 标记文件路径。
    path: PathBuf,
}

impl WakeSignal {
    /// 标记存在就删掉并返回 `true`（一条信号只消费一次）。
    pub fn take(&self) -> bool {
        match fs::remove_file(&self.path) {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                // 另一个实例可能正握着这个文件写（Windows 禁止删除打开中的文件）：下一轮再试。
                log::warn!("无法清理唤醒标记（{error}）：{}", self.path.display());
                false
            }
        }
    }
}

fn lock_path() -> PathBuf {
    data_dir().join(LOCK_FILE)
}

fn wake_path() -> PathBuf {
    data_dir().join(WAKE_FILE)
}

/// 打开（必要时创建）锁文件：数据目录可能还不存在。
fn open_lock(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    File::options().create(true).truncate(false).write(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::WakeSignal;

    #[test]
    fn wake_signal_is_consumed_once() {
        let path = std::env::temp_dir().join(format!("myplay-wake-{}.tmp", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let signal = WakeSignal { path: path.clone() };
        assert!(!signal.take(), "没有标记时不算信号");
        std::fs::write(&path, b"").expect("写唤醒标记失败");
        assert!(signal.take(), "标记存在时应当被消费");
        assert!(!signal.take(), "同一条信号不能重复消费");
    }
}
