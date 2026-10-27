//! 妙播接收通道：在本进程内直接启动 MiPlay 接收核心（嵌入，不套外部 exe）。
//!
//! 数据流：界面点「启动」→ [`Receiver::start`] 选网卡、加载身份、`MiPlayReceiver::start`；
//! SDK 事件线程把每条事件（已是 [`serde_json::Value`]）交给 [`crate::core::event::parse_value`]
//! 归一成 `UiEvent` 后投进 [`EventSink`]，Core 侧的任务据此更新状态。

use std::fs;
use std::net::Ipv4Addr;
use std::path::Path;

use anyhow::{Context as _, Result};
use fusionplay_miplay_sdk::{
    MediaAction, MiPlayReceiver, ReceiverConfig, ReceiverController, ReceiverIdentity,
};
use serde_json::Value;

use crate::channels::{EventSink, usable_v4};
use crate::core::config::{Settings, data_dir};
use crate::core::event;

/// 运行中的接收器。丢弃即停止（`MiPlayReceiver::drop` 会关停核心）。
pub struct Receiver {
    /// 播放/暂停等控制入口。
    controller: ReceiverController,
    /// 接收核心本体：字段本身不读，靠生命周期维持广播与监听。
    _core: MiPlayReceiver,
    /// 实际生效的广播信息，供界面展示。
    pub snapshot: Snapshot,
}

/// 启动后确定的广播信息（界面「广播信息」区用）。
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// 对外展示的设备名。
    pub name: String,
    /// 设备类型中文标签。
    pub device_type: &'static str,
    /// 生效的本机 IPv4。
    pub local_ip: String,
    /// 该 IP 所属网卡名。
    pub interface: String,
    /// 广播用的 MAC（真实网卡，拿不到时由身份派生）。
    pub hardware_address: String,
    /// mDNS 服务全名。
    pub mdns_service: String,
    /// Lyra 控制端口。
    pub lyra_port: u16,
}

impl Receiver {
    /// 启动接收器：解析网卡 → 加载/生成身份 → 起核心。
    pub fn start(settings: &Settings, sink: EventSink) -> Result<Self> {
        let (local_ip, interface_name) = resolve_interface(settings)?;
        let identity = load_or_create_identity(&data_dir())?;

        let emitter: fusionplay_miplay_sdk::EventEmitter =
            std::sync::Arc::new(move |value: Value| {
                if let Some(event) = event::parse_value(&value) {
                    let _ = sink.send(event);
                }
            });

        let mut config = ReceiverConfig::new(
            settings.name.clone(),
            identity.clone(),
            local_ip,
            interface_name.clone(),
        )
        .with_device_type(settings.device_type.to_sdk())
        .with_output_device(settings.output_device_opt())
        .with_initial_volume_percent(50);

        // 广播身份固定：真实网卡 MAC（拿不到时退回身份派生值，避免广播空 MAC）。
        let hardware_address = if_addrs::hardware_address_for(std::net::IpAddr::V4(local_ip))
            .ok()
            .flatten()
            .map(format_mac)
            .unwrap_or_else(|| derived_mac(&identity));
        config = config.with_hardware_address(Some(hardware_address.clone()));

        let core = MiPlayReceiver::start(config, emitter)
            .context("启动米播接收器失败（端口被占用或网卡不可用）")?;

        let snapshot = Snapshot {
            name: settings.name.clone(),
            device_type: settings.device_type.label(),
            local_ip: local_ip.to_string(),
            interface: interface_name,
            hardware_address,
            mdns_service: core.service_name().to_owned(),
            lyra_port: core.lyra_command_port(),
        };

        Ok(Self {
            controller: core.controller(),
            _core: core,
            snapshot,
        })
    }

    /// 停止接收（幂等；`Drop` 时也会走到同一个停止路径）。
    pub fn shutdown(&self) {
        self.controller.stop();
    }

    /// 播放 / 暂停。
    pub fn toggle_playback(&self) {
        let _ = self.controller.send(MediaAction::Toggle);
    }
}

/// `AA:BB:CC:DD:EE:FF`（日志与界面用大写，与小米端的展示一致）。
fn format_mac(bytes: [u8; 6]) -> String {
    use std::fmt::Write as _;

    let mut text = String::with_capacity(17);
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 {
            text.push(':');
        }
        let _ = write!(text, "{byte:02X}");
    }
    text
}

/// 派生 MAC：本地管理位 + 身份指纹，避免与厂商按真实 MAC 留下的记录合并。
fn derived_mac(identity: &ReceiverIdentity) -> String {
    let digits = identity
        .lyra_instance_id
        .chars()
        .filter(char::is_ascii_hexdigit)
        .collect::<String>();
    let mut octets = [0x02_u8, 0x4D, 0x50, 0x00, 0x00, 0x01];
    for (index, chunk) in digits.as_bytes().chunks(2).take(3).enumerate() {
        if let Ok(value) = u8::from_str_radix(std::str::from_utf8(chunk).unwrap_or("00"), 16) {
            octets[index + 3] = value;
        }
    }
    format_mac(octets)
}

/// 解析要使用的 (IP, 接口名)，优先用户指定值。
fn resolve_interface(settings: &Settings) -> Result<(Ipv4Addr, String)> {
    let interfaces = if_addrs::get_if_addrs().context("无法枚举本机网络接口")?;

    if let Ok(requested) = settings.local_ip.parse::<Ipv4Addr>() {
        let name = interfaces
            .iter()
            .find(|interface| interface.ip() == std::net::IpAddr::V4(requested))
            .map(|interface| interface.name.clone())
            .unwrap_or_else(|| settings.interface.clone());
        return Ok((requested, name));
    }

    let mut candidates: Vec<(Ipv4Addr, String)> = interfaces
        .iter()
        .filter(|interface| {
            usable_v4(interface)
                && (settings.interface.is_empty() || interface.name == settings.interface)
        })
        .filter_map(|interface| match interface.addr {
            if_addrs::IfAddr::V4(ref v4) => Some((v4.ip, interface.name.clone())),
            if_addrs::IfAddr::V6(_) => None,
        })
        .collect();

    candidates.sort_by_key(|(ip, _)| u8::from(!ip.is_private()));
    candidates
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("没有可用的局域网 IPv4 地址（检查网卡设置）"))
}

/// 读取或生成持久化身份；格式与 FusionPlay 桌面 host 相同，可共用同一份文件。
fn load_or_create_identity(directory: &Path) -> Result<ReceiverIdentity> {
    fs::create_dir_all(directory)
        .with_context(|| format!("无法创建数据目录：{}", directory.display()))?;
    let path = directory.join("miplay-identity-v1.json");
    if let Ok(text) = fs::read_to_string(&path)
        && let Ok(value) = serde_json::from_str::<Value>(&text)
        && let (Some(idm), Some(lyra), Some(media)) = (
            value.get("idm_instance_id").and_then(Value::as_str),
            value.get("lyra_instance_id").and_then(Value::as_str),
            value.get("media_device_id").and_then(Value::as_str),
        )
    {
        return Ok(ReceiverIdentity::new(idm, lyra, media));
    }

    let raw = random_hex(32);
    let idm = format!(
        "{}-{}-{}-{}-{}",
        &raw[0..8],
        &raw[8..12],
        &raw[12..16],
        &raw[16..20],
        &raw[20..32]
    );
    let lyra = random_hex(8);
    let media = random_hex(8);
    let document = serde_json::json!({
        "idm_instance_id": idm,
        "lyra_instance_id": lyra,
        "media_device_id": media,
    });
    fs::write(&path, serde_json::to_string_pretty(&document)?)
        .with_context(|| format!("无法写入身份文件：{}", path.display()))?;
    Ok(ReceiverIdentity::new(idm, lyra, media))
}

/// 生成 `length` 位十六进制串：时间戳 + 随机哈希，仅用于本机身份，不涉安全。
fn random_hex(length: usize) -> String {
    use std::fmt::Write as _;
    use std::hash::{BuildHasher, Hasher};

    let mut out = String::with_capacity(length + 16);
    while out.len() < length {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0),
        );
        let _ = write!(out, "{:016X}", hasher.finish());
    }
    out.truncate(length);
    out
}

#[cfg(test)]
mod tests {
    use super::format_mac;

    #[test]
    fn mac_is_uppercase_colon_separated() {
        assert_eq!(
            format_mac([0x02, 0x4D, 0x50, 0xAB, 0x0C, 0xFF]),
            "02:4D:50:AB:0C:FF"
        );
    }
}
