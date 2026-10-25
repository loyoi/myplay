//! 配置模型与持久化。
//!
//! 数据流：`main` 启动时 [`Settings::load`] 读一次 `%LOCALAPPDATA%\myplay\settings.json`，
//! 之后由 `core::Core` 持有唯一副本；界面上每次改动立刻 `save`，因此托盘「启动」用的
//! 始终是最后一次确认的配置。

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use fusionplay_miplay_sdk::MiPlayDeviceType;
use serde::{Deserialize, Serialize};

/// 接收器在小米端展示的设备类型（协议取值的稳定英文标识）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeviceKind {
    #[default]
    Speaker,
    DisplaySpeaker,
    Television,
    Tablet,
    Vehicle,
}

impl DeviceKind {
    pub const ALL: [Self; 5] = [
        Self::Speaker,
        Self::DisplaySpeaker,
        Self::Television,
        Self::Tablet,
        Self::Vehicle,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Speaker => "音箱",
            Self::DisplaySpeaker => "带屏音箱",
            Self::Television => "电视",
            Self::Tablet => "平板",
            Self::Vehicle => "车机",
        }
    }

    pub const fn to_sdk(self) -> MiPlayDeviceType {
        match self {
            Self::Speaker => MiPlayDeviceType::Speaker,
            Self::DisplaySpeaker => MiPlayDeviceType::DisplaySpeaker,
            Self::Television => MiPlayDeviceType::Television,
            Self::Tablet => MiPlayDeviceType::Tablet,
            Self::Vehicle => MiPlayDeviceType::Vehicle,
        }
    }
}

/// 启动时使用的接收通道。
///
/// AirPlay 通道已移除：旧的 `settings.json` 里若还留着 `"source": "airplay"`，
/// 通过下面的 `alias` 直接读成默认源，避免整份配置被判定为解析失败而重置。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    /// 小米妙播（MiPlay，AAC 48 kHz）。
    #[default]
    #[serde(alias = "airplay")]
    MiPlay,
    /// DLNA（任意 DLNA 播放器 / NAS，可直传 FLAC/WAV）。
    Dlna,
}

impl Source {
    pub const ALL: [Self; 2] = [Self::MiPlay, Self::Dlna];

    pub const fn label(self) -> &'static str {
        match self {
            Self::MiPlay => "小米妙播",
            Self::Dlna => "DLNA",
        }
    }

    pub const fn hint(self) -> &'static str {
        match self {
            Self::MiPlay => "AAC-LC 48 kHz · 小米/红米设备投放",
            Self::Dlna => "FLAC/WAV 可无损 · 任意 DLNA 播放器 / NAS",
        }
    }
}

/// 关闭窗口的行为。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CloseAction {
    /// 收到托盘。
    #[default]
    ToTray,
    /// 直接退出。
    Quit,
}

impl CloseAction {
    pub const ALL: [Self; 2] = [Self::ToTray, Self::Quit];

    pub const fn label(self) -> &'static str {
        match self {
            Self::ToTray => "最小化到托盘",
            Self::Quit => "退出程序",
        }
    }
}

/// 用户可配置项。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// 启动时使用的接收通道。
    pub source: Source,
    /// 广播给对端看的设备名。
    pub name: String,
    /// 广播的设备类型（小米端展示的图标/分类）。
    pub device_type: DeviceKind,
    /// 手动指定的网卡名；空表示自动选择。
    pub interface: String,
    /// 手动指定的本机 IPv4；空表示随机取所选网卡地址。
    pub local_ip: String,
    /// 音频输出设备名（cpal 设备 id）；空表示系统默认。
    pub output_device: String,
    /// 是否开机自启（`HKCU\...\Run` 下的 `myplay` 值）。
    pub start_with_windows: bool,
    /// 启动后是否只驻留托盘（不打开主窗口）；点托盘图标再打开。
    pub start_minimized: bool,
    /// 上次关软件时接收器在运行：下次启动按这个状态还原（启停按钮自动记下）。
    pub restore_running: bool,
    /// 关闭窗口时的行为。
    pub close_action: CloseAction,
    /// 是否输出 debug 级日志（下次启动生效）。
    pub verbose_log: bool,
    /// 事件日志落盘路径；空表示不写文件。
    pub log_file: String,
    /// 需要发 goodbye 清理的历史 Lyra 实例（逗号分隔）。
    pub retire_lyra: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            source: Source::default(),
            name: "myplay".to_owned(),
            device_type: DeviceKind::Speaker,
            interface: String::new(),
            local_ip: String::new(),
            output_device: String::new(),
            start_with_windows: false,
            start_minimized: false,
            restore_running: false,
            close_action: CloseAction::default(),
            verbose_log: false,
            log_file: String::new(),
            retire_lyra: String::new(),
        }
    }
}

/// 应用数据目录：`%LOCALAPPDATA%\myplay`。
pub fn data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("myplay")
}

pub fn settings_path() -> PathBuf {
    data_dir().join("settings.json")
}

impl Settings {
    pub fn load() -> Self {
        let path = settings_path();
        match fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Self>(&text) {
                Ok(settings) => settings.normalized(),
                Err(error) => {
                    log::warn!("配置解析失败（{error}），使用默认值：{}", path.display());
                    Self::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                log::warn!("配置读取失败（{error}）：{}", path.display());
                Self::default()
            }
        }
    }

    pub fn save(&self) {
        if let Err(error) = self.write_to(&settings_path()) {
            log::warn!("配置保存失败：{error:#}");
        }
    }

    /// 输出设备配置的借用形态：空串统一成 `None`，交给通道/核心时不必各写一遍判断。
    pub fn output_device_opt(&self) -> Option<String> {
        (!self.output_device.is_empty()).then(|| self.output_device.clone())
    }

    fn write_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("无法创建配置目录：{}", parent.display()))?;
        }
        fs::write(path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("无法写入配置：{}", path.display()))
    }

    fn normalized(mut self) -> Self {
        self.name = self.name.trim().to_owned();
        if self.name.is_empty() {
            self.name = Self::default().name;
        }
        self.interface = self.interface.trim().to_owned();
        self.local_ip = self.local_ip.trim().to_owned();
        self.output_device = self.output_device.trim().to_owned();
        self.log_file = self.log_file.trim().to_owned();
        self.retire_lyra = self.retire_lyra.trim().to_owned();
        self
    }

    /// 导出给 SDK 的环境变量。
    ///
    /// 广播身份是固定的：只公布 Mi Connect 一条记录（单条目），并使用真实网卡 MAC。
    /// 单条目避免小米端把两条 mDNS 记录显示成两台设备；真实 MAC 让小米端把它与既有
    /// 设备记录合并成同一个目标，而不是多出一条。
    ///
    /// **调用时机**：必须在 `main` 里、任何线程启动之前调用一次。
    /// `std::env::set_var` 在多线程下是未定义行为，接收器运行中切换配置不会重新调用它。
    pub fn apply_environment(&self) {
        // SAFETY: 由 `main` 在托盘线程 / GPUI / 接收器线程启动之前调用，此时进程是单线程的。
        unsafe {
            std::env::set_var("FUSIONPLAY_MIPLAY_DISABLE_LYRA_DISCOVERY", "1");
            let retire = self
                .retire_lyra
                .split([',', ' ', ';'])
                .map(str::trim)
                .filter(|value| value.len() == 8 && value.bytes().all(|b| b.is_ascii_hexdigit()))
                .collect::<Vec<_>>()
                .join(",");
            if retire.is_empty() {
                std::env::remove_var("FUSIONPLAY_MIPLAY_RETIRED_LYRA_INSTANCES");
            } else {
                std::env::set_var("FUSIONPLAY_MIPLAY_RETIRED_LYRA_INSTANCES", retire);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Settings;

    #[test]
    fn empty_output_device_means_system_default() {
        let mut settings = Settings::default();
        assert_eq!(settings.output_device_opt(), None);
        settings.output_device = "wasapi:{abc}".to_owned();
        assert_eq!(
            settings.output_device_opt().as_deref(),
            Some("wasapi:{abc}")
        );
    }

    #[test]
    fn normalized_trims_and_restores_empty_name() {
        let settings = Settings {
            name: "   ".to_owned(),
            interface: "  WLAN  ".to_owned(),
            log_file: " C:/a.log ".to_owned(),
            ..Settings::default()
        }
        .normalized();
        assert_eq!(settings.name, Settings::default().name);
        assert_eq!(settings.interface, "WLAN");
        assert_eq!(settings.log_file, "C:/a.log");
    }
}
