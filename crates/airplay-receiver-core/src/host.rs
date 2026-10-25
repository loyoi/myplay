//! 核心对外的装配入口：只负责启动 DLNA DMR 与会话间本地状态。
//!
//! 数据流：嵌入方（`crates/myplay`）→ [`start_dlna`] → [`DmrService`]（SSDP / GENA / HTTP）
//! 并把 [`DmrController`] 交给嵌入方自己的播放器回填「就绪 / 位置 / 速率」。
//!
//! AirPlay（RAOP）通道已从本 crate 移除：`crates` 里只保留 DLNA 需要的基础设施，
//! 因此这里不再依赖 `shairplay`。

use crate::dlna::{DmrController, DmrService};
use crate::events::EventSink;
use crate::takeover::PlaybackArbiter;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// 嵌入方传给核心的运行设置。
#[derive(Debug, Clone)]
pub struct Settings {
    /// 广播给控制点的设备名。
    pub name: String,
    /// 会话间持久化文件（DLNA 的设备 MAC 等身份）。
    pub state_path: PathBuf,
}

/// 跨会话保持的本地身份：DLNA 只用到稳定的设备 MAC。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PersistedState {
    /// 上报给控制点的设备 MAC；首次启动时随机生成，之后保持不变。
    pub mac: Option<[u8; 6]>,
}

impl PersistedState {
    /// 读状态文件；不存在或损坏时返回默认值（下次 `save` 会重建）。
    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default()
    }

    /// 写回状态文件；失败只影响身份是否稳定，因此不打断启动。
    pub fn save(&self, path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = fs::write(path, json);
        }
    }
}

/// 只启动 DLNA DMR，并把控制器交给调用方。
///
/// 嵌入方（例如 myplay）自己负责拉流、解码与出声，因此还需要 [`DmrController`] 来
/// 回报「就绪 / 位置 / 速率」：DMR 的 `GetPositionInfo` 由 `renderer_active` 门控，
/// 没有这些回报，控制点读到的位置会一直停在起始值。
pub async fn start_dlna(
    settings: Settings,
    events: Arc<EventSink>,
) -> Result<(DmrService, Arc<DmrController>)> {
    let arbiter = Arc::new(PlaybackArbiter::new(Arc::clone(&events)));
    let mac = {
        let mut persisted = PersistedState::load(&settings.state_path);
        let mac = persisted.mac.unwrap_or_else(|| {
            let mut generated = [0_u8; 6];
            // 本地管理地址：首字节置为 0x02，避免与控制点网段内的真实设备冲突。
            generated[0] = 0x02;
            rand::fill(&mut generated[1..]);
            generated
        });
        persisted.mac = Some(mac);
        persisted.save(&settings.state_path);
        mac
    };
    let service = DmrService::start(settings.name, mac, Arc::clone(&events), arbiter).await?;
    let controller = service.controller();
    Ok((service, controller))
}
