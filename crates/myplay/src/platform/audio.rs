//! 音频输出设备枚举与匹配。
//!
//! 与接收器核心使用同一个后端（cpal），因此这里列出的设备 id/名称可以直接交给
//! `ReceiverConfig::with_output_device` / `cpal` 开流。设备匹配逻辑只在这里实现一份，
//! 通道与播放器都复用它。

use cpal::traits::{DeviceTrait, HostTrait};

/// 一个可选的输出设备。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputDevice {
    /// 稳定的设备 id（`wasapi:{...}`），写入配置用它。
    pub id: String,
    /// 人类可读名称（如「扬声器」「GX329Q」）。
    pub name: String,
}

/// 枚举本机所有输出设备。失败时返回空列表（界面会提示只有「系统默认」可选）。
pub fn list_output_devices() -> Vec<OutputDevice> {
    let host = cpal::default_host();
    let Ok(devices) = host.output_devices() else {
        return Vec::new();
    };

    let mut listed: Vec<OutputDevice> = devices
        .filter_map(|device| {
            let id = device.id().ok()?.to_string();
            let name = device_name(&device).unwrap_or_else(|| id.clone());
            Some(OutputDevice { id, name })
        })
        .collect();
    listed.sort_by(|left, right| left.name.cmp(&right.name));
    listed
}

/// 设备的显示名；拿不到描述时为 `None`。
pub fn device_name(device: &cpal::Device) -> Option<String> {
    device
        .description()
        .ok()
        .map(|description| description.name().to_owned())
}

/// 是否是配置里指定的那台设备：先比稳定 id，再比忽略大小写的名称。
pub fn matches(device: &cpal::Device, wanted: &str) -> bool {
    matches_identity(
        device.id().ok().map(|id| id.to_string()).as_deref(),
        device_name(device).as_deref(),
        wanted,
    )
}

/// [`matches`] 的纯函数内核，便于单测。
fn matches_identity(id: Option<&str>, name: Option<&str>, wanted: &str) -> bool {
    id == Some(wanted) || name.is_some_and(|name| name.eq_ignore_ascii_case(wanted))
}

#[cfg(test)]
mod tests {
    use super::matches_identity;

    #[test]
    fn device_identity_matches_id_then_name() {
        assert!(matches_identity(
            Some("wasapi:{abc}"),
            Some("扬声器"),
            "wasapi:{abc}"
        ));
        // id 不匹配时退回名称，且忽略大小写。
        assert!(matches_identity(
            Some("wasapi:{abc}"),
            Some("GX329Q"),
            "gx329q"
        ));
        assert!(!matches_identity(Some("wasapi:{abc}"), None, "gx329q"));
        assert!(!matches_identity(None, None, "gx329q"));
    }
}
