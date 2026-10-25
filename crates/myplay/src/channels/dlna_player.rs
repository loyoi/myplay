//! DLNA 音频播放：拉流 → symphonia 解码 → 重采样到设备采样率 → cpal 输出。
//!
//! 关键设计：**命令线程只做调度，解码在独立线程里跑**。每首歌/每次操作都会自增
//! 「代次」（generation），解码线程每写一批采样就核对代次，不匹配立即退出——这样
//! Stop / 换曲 / seek 都能立刻生效，而不是等整首放完。
//!
//! 两个线程的接触面只有 [`Shared`]：解码线程生产采样、cpal 回调消费采样。回调里
//! **不分配内存、不打开文件、不做字符串处理**，缓冲腾挪靠 [`Condvar`] 通知而不是轮询。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use fusionplay_core::DmrController;
use symphonia::core::audio::{Channels, GenericAudioBufferRef};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoderOptions};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Time;

use crate::channels::EventSink;
use crate::core::event::{UiEvent, opt_text};
use crate::platform::audio;

/// 环形缓冲容量（毫秒）：够吸收解码抖动，又不至于让 Stop 之后还有明显余音。
const BUFFER_MS: usize = 400;
/// 缓冲满时的最长等待；超时后重新核对代次（防止漏唤醒导致线程卡死）。
const FULL_WAIT: Duration = Duration::from_millis(20);
/// 进度回报间隔（毫秒）。
const PROGRESS_REPORT_MS: u64 = 500;
/// 连续多少个音频包解码失败就判定这条流已失效（单个坏包仍然跳过）。
const DECODE_FAILURE_LIMIT: u32 = 8;
/// 流式补偿（跳过/缓存）的块读大小。
const HTTP_CHUNK: usize = 16 * 1024;
/// 服务端忽略 Range 时允许整包缓存到内存的上限。超过它就退回「本地跳字节」，
/// 以免一条长视频/大无损把内存吃光。
const STREAM_CACHE_LIMIT: u64 = 64 * 1024 * 1024;

/// 播放器句柄。
#[derive(Clone)]
pub struct Player {
    /// 命令入口（Load / Pause / Resume / Seek / Stop / Exit）。
    tx: flume::Sender<Command>,
    /// 与解码线程、音频回调共享的播放状态。
    shared: Arc<Shared>,
}

/// 当前媒体的全部信息：url / mime / 元数据只存这一份，Load 与 Seek 都从这里取。
#[derive(Clone, Debug, Default)]
struct Track {
    /// 拉流地址。
    url: String,
    /// 控制点在 DIDL 里声明的 mime（只当 probe 提示用；HTTP 头的 Content-Type 另外打日志）。
    mime: Option<String>,
    /// 曲名。
    title: Option<String>,
    /// 艺术家。
    artist: Option<String>,
    /// 专辑。
    album: Option<String>,
    /// 控制点在 DIDL-Lite 里声明的质量（不参与播放，只用于诊断日志对比）。
    declared: DeclaredQuality,
}

/// 控制点声明的质量参数（`dlna_media` 事件里的 `bitrate_bps` / `sample_rate` / …）。
///
/// 这些值只做诊断：真正显示给用户的是我们拉流后实测出来的编码与码率。
/// 两侧对不上时，就能区分「发送端只给了这个质量」和「我们显示错了」。
#[derive(Clone, Copy, Debug, Default)]
struct DeclaredQuality {
    /// `<res bitrate>`（bps）。
    bitrate_bps: Option<u64>,
    /// `<res sampleFrequency>`（Hz）。
    sample_rate: Option<u64>,
    /// `<res bitsPerSample>`。
    bits_per_sample: Option<u64>,
    /// `<res nrAudioChannels>`。
    channels: Option<u64>,
    /// `<res duration>`（毫秒）。
    duration_ms: Option<u64>,
}

impl DeclaredQuality {
    /// 从核心的 `dlna_media` 事件里读出声明质量；缺字段留 `None`。
    fn from_event(value: &serde_json::Value) -> Self {
        let field = |key: &str| value.get(key).and_then(serde_json::Value::as_u64);
        Self {
            bitrate_bps: field("bitrate_bps"),
            sample_rate: field("sample_rate"),
            bits_per_sample: field("bits_per_sample"),
            channels: field("channels"),
            duration_ms: field("duration_ms"),
        }
    }
}

/// 诊断用单行文本（缺失字段写 `-`）；直接 `{}` 输出，不额外分配。
impl std::fmt::Display for DeclaredQuality {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "码率={} 采样率={} 位深={} 声道={} 时长={}ms",
            fmt_opt(self.bitrate_bps),
            fmt_opt(self.sample_rate),
            fmt_opt(self.bits_per_sample),
            fmt_opt(self.channels),
            fmt_opt(self.duration_ms),
        )
    }
}

/// 播放线程命令。
enum Command {
    /// 换曲并从 `start_ms` 起播。
    Load {
        /// 新曲目的媒体信息。
        track: Track,
        /// 起播位置（毫秒）。
        start_ms: u64,
    },
    /// 暂停输出（保留缓冲与位置）。
    Pause,
    /// 恢复输出。
    Resume,
    /// 跳到指定毫秒。
    Seek(u64),
    /// 停止当前媒体并清空缓冲。
    Stop,
    /// 关停播放线程。
    Exit,
}

/// 回报给 DMR 的播放状态（控制点读 GetPositionInfo 用）。
#[derive(Clone, Copy, Debug)]
struct StateReport {
    /// 播放位置（毫秒）；`None` 表示本次不更新。
    position_ms: Option<u64>,
    /// 总时长（毫秒）；`None` 表示本次不更新。
    duration_ms: Option<u64>,
    /// 播放速率，0.0 表示暂停。
    rate: f32,
    /// 传输是否就绪。
    ready: bool,
}

impl StateReport {
    /// 换曲起播。
    const fn started(position_ms: u64) -> Self {
        Self {
            position_ms: Some(position_ms),
            duration_ms: None,
            rate: 1.0,
            ready: true,
        }
    }

    /// 解码进度。
    const fn progress(position_ms: u64, duration_ms: u64) -> Self {
        Self {
            position_ms: Some(position_ms),
            duration_ms: Some(duration_ms),
            rate: 1.0,
            ready: true,
        }
    }

    /// 暂停。
    const fn paused() -> Self {
        Self {
            position_ms: None,
            duration_ms: None,
            rate: 0.0,
            ready: true,
        }
    }

    /// 从暂停恢复。
    const fn resumed() -> Self {
        Self {
            position_ms: None,
            duration_ms: None,
            rate: 1.0,
            ready: true,
        }
    }

    /// 停止（`ready = false` 让控制点看到 STOPPED）。
    const fn stopped() -> Self {
        Self {
            position_ms: None,
            duration_ms: None,
            rate: 0.0,
            ready: false,
        }
    }
}

/// 播放状态（跨线程共享；除采样缓冲外都是原子量或低频锁）。
struct Shared {
    /// 采样缓冲（解码线程生产，音频回调消费）。
    sink: Mutex<Sink>,
    /// 缓冲腾出空间时唤醒解码线程。
    drained: Condvar,
    /// 代次：任何会让当前解码失效的操作都会自增。
    generation: AtomicU64,
    /// 是否暂停（音频回调每帧读，用 `Relaxed` 即可）。
    paused: AtomicBool,
    /// 输出流的采样率（Hz）；0 表示输出流还没建好。解码线程据此重采样。
    output_rate: AtomicU32,
    /// 播放线程是否已处理完 `Stop`（已回报 DMR、已发媒体流结束）。
    ///
    /// 收尾顺序用：GENA 的 `STOPPED` 通知必须在核心运行时还活着时发出，
    /// 否则控制点会一直以为在播放。
    stop_reported: AtomicBool,
    /// 当前媒体（seek 时重连要用）。
    track: Mutex<Option<Track>>,
    /// DLNA DMR 控制器 + 核心线程的 tokio 句柄（控制器内部要用 tokio 发 GENA 通知）。
    controller: Mutex<Option<(Arc<DmrController>, tokio::runtime::Handle)>>,
}

impl Shared {
    fn new() -> Self {
        Self {
            sink: Mutex::new(Sink::default()),
            drained: Condvar::new(),
            generation: AtomicU64::new(0),
            paused: AtomicBool::new(false),
            output_rate: AtomicU32::new(0),
            stop_reported: AtomicBool::new(false),
            track: Mutex::new(None),
            controller: Mutex::new(None),
        }
    }

    /// 作废当前解码代次并清空缓冲，返回新代次（Stop / 换曲 / seek 共用这一条路径）。
    fn invalidate(&self) -> u64 {
        // 新的一代（含换曲）还没收尾，重置关停确认。
        self.stop_reported.store(false, Ordering::Relaxed);
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        if let Ok(mut sink) = self.sink.lock() {
            sink.buffer.clear();
        }
        // 等待中的旧解码线程需要醒来核对代次。
        self.drained.notify_all();
        generation
    }

    /// 该代次是否仍然有效。
    fn is_current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::Acquire) == generation
    }

    /// 输出流的采样率（Hz）；建流前返回 `None`。
    fn output_rate(&self) -> Option<u64> {
        let rate = self.output_rate.load(Ordering::Acquire);
        (rate > 0).then(|| u64::from(rate))
    }
}

impl Player {
    /// 启动播放线程。`output_device` 为设置里选择的输出设备（id 或名称）。
    pub fn spawn(events: EventSink, output_device: Option<String>) -> Self {
        let (tx, rx) = flume::unbounded();
        let shared = Arc::new(Shared::new());
        let shared_for_run = Arc::clone(&shared);
        let events_for_panic = events.clone();
        let spawned = std::thread::Builder::new()
            .name("myplay-dlna-player".to_owned())
            .spawn(move || {
                // panic 会静默杀掉播放线程（表现就是“没声音但状态正常”），这里兜住并报给界面。
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(rx, events, output_device, shared_for_run)
                }));
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => log::error!("DLNA 播放线程退出：{error:#}"),
                    Err(_) => {
                        let _ = events_for_panic.send(UiEvent::Error {
                            message: "DLNA 播放线程崩溃（panic），详见控制台日志".to_owned(),
                        });
                    }
                }
            });
        if let Err(error) = spawned {
            log::error!("无法启动 DLNA 播放线程：{error}");
        }
        Self { tx, shared }
    }

    /// 接管 DLNA 控制器：解码/暂停/停止时把位置与速率回报给 DMR，
    /// 这样控制点（如网易云）读到的进度才会跟着走。
    pub fn attach_controller(&self, controller: Arc<DmrController>) {
        // 必须在核心的 tokio 上下文里调用（`start_dlna` 返回处），这样才能拿到 Handle；
        // 播放器线程没有运行时，直接调用控制器会 panic。
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            log::warn!("attach_controller 未在 tokio 上下文中调用，DLNA 进度不会回填");
            return;
        };
        if let Ok(mut slot) = self.shared.controller.lock() {
            *slot = Some((controller, handle));
        }
    }

    /// 停止并关停播放线程：先让解码失效、回报「已停止」，再退出命令循环。
    pub fn shutdown(&self) {
        let _ = self.tx.send(Command::Stop);
        let _ = self.tx.send(Command::Exit);
    }

    /// 等待播放线程确认「已停止」（`timeout` 为上限）。
    ///
    /// 只给核心线程拆服务前排队用（GENA 需要运行时活着），不要在界面线程调用。
    pub fn wait_stopped(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.shared.stop_reported.load(Ordering::Acquire) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        self.shared.stop_reported.load(Ordering::Acquire)
    }

    /// 播放/暂停切换（托盘菜单与窗口按钮用）。
    pub fn toggle(&self) {
        let paused = self.shared.paused.load(Ordering::Relaxed);
        let _ = self.tx.send(if paused {
            Command::Resume
        } else {
            Command::Pause
        });
    }

    /// 把一条核心事件翻译成播放命令。
    pub fn dispatch(&self, value: &serde_json::Value) {
        match value
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
        {
            "dlna_media" => {
                let Some(url) = text_field(value, "url") else {
                    return;
                };
                let track = Track {
                    url,
                    mime: text_field(value, "content_type"),
                    title: text_field(value, "title"),
                    artist: text_field(value, "artist"),
                    album: text_field(value, "album"),
                    declared: DeclaredQuality::from_event(value),
                };
                let start_ms = value
                    .get("start_position_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let _ = self.tx.send(Command::Load { track, start_ms });
            }
            "dlna_rate" => {
                let rate = value.get("rate").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let _ = self.tx.send(if rate > 0.0 {
                    Command::Resume
                } else {
                    Command::Pause
                });
            }
            "dlna_seek" => {
                if let Some(position) = value.get("position_ms").and_then(|v| v.as_u64()) {
                    let _ = self.tx.send(Command::Seek(position));
                }
            }
            "dlna_stop" | "transport_stopped" => {
                let _ = self.tx.send(Command::Stop);
            }
            _ => {}
        }
    }
}

fn run(
    commands: flume::Receiver<Command>,
    events: EventSink,
    output_device: Option<String>,
    shared: Arc<Shared>,
) -> Result<()> {
    let (stream, device_name) = match build_stream(Arc::clone(&shared), output_device.as_deref()) {
        Ok(value) => value,
        Err(error) => {
            emit(
                &events,
                UiEvent::Error {
                    message: format!("DLNA 音频输出初始化失败：{error:#}"),
                },
            );
            return Err(error);
        }
    };
    if let Err(error) = stream.play() {
        emit(
            &events,
            UiEvent::Error {
                message: format!("无法启动音频输出：{error}"),
            },
        );
        return Err(error).context("无法启动音频输出");
    }
    emit(&events, UiEvent::OutputReady { name: device_name });

    while let Ok(command) = commands.recv() {
        match command {
            Command::Load { track, start_ms } => {
                // 立刻作废上一首的解码线程，并清空缓冲（停止后不再有声音）。
                let generation = shared.invalidate();
                shared.paused.store(false, Ordering::Relaxed);
                if let Ok(mut slot) = shared.track.lock() {
                    *slot = Some(track.clone());
                }
                report(&shared, StateReport::started(start_ms));
                spawn_decode(&shared, generation, track, &events, start_ms);
            }
            Command::Pause => {
                shared.paused.store(true, Ordering::Relaxed);
                report(&shared, StateReport::paused());
                emit(&events, UiEvent::Playback { playing: false });
            }
            Command::Resume => {
                shared.paused.store(false, Ordering::Relaxed);
                report(&shared, StateReport::resumed());
                emit(&events, UiEvent::Playback { playing: true });
            }
            Command::Seek(position_ms) => {
                let track = shared.track.lock().ok().and_then(|slot| slot.clone());
                let Some(track) = track else {
                    emit(
                        &events,
                        UiEvent::Warning {
                            message: "DLNA seek：当前没有已加载的媒体".to_owned(),
                        },
                    );
                    continue;
                };
                let generation = shared.invalidate();
                spawn_decode(&shared, generation, track, &events, position_ms);
            }
            Command::Stop => {
                // 代次自增即可让解码线程在下一批采样时退出。
                shared.invalidate();
                if let Ok(mut slot) = shared.track.lock() {
                    *slot = None;
                }
                report(&shared, StateReport::stopped());
                emit(&events, UiEvent::StreamClosed);
                // 通知等待收尾的线程：可以安全拆掉 DMR 服务了。
                shared.stop_reported.store(true, Ordering::Release);
            }
            Command::Exit => break,
        }
    }
    Ok(())
}

/// 起一条解码线程。代次失效后它自己退出，所以旧线程不需要 join。
fn spawn_decode(
    shared: &Arc<Shared>,
    generation: u64,
    track: Track,
    events: &EventSink,
    start_ms: u64,
) {
    let shared = Arc::clone(shared);
    let events = events.clone();
    let spawned = std::thread::Builder::new()
        .name("myplay-dlna-decode".to_owned())
        .spawn(move || {
            if let Err(error) = decode(&shared, generation, track, &events, start_ms)
                && shared.is_current(generation)
            {
                emit(
                    &events,
                    UiEvent::Error {
                        message: format!("DLNA 播放失败：{error:#}"),
                    },
                );
            }
        });
    if let Err(error) = spawned {
        log::error!("无法启动解码线程：{error}");
    }
}

fn decode(
    shared: &Shared,
    generation: u64,
    track: Track,
    events: &EventSink,
    start_ms: u64,
) -> Result<()> {
    let mut hint = Hint::new();
    if let Some(mime) = track.mime.as_deref() {
        hint.mime_type(mime);
    }
    let http = HttpSource::open(&track.url)?;
    let total_bytes = http.len;
    let source = MediaSourceStream::new(Box::new(http), MediaSourceStreamOptions::default());
    let probed = symphonia::default::get_probe()
        .probe(
            &hint,
            source,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .with_context(|| {
            // 探测失败时就看不到下面那行质量诊断了，把关键上下文塞进错误链。
            format!(
                "无法识别媒体格式（可能是加密/会员流）：mime={:?} 地址={}",
                track.mime,
                url_shape(&track.url)
            )
        })?;
    let mut format = probed;
    let track_info = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| anyhow!("没有可播放音轨"))?
        .clone();
    let track_id = track_info.id;
    let Some(CodecParameters::Audio(params)) = track_info.codec_params.clone() else {
        bail!("没有可播放音轨");
    };
    let sample_rate = params.sample_rate.unwrap_or(44_100).max(1);
    let duration_ms = track_info
        .num_frames
        .map(|frames| frames * 1000 / u64::from(sample_rate))
        .unwrap_or(0);
    let bitrate = match (total_bytes, duration_ms) {
        (Some(bytes), duration) if duration > 0 => bytes * 8 * 1000 / duration,
        _ => 0,
    };
    // 源采样率与设备采样率不一致时必须重采样，否则会变调变速（见 [`Resampler`]）。
    let output_rate = shared.output_rate().unwrap_or_else(|| {
        // 只可能发生在建流信息还没落地时；此时退化为不重采样，至少要留痕。
        log::warn!("输出流采样率未知，本曲不做重采样（可能出现变调/变速）");
        u64::from(sample_rate)
    });
    let mut resampler = (output_rate != u64::from(sample_rate))
        .then(|| Resampler::new(u64::from(sample_rate), output_rate));
    let mut resampled = Vec::<f32>::new();
    // 诊断锚点：把「控制点声明」「URL 形状」「探测到的真实编码/采样率」「推算码率」打在一行。
    // 显示用的码率就是这一行里的推算码率，出问题时直接看它是否等于 `字节 × 8 / 时长`。
    log::info!(
        "DLNA 质量诊断：mime={:?} 地址={}｜控制点声明：{}｜实际探测：编码={}(id={}) 采样率={}Hz 位深={} 声道={} 帧数={} 总字节={} 时长={}ms 推算码率={}bps｜输出采样率={}Hz",
        track.mime,
        url_shape(&track.url),
        track.declared,
        codec_name(&params),
        params.codec,
        sample_rate,
        fmt_opt(params.bits_per_sample),
        fmt_opt(params.channels.as_ref().map(Channels::count)),
        fmt_opt(track_info.num_frames),
        fmt_opt(total_bytes),
        duration_ms,
        bitrate,
        output_rate,
    );
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .context("不支持的音频编码")?;

    emit(
        events,
        UiEvent::StreamStarted {
            codec: codec_display(&params, track.mime.as_deref()),
            sample_rate: u64::from(sample_rate),
        },
    );
    emit(events, UiEvent::AudioFormat { bitrate });
    emit(
        events,
        UiEvent::Media {
            title: track.title.clone().unwrap_or_default(),
            artist: track.artist.clone().unwrap_or_default(),
            album: track.album.clone().unwrap_or_default(),
            duration_ms,
            media_url: None,
            artwork_url: None,
            position_ms: Some(start_ms),
        },
    );

    // 先尝试让解复用器直接定位（HttpSource 支持 Range，通常是 O(1)）；
    // 失败则退回「从头解码、丢弃到目标位置」。
    let mut seek_frames = start_ms * u64::from(sample_rate) / 1000;
    if start_ms > 0
        && format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    // symphonia 0.6 的 `Time` 用秒 + 纳秒表示，接受纳秒总量。
                    time: Time::try_from_nanos_i128(i128::from(start_ms) * 1_000_000)
                        .unwrap_or(Time::ZERO),
                    track_id: Some(track_id),
                },
            )
            .is_ok()
    {
        seek_frames = 0;
    }
    let mut frames_done = 0_u64;
    let mut last_report = 0_u64;
    let mut decode_failures = 0_u32;
    // 流被判失效时已经发过 Warning，不再重复发「媒体流结束」，否则界面会同时报两件事。
    let mut stream_lost = false;
    // 复用同一块交织缓冲，避免每个 packet 都分配。
    let mut block = Vec::<f32>::new();

    loop {
        if !shared.is_current(generation) {
            return Ok(());
        }
        // symphonia 0.6 的 `next_packet` 用 `Ok(None)` 表示流结束。
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(error) => {
                // 有些容器在正常结尾也会返回错误，所以只落日志、不弹到界面。
                log::warn!("读取媒体包失败，结束本曲：{error}");
                break;
            }
        };
        if packet.track_id != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => {
                decode_failures = 0;
                decoded
            }
            Err(error) => {
                // 单个坏包（网络抖动 / 尾部填充）可以跳过；连续失败说明这条连接上跑的不再是我们在放的
                // 那一路流——典型场景：控制点在手机上切成本机播放，投送代理把同一条 HTTP 连接换成别的流，
                // 解码器于是持续报 `check failed`。这时必须停下并说明，否则只会无声地刷日志。
                decode_failures += 1;
                if decode_failures >= DECODE_FAILURE_LIMIT {
                    emit(
                        events,
                        UiEvent::Warning {
                            message: format!(
                                "连续 {decode_failures} 个音频包无法解码，已停止本曲（媒体流可能已被控制点切换）：{error}"
                            ),
                        },
                    );
                    stream_lost = true;
                    break;
                }
                continue;
            }
        };
        let frames = decoded.frames() as u64;
        if frames_done + frames < seek_frames {
            frames_done += frames;
            continue;
        }
        interleave_into(&decoded, &mut block);
        let written = match resampler.as_mut() {
            Some(resampler) => {
                resampler.process(&block, &mut resampled);
                write(shared, generation, &resampled)
            }
            None => write(shared, generation, &block),
        };
        if !written {
            return Ok(());
        }
        frames_done += frames;

        let position_ms = frames_done * 1000 / u64::from(sample_rate);
        if position_ms.saturating_sub(last_report) >= PROGRESS_REPORT_MS {
            last_report = position_ms;
            report(shared, StateReport::progress(position_ms, duration_ms));
            emit(
                events,
                UiEvent::Progress {
                    position_ms,
                    duration_ms: Some(duration_ms),
                },
            );
        }
    }

    if !stream_lost && shared.is_current(generation) {
        emit(events, UiEvent::StreamClosed);
    }
    Ok(())
}

/// 写入环形缓冲；返回 `false` 表示这一代已经作废（停止/换曲）。
fn write(shared: &Shared, generation: u64, samples: &[f32]) -> bool {
    let mut offset = 0;
    while offset < samples.len() {
        if !shared.is_current(generation) {
            return false;
        }
        let Ok(mut guard) = shared.sink.lock() else {
            return false;
        };
        let free = guard.capacity.saturating_sub(guard.buffer.len());
        if free == 0 {
            // 缓冲满：等音频回调腾出空间，超时后重新核对代次。
            match shared.drained.wait_timeout(guard, FULL_WAIT) {
                Ok(_) => continue,
                Err(_) => return false,
            }
        }
        let take = free.min(samples.len() - offset);
        guard
            .buffer
            .extend(samples[offset..offset + take].iter().copied());
        offset += take;
    }
    true
}

/// 把源采样率拉到输出采样率（流式、线性插值、跨 packet 连续）。
///
/// 为什么必须重采样：cpal 的输出流按**设备默认采样率**开（Windows 常见 48000），而解码出来的
/// 是源采样率（常见 44100）。1:1 直送等于把 44100 的内容当 48000 播出去：速度快 8.8%、
/// 音高整体升高（听感就是“加速 + 音色变了”）。
///
/// 只保留 1 帧跳包余量：`pending` 每次处理后就地丢弃已消费的整帧，内存与耗时都是 O(packet)，
/// 且只在解码线程跑，音频回调里没有任何重采样开销。
struct Resampler {
    /// 每个输出帧消耗的源帧数（`src_rate / dst_rate`）。
    step: f64,
    /// 下一个输出帧在 `pending` 里的帧位置（帧为单位，可以是小数）。
    position: f64,
    /// 尚未消费完的输入帧（立体声交织——`interleave_into` 已统一成 2 声道）。
    pending: Vec<f32>,
}

impl Resampler {
    /// 新建一个重采样器（立体声交织，2 个 f32 为一帧）。
    fn new(src_rate: u64, dst_rate: u64) -> Self {
        Self {
            step: src_rate as f64 / dst_rate as f64,
            position: 0.0,
            pending: Vec::new(),
        }
    }

    /// 吃一段交织样本，把重采样结果写进 `out`（先清空；`out` 由调用方复用，避免每个 packet 分配）。
    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        out.clear();
        self.pending.extend_from_slice(input);
        let frames = self.pending.len() / 2;
        loop {
            let index = self.position as usize;
            if index + 1 >= frames {
                break;
            }
            let fraction = (self.position - index as f64) as f32;
            let head = index * 2;
            let next = head + 2;
            out.push(self.pending[head] + (self.pending[next] - self.pending[head]) * fraction);
            out.push(
                self.pending[head + 1]
                    + (self.pending[next + 1] - self.pending[head + 1]) * fraction,
            );
            self.position += self.step;
        }
        // 保留最后一帧：下一次的输出可能落在它与新到的数据之间。
        let consumed = (self.position as usize).saturating_sub(1);
        if consumed > 0 {
            self.pending.drain(..consumed * 2);
            self.position -= consumed as f64;
        }
    }
}

/// 把解码结果整理成双声道交错 f32。
///
/// symphonia 0.6 返回的是通用缓冲（`GenericAudioBufferRef`），`copy_to_slice_interleaved`
/// 直接把任意内部采样格式转成 f32 交错排列；声道路数不匹配时在这里归一：
/// 单声道复制成双声道，超过两声道只保留前两路。
fn interleave_into(decoded: &GenericAudioBufferRef<'_>, out: &mut Vec<f32>) {
    let channels = decoded.spec().channels().count().max(1);
    let frames = decoded.frames();
    out.resize(decoded.samples_interleaved(), 0.0);
    decoded.copy_to_slice_interleaved(&mut out[..]);
    match channels {
        1 => {
            // 倒着展开：写入位置总在读取位置之后，不会覆盖尚未搬走的样本。
            out.resize(frames * 2, 0.0);
            for frame in (0..frames).rev() {
                let sample = out[frame];
                out[frame * 2] = sample;
                out[frame * 2 + 1] = sample;
            }
        }
        2 => {}
        _ => {
            // 写入位置总在读取位置之前，顺势压缩到前两声道。
            for frame in 0..frames {
                let left = out[frame * channels];
                let right = out[frame * channels + 1];
                out[frame * 2] = left;
                out[frame * 2 + 1] = right;
            }
            out.truncate(frames * 2);
        }
    }
}

/// 流式媒体源：直接读 HTTP 响应；`seek` 优走 `Range` 重连（服务端 206 时是 O(1)）。
/// 服务端忽略 `Range`（回 200）时，小文件整包缓存到内存，之后 seek/重读不再发请求。
struct HttpSource {
    /// 原始地址（重连用）。
    url: String,
    /// 已消费到的字节位置。
    pos: u64,
    /// 媒体总长度（服务端给得出时）。
    len: Option<u64>,
    /// 当前连接；`None` 表示尚未连接或已断开。
    reader: Option<Box<dyn std::io::Read + Send + Sync>>,
    /// 整包缓存（仅服务端不支持 Range 且体积在上限内时填充）；有它在时 `reader` 不使用，
    /// 读取与定位全部在内存里完成。
    cache: Option<Vec<u8>>,
}

impl HttpSource {
    fn open(url: &str) -> Result<Self> {
        let mut source = Self {
            url: url.to_owned(),
            pos: 0,
            len: None,
            reader: None,
            cache: None,
        };
        source.connect()?;
        Ok(source)
    }

    /// 建立（或重建）底层连接，起始位置为 `self.pos`。
    fn connect(&mut self) -> Result<()> {
        let response = ureq::get(&self.url)
            .header("User-Agent", "myplay/0.1 (DLNA renderer)")
            .header("Accept", "*/*")
            .header("Range", format!("bytes={}-", self.pos))
            .call()
            .map_err(|error| anyhow!("HTTP 请求失败：{error}"))?;
        // ureq 3 的响应是 `http::Response`：头取一次就拷成自有字符串，随后才能 move body。
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let content_range = header("Content-Range");
        let content_length = header("Content-Length");
        let status = response.status().as_u16();
        if self.len.is_none() {
            // Content-Range: bytes 0-1023/4096 → 取总长度；否则退回 Content-Length。
            if let Some(total) = content_range
                .as_deref()
                .and_then(|range| range.rsplit('/').next())
                .and_then(|value| value.trim().parse().ok())
            {
                self.len = Some(total);
            } else if let Some(length) = content_length
                .as_deref()
                .and_then(|value| value.trim().parse::<u64>().ok())
            {
                self.len = Some(length + self.pos);
            }
        }
        log::info!(
            "DLNA 拉流：{}（Range 支持：{}，长度：{:?}，Content-Type：{:?}）",
            self.pos,
            if status == 206 { "206" } else { "200 回退" },
            self.len,
            header("Content-Type"),
        );
        let mut reader: Box<dyn std::io::Read + Send + Sync> =
            Box::new(response.into_body().into_reader());
        // 服务端无视 Range（回 200 而不是 206）时，正文是从第 0 字节开始的。
        // ① 小文件：整包读进内存，之后 seek/重读都是纯内存操作（不再重下前缀）。
        //    代价：这次是在解码线程上同步读完整个文件（12 MB/LAN 约 0.2~1 s），
        //    会吃掉播放缓冲、产生一次可听空隙；换来之后所有 seek 都不再重下。
        // ② 大文件：只能把前 `self.pos` 字节读掉丢掉，否则「位置」与「数据」错位——
        //    解复用器从半路读到的不是它以为的字节，AAC 这类会直接校验失败。
        if status != 206 && self.pos > 0 {
            if let Some(len) = self
                .len
                .filter(|len| (1..=STREAM_CACHE_LIMIT).contains(len))
            {
                // 按声明长度截断读取：服务端谎报长度时也只吃 `len` 字节，不会无限放大内存。
                let mut cache = Vec::with_capacity(len as usize);
                let read = read_up_to(&mut *reader, len, &mut cache)
                    .with_context(|| format!("缓存 {len} 字节的媒体流失败"))?;
                if read != len {
                    log::warn!(
                        "DLNA 拉流：声明 {len} 字节但只读到 {}，按实际长度使用",
                        cache.len()
                    );
                }
                log::info!(
                    "DLNA 拉流：服务端忽略 Range，已整包缓存 {} 字节（后续 seek 走内存）",
                    cache.len()
                );
                self.cache = Some(cache);
                self.reader = None;
                return Ok(());
            }
            let skipped = skip_bytes(&mut *reader, self.pos)
                .with_context(|| format!("跳过 {} 字节失败（服务端忽略 Range）", self.pos))?;
            if skipped == self.pos {
                log::info!("DLNA 拉流：服务端忽略 Range（200），已在本地跳过 {skipped} 字节");
            } else {
                log::warn!(
                    "DLNA 拉流：服务端忽略 Range 且提前结束，只跳过了 {skipped}/{} 字节",
                    self.pos
                );
            }
        }
        self.reader = Some(reader);
        Ok(())
    }
}

/// 读入最多 `limit` 字节到 `out`（整包缓存用；按声明长度截断，防止谎报长度放大内存）。
///
/// 返回实际读到的字节数：小于 `limit` 表示流提前结束，由调用方决定严重程度。
fn read_up_to<R: std::io::Read + ?Sized>(
    reader: &mut R,
    limit: u64,
    out: &mut Vec<u8>,
) -> std::io::Result<u64> {
    let mut scratch = [0_u8; HTTP_CHUNK];
    let mut total = 0_u64;
    while total < limit {
        let want = (limit - total).min(HTTP_CHUNK as u64) as usize;
        let read = reader.read(&mut scratch[..want])?;
        if read == 0 {
            break;
        }
        out.extend_from_slice(&scratch[..read]);
        total += read as u64;
    }
    Ok(total)
}

/// 从流里丢弃 `count` 字节（服务端不支持 Range 时的本地补偿）。
///
/// 返回实际丢弃的字节数：读到 EOF 时小于 `count`，由调用方判断严重程度。
/// 固定小缓冲，不分配，也不把数据留在内存里。
fn skip_bytes<R: std::io::Read + ?Sized>(reader: &mut R, count: u64) -> std::io::Result<u64> {
    // 块读，块大小只影响系统调用次数。
    let mut scratch = [0_u8; HTTP_CHUNK];
    let mut remaining = count;
    while remaining > 0 {
        let want = remaining.min(HTTP_CHUNK as u64) as usize;
        let read = reader.read(&mut scratch[..want])?;
        if read == 0 {
            break;
        }
        remaining -= read as u64;
    }
    Ok(count - remaining)
}

impl std::io::Read for HttpSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // 整包缓存命中：直接从内存取，位置与数据天然对齐。
        if let Some(cache) = self.cache.as_ref() {
            let start = (self.pos as usize).min(cache.len());
            let available = cache.len() - start;
            let take = available.min(buf.len());
            buf[..take].copy_from_slice(&cache[start..start + take]);
            self.pos += take as u64;
            return Ok(take);
        }
        let Some(reader) = self.reader.as_mut() else {
            return Ok(0);
        };
        let read = reader.read(buf)?;
        self.pos += read as u64;
        Ok(read)
    }
}

impl std::io::Seek for HttpSource {
    fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
        let target = match position {
            std::io::SeekFrom::Start(value) => value as i64,
            std::io::SeekFrom::End(value) => self.len.map(|len| len as i64 + value).unwrap_or(0),
            std::io::SeekFrom::Current(value) => self.pos as i64 + value,
        }
        .max(0) as u64;

        if target == self.pos {
            return Ok(target);
        }
        self.pos = target;
        // 有整包缓存时 seek 是纯内存操作：不重连、不发请求。
        if self.cache.is_some() {
            return Ok(target);
        }
        self.reader = None;
        self.connect()
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        Ok(self.pos)
    }
}

impl symphonia::core::io::MediaSource for HttpSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        self.len
    }
}

/// 把播放状态回报给 DMR（控制点读 GetPositionInfo 用）。
fn report(shared: &Shared, state: StateReport) {
    let Ok(slot) = shared.controller.lock() else {
        return;
    };
    let Some((controller, handle)) = slot.as_ref() else {
        return;
    };
    let controller = Arc::clone(controller);
    // 回到核心运行时里执行：控制器内部的 GENA 通知需要 tokio。
    handle.spawn(async move {
        controller.update_playback_state(
            state.position_ms,
            state.duration_ms,
            Some(state.rate),
            Some(state.ready),
        );
    });
}

/// 从 JSON 里取一个可选字符串字段（`null` / 空串视为缺失）。
fn text_field(value: &serde_json::Value, key: &str) -> Option<String> {
    opt_text(value, key)
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

/// `Option` → 诊断文本：缺失写 `-`（只给日志用，不在音频路径上）。
fn fmt_opt<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| value.to_string())
}

/// 探测出的真实编码名（`mp3` / `flac` / …）；注册表里没有就返回 `unknown`。
///
/// 这是 symphonia 按 codec id 查到的解码器名，与 DIDL 里声明的 mime 无关，
/// 因此可以用它验证「显示成 MP3」到底是真 MP3 还是回退文案。
fn codec_name(params: &AudioCodecParameters) -> &'static str {
    symphonia::default::get_codecs()
        .get_audio_decoder(params.codec)
        .map_or("unknown", |registered| registered.codec.info.short_name)
}

/// 诊断用的 URL 形状：只留主机与路径后缀，丢弃查询串（里面有鉴权参数，不能落盘）。
fn url_shape(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let host = rest.split(['/', '?']).next().unwrap_or_default();
    let path = rest.split('?').next().unwrap_or_default();
    let suffix = path
        .rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, suffix)| suffix)
        .filter(|suffix| !suffix.is_empty() && suffix.len() <= 5)
        .unwrap_or("-");
    format!("{host} 后缀={suffix}")
}

/// 界面显示的编码名：优先用 symphonia 注册表里查到的**真实**编码名（`mp3` / `flac` / …），
/// 查不到才退回 mime 猜测。旧实现只看 mime，缺失时一律兜底成 `MP3`，会把 FLAC 显示错。
fn codec_display(params: &AudioCodecParameters, mime: Option<&str>) -> String {
    match codec_name(params) {
        "unknown" => codec_label(mime).to_owned(),
        name => name.to_ascii_uppercase(),
    }
}

/// mime → 编码名（仅作 [`codec_display`] 的回退，不参与正常路径）。
fn codec_label(mime: Option<&str>) -> &'static str {
    let lowered = mime.unwrap_or_default().to_ascii_lowercase();
    if lowered.contains("flac") {
        "FLAC"
    } else if lowered.contains("aac")
        || (lowered.contains("mpeg") && lowered.contains('4'))
        || lowered.contains("mp4")
    {
        "AAC"
    } else if lowered.contains("wav") {
        "PCM"
    } else if lowered.contains("ogg") {
        "Vorbis"
    } else {
        "MP3"
    }
}

/// 把一个事件交给 Core 事件泵（Core 已释放时直接丢弃）。
fn emit(events: &EventSink, event: UiEvent) {
    let _ = events.send(event);
}

/// 采样缓冲：解码线程写、音频回调读。
#[derive(Default)]
struct Sink {
    /// 已解码但未被音频回调取走的采样。
    buffer: VecDeque<f32>,
    /// 缓冲上限（采样数），按输出格式一次性算好，之后不再扩容。
    capacity: usize,
}

fn build_stream(shared: Arc<Shared>, requested: Option<&str>) -> Result<(cpal::Stream, String)> {
    let host = cpal::default_host();
    let device = requested
        .and_then(|wanted| {
            let devices = host.output_devices().ok()?;
            let mut matched = None;
            for candidate in devices {
                if audio::matches(&candidate, wanted) {
                    matched = Some(candidate);
                    break;
                }
            }
            matched
        })
        .or_else(|| {
            if requested.is_some() {
                log::warn!("请求的输出设备不可用，回退系统默认");
            }
            host.default_output_device()
        })
        .ok_or_else(|| anyhow!("没有可用输出设备"))?;

    let supported = device
        .default_output_config()
        .context("读取默认输出格式失败")?;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let channels = usize::from(config.channels).max(1);
    // 缓冲在开流前一次性按上限预留，之后只 push/pop，音频回调里绝不触发扩容。
    let capacity = (config.sample_rate as usize / 1000) * BUFFER_MS * channels;
    shared
        .output_rate
        .store(config.sample_rate, Ordering::Release);
    if let Ok(mut guard) = shared.sink.lock() {
        guard.capacity = capacity;
        guard.buffer = VecDeque::with_capacity(capacity);
    }
    let error_fn = |error| log::error!("音频输出错误：{error}");
    let stream = match format {
        cpal::SampleFormat::F32 => {
            let shared = Arc::clone(&shared);
            device.build_output_stream::<f32, _, _>(
                config,
                move |out: &mut [f32], _: &cpal::OutputCallbackInfo| fill(&shared, out),
                error_fn,
                None,
            )
        }
        cpal::SampleFormat::I16 => {
            let shared = Arc::clone(&shared);
            // 只在首帧扩容一次：音频回调里禁止分配内存。
            let mut scratch = Vec::<f32>::new();
            device.build_output_stream::<i16, _, _>(
                config,
                move |out: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    if scratch.len() < out.len() {
                        scratch.resize(out.len(), 0.0);
                    }
                    let block = &mut scratch[..out.len()];
                    fill(&shared, block);
                    for (target, value) in out.iter_mut().zip(block.iter()) {
                        *target = unit_to_i16(*value);
                    }
                },
                error_fn,
                None,
            )
        }
        other => bail!("不支持的输出格式：{other:?}"),
    }
    .context("无法创建音频输出流")?;
    let device_name = audio::device_name(&device).unwrap_or_else(|| "系统默认输出".to_owned());
    // 输出流按设备默认采样率跑；源采样率不同时由解码线程重采样，这一行是排查变调/变速的锚点。
    log::info!(
        "音频输出：{device_name}｜{} Hz｜{} 声道｜{format:?}",
        config.sample_rate,
        config.channels
    );
    Ok((stream, device_name))
}

/// f32 采样 → i16（先夹紧再缩放，避免溢出成反相噪声）。
fn unit_to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16
}

/// 音频回调：从缓冲取采样；缓冲空了就输出静音（欠载不报错）。
fn fill(shared: &Shared, out: &mut [f32]) {
    let Ok(mut guard) = shared.sink.lock() else {
        out.fill(0.0);
        return;
    };
    if shared.paused.load(Ordering::Relaxed) {
        out.fill(0.0);
        return;
    }
    let available = out.len().min(guard.buffer.len());
    for (slot, sample) in out.iter_mut().zip(guard.buffer.drain(..available)) {
        *slot = sample;
    }
    for slot in &mut out[available..] {
        *slot = 0.0;
    }
    drop(guard);
    // 唤醒等待空间的解码线程（没有等待者时是空操作）。
    shared.drained.notify_all();
}

#[cfg(test)]
mod tests {
    use super::{
        DeclaredQuality, HttpSource, Resampler, codec_display, codec_label, codec_name, fmt_opt,
        read_up_to, skip_bytes, unit_to_i16, url_shape,
    };
    use serde_json::json;
    use std::io::{Read as _, Seek as _};
    use symphonia::core::codecs::audio::AudioCodecParameters;

    #[test]
    fn codec_display_prefers_the_probed_codec() {
        use symphonia::core::codecs::audio::well_known::{CODEC_ID_FLAC, CODEC_ID_MP3};
        let mut params = AudioCodecParameters {
            codec: CODEC_ID_MP3,
            ..AudioCodecParameters::default()
        };
        assert_eq!(codec_display(&params, Some("audio/mpeg")), "MP3");
        // probe 与 mime 冲突时以 probe 为准：旧实现照 mime 显示，会把 FLAC 报成 MP3。
        params.codec = CODEC_ID_FLAC;
        assert_eq!(codec_display(&params, Some("audio/mpeg")), "FLAC");
        // 注册表查不到时退回 mime；mime 也没有才沿用旧的 MP3 兜底。
        assert_eq!(
            codec_display(&AudioCodecParameters::default(), Some("audio/flac")),
            "FLAC"
        );
        assert_eq!(codec_display(&AudioCodecParameters::default(), None), "MP3");
    }

    #[test]
    fn codec_label_follows_mime() {
        assert_eq!(codec_label(Some("audio/flac")), "FLAC");
        assert_eq!(codec_label(Some("audio/mp4")), "AAC");
        assert_eq!(codec_label(Some("audio/aac")), "AAC");
        assert_eq!(codec_label(Some("audio/wav")), "PCM");
        assert_eq!(codec_label(Some("audio/ogg")), "Vorbis");
        assert_eq!(codec_label(Some("audio/mpeg")), "MP3");
        assert_eq!(codec_label(None), "MP3");
    }

    #[test]
    fn conversion_clamps_instead_of_wrapping() {
        assert_eq!(unit_to_i16(0.0), 0);
        assert_eq!(unit_to_i16(1.0), i16::MAX);
        assert_eq!(unit_to_i16(-1.0), -i16::MAX);
        assert_eq!(unit_to_i16(2.0), i16::MAX);
        assert_eq!(unit_to_i16(-2.0), -i16::MAX);
    }

    #[test]
    fn declared_quality_reads_dlna_media_fields() {
        let declared = DeclaredQuality::from_event(&json!({
            "type": "dlna_media",
            "url": "https://example.test/a.mp3",
            "bitrate_bps": 1_411_200,
            "sample_rate": 96_000,
            "bits_per_sample": 24,
            "channels": 2,
            "duration_ms": 185_250,
        }));
        assert_eq!(declared.bitrate_bps, Some(1_411_200));
        assert_eq!(declared.sample_rate, Some(96_000));
        assert_eq!(declared.bits_per_sample, Some(24));
        assert_eq!(declared.channels, Some(2));
        assert_eq!(declared.duration_ms, Some(185_250));
        assert_eq!(
            declared.to_string(),
            "码率=1411200 采样率=96000 位深=24 声道=2 时长=185250ms"
        );
    }

    #[test]
    fn declared_quality_keeps_missing_fields_empty() {
        let declared = DeclaredQuality::from_event(&json!({"type": "dlna_media"}));
        assert_eq!(declared.bitrate_bps, None);
        assert_eq!(
            declared.to_string(),
            "码率=- 采样率=- 位深=- 声道=- 时长=-ms"
        );
    }

    #[test]
    fn url_shape_drops_credentials_and_query() {
        // 播放地址带鉴权串，日志只能留主机 + 后缀。
        assert_eq!(
            url_shape("https://m701.music.126.net/20240101/abc/track.mp3?auth=secret"),
            "m701.music.126.net 后缀=mp3"
        );
        assert_eq!(
            url_shape("http://192.168.1.5:8080/"),
            "192.168.1.5:8080 后缀=-"
        );
    }

    #[test]
    fn resampler_passes_through_when_rates_match() {
        let mut resampler = Resampler::new(44_100, 44_100);
        let mut out = Vec::new();
        let input = [0.0_f32, 1.0, 0.25, 0.5, 0.75, 0.125];
        resampler.process(&input, &mut out);
        // 采样率相同时（真实路径不会创建它）也必须原样输出，只差末尾留作余量的一帧。
        assert!(out.len() >= input.len() - 2);
        assert!((out[0] - input[0]).abs() < 1e-6);
        assert!((out[1] - input[1]).abs() < 1e-6);
        assert!((out[2] - input[2]).abs() < 1e-6);
    }

    #[test]
    fn resampler_matches_the_output_timeline() {
        // 0.1 s 的源素材，重采样后时长必须还是 0.1 s（帧数 = 秒数 × 输出采样率）。
        for (src, dst, input_frames) in [(44_100_u64, 48_000_u64, 4_410), (48_000, 44_100, 4_800)] {
            let mut resampler = Resampler::new(src, dst);
            let mut out = Vec::new();
            resampler.process(&vec![0.1_f32; input_frames * 2], &mut out);
            let expected = (input_frames as f64 * dst as f64 / src as f64) as usize;
            assert!(
                (out.len() / 2).abs_diff(expected) <= 3,
                "{src} → {dst}：{} 帧，期望 {expected}",
                out.len() / 2
            );
        }
    }

    #[test]
    fn resampler_keeps_the_timeline_across_packets() {
        // 分块送入与一次性送入必须得到同样长度的输出，否则长曲会持续跑偏。
        let input = vec![0.25_f32; 4_410 * 2];
        let mut once = Resampler::new(44_100, 48_000);
        let mut once_out = Vec::new();
        once.process(&input, &mut once_out);

        let mut chunked = Resampler::new(44_100, 48_000);
        let mut chunked_out = Vec::new();
        for chunk in input.chunks(1_024 * 2) {
            let mut out = Vec::new();
            chunked.process(chunk, &mut out);
            chunked_out.extend_from_slice(&out);
        }
        assert!(
            once_out.len().abs_diff(chunked_out.len()) <= 3,
            "一次 {} vs 分块 {}",
            once_out.len(),
            chunked_out.len()
        );
    }

    #[test]
    fn resampler_keeps_a_sine_intact() {
        // 线性插值对 441 Hz 这种低频几乎不衰减，也不应产生 NaN/削顶。
        let frames = 4_410_usize;
        let mut input = Vec::with_capacity(frames * 2);
        for index in 0..frames {
            let value = (std::f32::consts::TAU * 441.0 * index as f32 / 44_100.0).sin();
            input.push(value);
            input.push(value);
        }
        let mut resampler = Resampler::new(44_100, 48_000);
        let mut out = Vec::new();
        resampler.process(&input, &mut out);
        let peak = out
            .iter()
            .fold(0.0_f32, |peak, value| peak.max(value.abs()));
        assert!(out.iter().all(|value| value.is_finite()));
        assert!((0.98..=1.0).contains(&peak), "peak = {peak}");
    }

    #[test]
    fn skip_bytes_reads_exactly_the_prefix() {
        let data = [1_u8, 2, 3, 4, 5, 6, 7, 8];
        let mut reader: &[u8] = &data;
        assert_eq!(skip_bytes(&mut reader, 3).unwrap(), 3);
        // 跳过后从正确位置继续读：服务端忽略 Range 时靠这一步维持位置对齐。
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, [4_u8, 5, 6, 7, 8]);
    }

    #[test]
    fn read_up_to_stops_at_the_limit_or_at_eof() {
        let data = [1_u8, 2, 3, 4, 5, 6];
        let mut reader: &[u8] = &data;
        let mut out = Vec::new();
        // limit 小于数据量：只读 limit，剩余留给后续读取（缓存分支不会越界吃内存）。
        assert_eq!(read_up_to(&mut reader, 4, &mut out).unwrap(), 4);
        assert_eq!(out, [1_u8, 2, 3, 4]);

        let mut short = Vec::new();
        assert_eq!(read_up_to(&mut reader, 10, &mut short).unwrap(), 2);
        assert_eq!(short, [5_u8, 6]);
    }

    #[test]
    fn skip_bytes_stops_at_eof() {
        let data = [1_u8, 2, 3];
        let mut reader: &[u8] = &data;
        // 要求跳得比数据多：返回实际丢弃量，由调用方判断。
        assert_eq!(skip_bytes(&mut reader, 10).unwrap(), 3);
        assert_eq!(skip_bytes(&mut reader, 1).unwrap(), 0);
    }

    #[test]
    fn cached_source_seeks_in_memory() {
        // 整包缓存后：读取与 seek 都不再碰网络，也不会越界 panic。
        let mut source = HttpSource {
            url: "http://example.test/a.mp3".to_owned(),
            pos: 0,
            len: Some(4),
            reader: None,
            cache: Some(vec![1, 2, 3, 4]),
        };
        let mut head = [0_u8; 4];
        source.read_exact(&mut head).unwrap();
        assert_eq!(head, [1, 2, 3, 4]);

        assert_eq!(source.seek(std::io::SeekFrom::Start(1)).unwrap(), 1);
        let mut rest = Vec::new();
        source.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, [2, 3, 4]);

        // 越过末尾：读到 EOF，不 panic。
        assert_eq!(source.seek(std::io::SeekFrom::Start(99)).unwrap(), 99);
        assert_eq!(source.read(&mut head).unwrap(), 0);
    }

    #[test]
    fn unknown_codec_id_has_no_name() {
        // 默认参数里的 codec id 是 null codec，注册表查不到 → 必须落到 unknown，而不是 panic。
        assert_eq!(codec_name(&AudioCodecParameters::default()), "unknown");
    }

    #[test]
    fn optional_marks_missing_values() {
        assert_eq!(fmt_opt(None::<u64>), "-");
        assert_eq!(fmt_opt(Some(44_100_u32)), "44100");
    }
}
