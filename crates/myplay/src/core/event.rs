//! 事件归一：把三条通道的原始事件解析成 [`UiEvent`]。
//!
//! 数据流：妙播通道的 SDK 直接产出 JSON（这里 [`parse`] / [`parse_value`]），DLNA
//! 的核心事件由 `channels::remote_core::translate` 归一到同一个枚举。上层（Core / UI / 托盘）
//! 只 match [`UiEvent`]，不再各自解析 JSON 字段。

use serde_json::Value;

/// 归一后的事件。
#[derive(Clone, Debug, PartialEq)]
pub enum UiEvent {
    /// 通道已上线。
    Ready { name: String, address: String },
    /// 对端互联阶段（妙播）。
    Stage {
        remote: Option<String>,
        stage: String,
    },
    /// 对端设备名。
    Source { name: String },
    /// 输出设备就绪。
    OutputReady { name: String },
    /// 开始接收媒体流。
    StreamStarted { codec: String, sample_rate: u64 },
    /// 编码参数（码率等）。
    AudioFormat { bitrate: u64 },
    /// 曲目元数据。
    Media {
        title: String,
        artist: String,
        album: String,
        duration_ms: u64,
        media_url: Option<String>,
        artwork_url: Option<String>,
        /// 部分通道在元数据里直接给出起播位置。
        position_ms: Option<u64>,
    },
    /// 播放进度。
    Progress {
        position_ms: u64,
        duration_ms: Option<u64>,
    },
    /// 播放/暂停。
    Playback { playing: bool },
    /// 音量百分比。
    Volume { percent: u32 },
    /// 媒体流结束。
    StreamClosed,
    /// 错误。
    Error { message: String },
    /// 警告。
    Warning { message: String },
    /// 其余核心事件（类型 + 消息），用于日志页。
    Core { kind: String, message: String },
}

/// 解析已经解析成 [`Value`] 的事件（通道拿到 `Value` 时不再绕一次字符串）。
pub fn parse_value(value: &Value) -> Option<UiEvent> {
    let event = value.get("event").and_then(Value::as_str)?;
    let text = |key: &str| text(value, key);
    let number = |key: &str| number(value, key);

    Some(match event {
        "receiver_ready" => UiEvent::Ready {
            name: text("name"),
            address: text("address"),
        },
        "interconnect_stage" => UiEvent::Stage {
            remote: opt_text(value, "remote"),
            stage: text("stage"),
        },
        "source_info" => UiEvent::Source {
            name: text("source_name"),
        },
        "audio_output_ready" => UiEvent::OutputReady {
            name: text("device_name"),
        },
        "stream_started" => UiEvent::StreamStarted {
            codec: text("source_codec").to_uppercase(),
            sample_rate: number("source_sample_rate").unwrap_or(0),
        },
        "audio_format" => UiEvent::AudioFormat {
            bitrate: number("bitrate").unwrap_or(0),
        },
        "media_info" => UiEvent::Media {
            title: text("title"),
            artist: text("artist"),
            album: text("album"),
            duration_ms: number("duration_ms").unwrap_or(0),
            media_url: opt_text(value, "media_url"),
            artwork_url: opt_text(value, "artwork_url"),
            position_ms: number("position_ms"),
        },
        "progress" => UiEvent::Progress {
            position_ms: number("position_ms").unwrap_or(0),
            duration_ms: number("duration_ms"),
        },
        "playback_state" => UiEvent::Playback {
            playing: value
                .get("playing")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        "volume" => UiEvent::Volume {
            // 越界值直接丢弃，不做静默截断。
            percent: number("percent")
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(0),
        },
        "media_channel_closed" => UiEvent::StreamClosed,
        "error" => UiEvent::Error {
            message: text("message"),
        },
        "warning" => UiEvent::Warning {
            message: text("message"),
        },
        "core" => UiEvent::Core {
            kind: text("type"),
            message: text("message"),
        },
        _ => return None,
    })
}

/// 取一个可选字符串字段（缺失、`null`、非字符串都视为 `None`）。
pub fn opt_text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// 取一个字符串字段，缺失时为空串。
pub fn text(value: &Value, key: &str) -> String {
    opt_text(value, key).unwrap_or_default()
}

/// 取一个无符号整数字段，缺失或类型不符时为 `None`。
pub fn number(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

#[cfg(test)]
mod tests {
    use super::{UiEvent, parse_value};
    use serde_json::json;

    /// 测试用：从 JSON 文本走一遍值解析。
    fn parse(text: &str) -> Option<UiEvent> {
        parse_value(&serde_json::from_str(text).ok()?)
    }

    #[test]
    fn parses_media_info_with_optional_fields() {
        let json =
            r#"{"event":"media_info","title":"A","artist":"B","album":"C","duration_ms":1000}"#;
        assert_eq!(
            parse(json),
            Some(UiEvent::Media {
                title: "A".to_owned(),
                artist: "B".to_owned(),
                album: "C".to_owned(),
                duration_ms: 1000,
                media_url: None,
                artwork_url: None,
                position_ms: None,
            })
        );
    }

    #[test]
    fn parses_error_and_warning() {
        assert_eq!(
            parse(r#"{"event":"error","message":"boom"}"#),
            Some(UiEvent::Error {
                message: "boom".to_owned()
            })
        );
        assert_eq!(
            parse(r#"{"event":"warning","message":"careful"}"#),
            Some(UiEvent::Warning {
                message: "careful".to_owned()
            })
        );
    }

    #[test]
    fn ignores_unknown_events() {
        assert_eq!(parse(r#"{"event":"audio_pcm_decoded","peak":0.1}"#), None);
        assert_eq!(parse("not json"), None);
    }

    #[test]
    fn volume_out_of_range_is_not_truncated() {
        // 2^32 超出 u32 范围：旧实现会 `as u32` 静默回绕，这里必须是 0。
        let event = json!({"event": "volume", "percent": 4_294_967_296_u64});
        assert_eq!(
            super::parse_value(&event),
            Some(UiEvent::Volume { percent: 0 })
        );
        let valid = json!({"event": "volume", "percent": 42});
        assert_eq!(
            super::parse_value(&valid),
            Some(UiEvent::Volume { percent: 42 })
        );
    }
}
