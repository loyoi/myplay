//! L4 通道层：三种接收通道的统一抽象。
//!
//! 上层（UI / 托盘 / `core::Core`）只认 [`Channel`]：给一个 [`Source`] 就能启动，之后只通过
//! `stop` / `toggle` / `snapshot` 与之交互，不关心是妙播还是 DLNA。
//!
//! - [`miplay`]：小米妙播（嵌入 FusionPlay SDK）
//! - [`remote_core`]：DLNA 的 DMR 控制层（嵌入 fusionplay_core）
//! - [`dlna_player`]：DLNA 通道的本地拉流/解码/出声

pub mod dlna_player;
pub mod miplay;
pub mod remote_core;

use anyhow::Result;

use crate::core::config::{Settings, Source};
use crate::core::event::UiEvent;

/// 事件通道：通道把归一后的 [`UiEvent`] 直接交给 Core，中间不再序列化成字符串。
pub type EventSink = flume::Sender<UiEvent>;

pub use miplay::Snapshot;

/// 一条已启动的接收通道。
pub trait Channel: Send {
    /// 停止接收（应立即静音并释放端口）。
    fn stop(&self);

    /// 播放/暂停；不支持的通道是空操作。
    fn toggle(&self);

    /// 广播信息快照（只有妙播通道提供）。
    fn snapshot(&self) -> Option<&Snapshot>;
}

impl Channel for miplay::Receiver {
    fn stop(&self) {
        self.shutdown();
    }

    fn toggle(&self) {
        self.toggle_playback();
    }

    fn snapshot(&self) -> Option<&Snapshot> {
        Some(&self.snapshot)
    }
}

impl Channel for remote_core::CoreReceiver {
    fn stop(&self) {
        self.shutdown();
    }

    fn toggle(&self) {
        self.toggle_playback();
    }

    fn snapshot(&self) -> Option<&Snapshot> {
        None
    }
}

/// 按配置的启动源拉起对应通道。
pub fn start(source: Source, settings: &Settings, sink: EventSink) -> Result<Box<dyn Channel>> {
    match source {
        Source::MiPlay => Ok(Box::new(miplay::Receiver::start(settings, sink)?)),
        Source::Dlna => Ok(Box::new(remote_core::CoreReceiver::start(settings, sink)?)),
    }
}
