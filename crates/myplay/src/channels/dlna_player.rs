//! DLNA 音频播放：拉流 → symphonia 解码 → cpal 输出。
//!
//! 关键设计：**命令线程只做调度，解码在独立线程里跑**。每首歌/每次操作都会自增
//! 「代次」（generation），解码线程每写一批采样就核对代次，不匹配立即退出——这样
//! Stop / 换曲 / seek 都能立刻生效，而不是等整首放完。
//!
//! 两个线程的接触面只有 [`Shared`]：解码线程生产采样、cpal 回调消费采样。回调里
//! **不分配内存、不打开文件、不做字符串处理**，缓冲腾挪靠 [`Condvar`] 通知而不是轮询。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use fusionplay_core::DmrController;
use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::AudioDecoderOptions;
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
    /// 服务器给出的 Content-Type，用于猜测容器格式。
    mime: Option<String>,
    /// 曲名。
    title: Option<String>,
    /// 艺术家。
    artist: Option<String>,
    /// 专辑。
    album: Option<String>,
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
        .context("无法识别媒体格式（可能是加密/会员流）")?;
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
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .context("不支持的音频编码")?;

    let bitrate = match (total_bytes, duration_ms) {
        (Some(bytes), duration) if duration > 0 => bytes * 8 * 1000 / duration,
        _ => 0,
    };
    emit(
        events,
        UiEvent::StreamStarted {
            codec: codec_label(track.mime.as_deref()).to_owned(),
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
    // 复用同一块交织缓冲，避免每个 packet 都分配。
    let mut block = Vec::<f32>::new();

    loop {
        if !shared.is_current(generation) {
            return Ok(());
        }
        // symphonia 0.6 的 `next_packet` 用 `Ok(None)` 表示流结束。
        let Ok(Some(packet)) = format.next_packet() else {
            break;
        };
        if packet.track_id != track_id {
            continue;
        }
        let Ok(decoded) = decoder.decode(&packet) else {
            continue;
        };
        let frames = decoded.frames() as u64;
        if frames_done + frames < seek_frames {
            frames_done += frames;
            continue;
        }
        interleave_into(&decoded, &mut block);
        if !write(shared, generation, &block) {
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

    if shared.is_current(generation) {
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

/// 流式媒体源：直接读 HTTP 响应；`seek` 用 `Range` 重连（服务端 206 时是 O(1)）。
struct HttpSource {
    /// 原始地址（重连用）。
    url: String,
    /// 已消费到的字节位置。
    pos: u64,
    /// 媒体总长度（服务端给得出时）。
    len: Option<u64>,
    /// 当前连接；`None` 表示尚未连接或已断开。
    reader: Option<Box<dyn std::io::Read + Send + Sync>>,
}

impl HttpSource {
    fn open(url: &str) -> Result<Self> {
        let mut source = Self {
            url: url.to_owned(),
            pos: 0,
            len: None,
            reader: None,
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
            "DLNA 拉流：{}（Range 支持：{}，长度：{:?}）",
            self.pos,
            if status == 206 { "206" } else { "200 回退" },
            self.len
        );
        self.reader = Some(Box::new(response.into_body().into_reader()));
        Ok(())
    }
}

impl std::io::Read for HttpSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
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

/// 把 MIME 翻译成人看得懂的编码名（symphonia 的 codec 字段是数字 ID）。
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
    use super::{codec_label, unit_to_i16};

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
}
