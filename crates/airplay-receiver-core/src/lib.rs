//! FusionPlay 接收核心（DLNA DMR 部分）。
//!
//! 对外只暴露两件事：DLNA 服务/控制器（[`DmrService`] / [`DmrController`]）与事件通道
//! （[`CoreEvent`] / [`EventSink`]），装配入口是 [`host::start_dlna`]。
//! AirPlay（RAOP）通道已移除，相应的音频引擎、配对存储与 HLS 视频桥一并删除。

mod dlna;
mod events;
mod host;
mod takeover;

pub use dlna::{DmrController, DmrService};
pub use events::{CoreEvent, EventSink};
pub use host::{Settings, start_dlna};
