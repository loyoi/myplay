//! DLNA 接收通道：把融合核心的 DMR 服务嵌进本进程。
//!
//! 与 MiPlay 通道并列（同一时间只跑一个）。核心的 `EventSink` 支持回调，因此事件在这里
//! 直接归一成 [`UiEvent`]（[`translate`] 是核心事件词表的唯一翻译点），Core 无需为协议分支；
//! 同一条原始事件里与播放有关的命令交给 [`Player`]，不再二次解析。

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use fusionplay_core::{EventSink as CoreEventSink, Settings as CoreSettings, start_dlna};
use serde_json::Value;

use crate::channels::EventSink;
use crate::channels::dlna_player::Player;
use crate::core::config::{Settings, data_dir};
use crate::core::event::{UiEvent, number, opt_text, text};

/// 让核心收尾的关停命令（两端都由本仓库构造，用等值比较而不是子串匹配）。
const SHUTDOWN_COMMAND: &str = r#"{"command":"shutdown"}"#;
/// 关服务前等播放器回报 `STOPPED` 的上限：正常只需 1~2 ms，超时就不等了。
const TEARDOWN_WAIT: Duration = Duration::from_millis(300);
/// 拆完服务后再给 GENA 的最后一个通知留点离场时间，然后才丢弃运行时。
const GENA_GRACE: Duration = Duration::from_millis(80);

/// 运行中的 DLNA 接收器。丢弃即请求关停。
pub struct CoreReceiver {
    /// 核心线程的命令入口。
    commands: tokio::sync::mpsc::UnboundedSender<String>,
    /// 跑核心运行时的线程。
    _thread: std::thread::JoinHandle<()>,
    /// 本地播放器：拉流 / 解码 / 出声都在它这里。
    player: Player,
}

impl CoreReceiver {
    pub fn start(settings: &Settings, sink: EventSink) -> Result<Self> {
        let core_settings = CoreSettings {
            name: settings.name.clone(),
            state_path: data_dir().join("dlna-state.json"),
        };

        let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

        // 核心只发事件，实际拉流/解码/出声由本进程的播放器完成。
        let player = Player::spawn(sink.clone(), settings.output_device_opt());
        // 核心启动失败要报到界面：开机自启时网卡可能还没就绪，Core 看到失败态会自己重试；
        // 如果只写控制台日志（release 没有控制台），界面就会一直停在「运行中」。
        let failure_sink = sink.clone();
        let player_for_events = player.clone();
        let events = Arc::new(CoreEventSink::with_callback(Arc::new(
            move |raw: String| {
                let Ok(value) = serde_json::from_str::<Value>(&raw) else {
                    return;
                };
                player_for_events.dispatch(&value);
                if let Some(event) = translate(&value) {
                    let _ = sink.send(event);
                }
            },
        )));

        let player_for_thread = player.clone();

        let thread = std::thread::Builder::new()
            .name("myplay-core".to_owned())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let message = format!("DLNA 核心运行时启动失败：{error}");
                        log::error!("{message}");
                        let _ = failure_sink.send(UiEvent::Error { message });
                        return;
                    }
                };
                runtime.block_on(async move {
                    // 只起 DMR 控制层，音频由本进程播放器负责；因此必须把控制器交给播放器
                    // 回填位置，否则控制点读不到进度。
                    match start_dlna(core_settings, Arc::clone(&events)).await {
                        Ok((service, controller)) => {
                            player_for_thread.attach_controller(controller);
                            let mut commands = command_rx;
                            while let Some(command) = commands.recv().await {
                                if command == SHUTDOWN_COMMAND {
                                    break;
                                }
                            }
                            // 顺序要紧：先等播放器把 `STOPPED / ready=false` 经 GENA 报给控制点，
                            // 再拆服务。反过来的话运行时已经没了，那条通知发不出去，
                            // 控制点就会一直以为还在播放，直到它自己的超时。
                            let teardown = Instant::now();
                            let confirmed = player_for_thread.wait_stopped(TEARDOWN_WAIT);
                            // 必须 await 收尾：`Drop` 只发信号，任务来不及跑，
                            // SSDP 的 `ssdp:byebye`（以及 GENA 通知）就不会发出去。
                            service.stop().await;
                            tokio::time::sleep(GENA_GRACE).await;
                            log::info!(
                                "DLNA 通道已收尾：{} ms（播放器停止确认：{}）",
                                teardown.elapsed().as_millis(),
                                if confirmed { "是" } else { "超时未确认" }
                            );
                        }
                        Err(error) => {
                            let message = format!("DLNA 服务未就绪：{error:#}");
                            log::error!("{message}");
                            let _ = failure_sink.send(UiEvent::Error { message });
                        }
                    }
                });
            })
            .context("无法创建核心通道线程")?;

        Ok(Self {
            commands: command_tx,
            _thread: thread,
            player,
        })
    }

    /// 播放/暂停：本地播放器说了算（DLNA 的传输控制由控制点发起）。
    pub fn toggle_playback(&self) {
        self.player.toggle();
    }

    /// 停止接收：先关本地播放器（出声的是它），再请求核心收尾。
    ///
    /// 顺序很重要：先让播放器回报 `rate=0 / ready=false`，控制点（如网易云）才会看到
    /// 传输状态变成 STOPPED 并自行暂停界面。
    pub fn shutdown(&self) {
        self.player.shutdown();
        let _ = self.commands.send(SHUTDOWN_COMMAND.to_owned());
    }
}

impl Drop for CoreReceiver {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// 核心事件 → 界面事件：核心事件词表的唯一翻译点。
///
/// 未识别的类型统一转成 [`UiEvent::Core`]，至少能在日志页看到类型与消息，而不是静默丢弃。
fn translate(value: &Value) -> Option<UiEvent> {
    let kind = value.get("type").and_then(Value::as_str)?;
    Some(match kind {
        "receiver_ready" => UiEvent::Ready {
            name: text(value, "name"),
            address: text(value, "device_id"),
        },
        "output_device" => UiEvent::OutputReady {
            name: text(value, "name"),
        },
        "stream_started" => UiEvent::StreamStarted {
            codec: text(value, "source_codec").to_uppercase(),
            sample_rate: number(value, "source_sample_rate").unwrap_or(0),
        },
        "stream_stopped" | "dlna_stop" => UiEvent::StreamClosed,
        "now_playing" => UiEvent::Media {
            title: text(value, "title"),
            artist: text(value, "artist"),
            album: text(value, "album"),
            duration_ms: 0,
            media_url: None,
            artwork_url: None,
            position_ms: None,
        },
        "progress" => UiEvent::Progress {
            position_ms: number(value, "position_ms").unwrap_or(0),
            duration_ms: number(value, "duration_ms"),
        },
        "playback_state" => UiEvent::Playback {
            playing: value
                .get("playing")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        "volume" | "dlna_volume" => UiEvent::Volume {
            percent: number(value, "percent")
                .and_then(|percent| u32::try_from(percent).ok())
                .unwrap_or(0),
        },
        "error" => UiEvent::Error {
            message: text(value, "message"),
        },
        "status" => UiEvent::Core {
            kind: "status".to_owned(),
            message: text(value, "message"),
        },
        "dlna_ready" => UiEvent::Ready {
            name: "DLNA".to_owned(),
            address: text(value, "device_uuid"),
        },
        "dlna_media" => UiEvent::Media {
            title: text(value, "title"),
            artist: text(value, "artist"),
            album: text(value, "album"),
            duration_ms: number(value, "duration_ms").unwrap_or(0),
            media_url: opt_text(value, "url"),
            artwork_url: opt_text(value, "artwork_url"),
            position_ms: number(value, "start_position_ms"),
        },
        "dlna_rate" => UiEvent::Playback {
            playing: value.get("rate").and_then(Value::as_f64).unwrap_or(0.0) > 0.0,
        },
        "dlna_seek" => UiEvent::Progress {
            position_ms: number(value, "position_ms").unwrap_or(0),
            duration_ms: None,
        },
        "client_connected" => UiEvent::Stage {
            remote: opt_text(value, "address"),
            stage: "transport_connected".to_owned(),
        },
        "client_disconnected" => UiEvent::Stage {
            remote: None,
            stage: "transport_disconnected".to_owned(),
        },
        _ => UiEvent::Core {
            kind: kind.to_owned(),
            message: text(value, "message"),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::translate;
    use crate::core::event::UiEvent;
    use serde_json::json;

    #[test]
    fn translates_core_events_into_ui_events() {
        assert_eq!(
            translate(&json!({"type": "receiver_ready", "name": "N", "device_id": "D"})),
            Some(UiEvent::Ready {
                name: "N".to_owned(),
                address: "D".to_owned()
            })
        );
        assert_eq!(
            translate(&json!({"type": "dlna_rate", "rate": 1.0})),
            Some(UiEvent::Playback { playing: true })
        );
        assert_eq!(
            translate(&json!({"type": "dlna_stop"})),
            Some(UiEvent::StreamClosed)
        );
        assert_eq!(
            translate(&json!({"type": "client_disconnected"})),
            Some(UiEvent::Stage {
                remote: None,
                stage: "transport_disconnected".to_owned()
            })
        );
    }

    #[test]
    fn unknown_core_events_stay_visible_in_the_log() {
        assert_eq!(
            translate(&json!({"type": "something_new", "message": "hi"})),
            Some(UiEvent::Core {
                kind: "something_new".to_owned(),
                message: "hi".to_owned()
            })
        );
        // 没有 type 字段的事件无法归类。
        assert_eq!(translate(&json!({"message": "hi"})), None);
    }

    #[test]
    fn status_events_are_not_dropped() {
        // 旧实现把 `status` 翻译成 `{"event":"status"}`，而 `core::event::parse` 没有这个分支，
        // 于是一整类核心消息被静默丢弃。现在它必须落到 `Core`。
        assert_eq!(
            translate(&json!({"type": "status", "message": "缓冲中"})),
            Some(UiEvent::Core {
                kind: "status".to_owned(),
                message: "缓冲中".to_owned()
            })
        );
    }
}
