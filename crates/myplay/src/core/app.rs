//! L3 应用核心：[`Core`] 是唯一的业务状态源，且与窗口无关（窗口开开关关它都活着）。
//!
//! 数据流：
//! 1. 通道（L4）把归一后的 [`UiEvent`] 写进 [`EventSink`]（flume，有界）；
//! 2. [`Core::new`] 起的事件泵把每个事件交给 [`Core::apply`] 更新本结构体；
//! 3. 每次状态变化都 `cx.notify()`，订阅 Core 的视图（L1）据此重绘；
//! 4. 每次状态变化同时压成 [`StatusSummary`] 送进托盘回传通道（托盘只负责显示 tooltip）；
//! 5. 视图与托盘的意图（启动 / 停止 / 播放暂停 / 改配置）一律调用 Core 的方法，
//!    谁都不直接碰 [`Channel`]。
//!
//! 两条后台泵承担所有慢操作，主线程只做内存更新：事件解码（封面）与日志落盘。

use std::collections::VecDeque;
use std::io::Read as _;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use gpui_kit::*;

use crate::channels::{self, Channel, EventSink, Snapshot};
use crate::core::config::{CloseAction, DeviceKind, Settings, Source};
use crate::core::event::UiEvent;
use crate::core::state::RunState;
use crate::platform::system;

/// 事件日志保留的行数上限（超出从头删除）。
const LOG_LIMIT: usize = 1000;
/// 通道事件的队列上限：满时反压通道线程，宁可慢一点也不无限吃内存。
const EVENT_QUEUE: usize = 1024;
/// 封面下载结果队列上限。
const ARTWORK_QUEUE: usize = 4;
/// 封面单张最大字节数，防止一条超链接吃光内存。
const ARTWORK_LIMIT: u64 = 8 * 1024 * 1024;

/// 当前播放信息（由通道事件累积；停止接收时整体复位）。
#[derive(Default)]
pub struct NowPlaying {
    /// 曲名。
    pub title: String,
    /// 艺术家。
    pub artist: String,
    /// 专辑。
    pub album: String,
    /// 媒体编码（如 `AAC`、`ALAC`）。
    pub codec: String,
    /// 采样率（Hz）。
    pub sample_rate: u64,
    /// 码率（bps），0 表示通道未报告。
    pub bitrate: u64,
    /// 播放位置（ms）。
    pub position_ms: u64,
    /// 总时长（ms），0 表示未知。
    pub duration_ms: u64,
    /// 音量百分比；`None` 表示该通道不报音量。
    pub volume: Option<u32>,
    /// 通道报告的播放/暂停状态。
    pub playing: bool,
    /// DLNA 媒体来源主机（如 `music.126.net:80`），用于信息页显示。
    pub media_host: String,
    /// 已解码的封面图。
    pub artwork: Option<Arc<Image>>,
    /// 当前封面地址（避免同一张图重复下载）。
    pub artwork_url: String,
}

/// 可编辑的单行文本字段，指明要改 [`Settings`] 里的哪一个字符串。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextField {
    /// 设备名。
    Name,
    /// 事件日志文件路径。
    LogFile,
    /// 需要 goodbye 清理的历史 Lyra 实例。
    RetireLyra,
}

impl TextField {
    /// 渲染用的稳定元素 id（禁止用列表下标当 id）。
    pub const fn element_id(self) -> &'static str {
        match self {
            Self::Name => "field-name",
            Self::LogFile => "field-log-file",
            Self::RetireLyra => "field-retire-lyra",
        }
    }

    /// 取出该字段对应的字符串。
    pub fn text(self, settings: &Settings) -> &str {
        match self {
            Self::Name => &settings.name,
            Self::LogFile => &settings.log_file,
            Self::RetireLyra => &settings.retire_lyra,
        }
    }

    /// 取出该字段对应的可变字符串。
    fn text_mut(self, settings: &mut Settings) -> &mut String {
        match self {
            Self::Name => &mut settings.name,
            Self::LogFile => &mut settings.log_file,
            Self::RetireLyra => &mut settings.retire_lyra,
        }
    }
}

/// 一次文本编辑意图：视图负责把按键翻译成它，Core 不必认识 GPUI 的按键结构。
#[derive(Clone, Copy, Debug)]
pub enum TextEdit<'text> {
    /// 删除最后一个字符。
    Backspace,
    /// 插入一段文本（控制字符会被忽略）。
    Insert(&'text str),
    /// 其它按键：不改文本。
    Ignore,
}

/// 回传给托盘的极简状态（托盘只用它拼 tooltip，不持有业务状态）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusSummary {
    /// 接收器状态。
    pub status: RunState,
    /// 曲名；空表示当前没有媒体。
    pub title: String,
    /// 艺术家。
    pub artist: String,
    /// 播放/暂停。
    pub playing: bool,
}

impl StatusSummary {
    /// 托盘 tooltip 文本。Windows 托盘 tooltip 上限 128 字符，这里按 120 字符截断。
    pub fn tooltip(&self) -> String {
        let text = if self.title.is_empty() {
            format!("myplay · {}", self.status.label())
        } else {
            let state = if self.playing {
                "播放中"
            } else {
                "已暂停"
            };
            if self.artist.is_empty() {
                format!("myplay · {state} · {}", self.title)
            } else {
                format!("myplay · {state} · {} - {}", self.artist, self.title)
            }
        };
        text.chars().take(120).collect()
    }
}

/// 全局持有核心实体：窗口、托盘都通过它取到同一个 [`Core`]。
pub struct SharedCore(pub Entity<Core>);

impl Global for SharedCore {}

/// L3 核心：配置 + 状态机 + 事件归一 + 通道句柄。
///
/// 除标注「只读」的公开字段外，其它状态（通道、泵、托盘通道）都是私有的，
/// 只能通过方法改动，保证「改配置要落盘、状态变化要刷新界面与托盘」不被绕过。
pub struct Core {
    /// 只读：当前配置（修改请用 `set_*` 方法，它们负责落盘与重启）。
    pub settings: Settings,
    /// 只读：接收器状态机的当前状态。
    pub status: RunState,
    /// 只读：给用户看的一句话说明（上线地址、失败原因等）。
    pub detail: String,
    /// 只读：妙播通道的广播信息快照（其它通道为 `None`）。
    pub snapshot: Option<Snapshot>,
    /// 只读：当前播放信息。
    pub playing: NowPlaying,
    /// 只读：对端设备名。
    pub peer: String,
    /// 只读：握手阶段的中文标签。
    pub stage: String,
    /// 只读：事件日志（最新在尾部，上限 [`LOG_LIMIT`] 行）。用 `SharedString`
    /// 让界面重绘只克隆引用，不复制文本。
    pub log: VecDeque<SharedString>,
    /// 只读：实际在用的输出设备名（由通道事件回填）。
    pub active_output: Option<String>,
    /// 只读：本次启动时刻，用于界面显示运行时长。
    pub started_at: Option<Instant>,
    /// 只读：本次启动累计曲目数。
    pub tracks: u32,
    /// 只读：媒体流重建次数。
    pub reconnects: u32,
    /// 首次打开窗口时是否自动最小化；只生效一次。
    start_minimized_pending: bool,
    /// 封面下载请求入口（下载线程串行处理并合并连续换曲）。
    artwork_request_tx: flume::Sender<String>,
    /// 交给通道的事件入口（克隆给每条通道）。
    sink: EventSink,
    /// 正在运行的通道；`None` 表示已停止。
    channel: Option<Box<dyn Channel>>,
    /// 日志落盘入口：`(文件路径, 已格式化的一行)`。
    log_tx: flume::Sender<(String, String)>,
    /// 状态回传托盘通道。
    status_tx: flume::Sender<StatusSummary>,
    /// 上一次回传的内容，避免重复打扰托盘线程。
    last_status: Option<StatusSummary>,
}

impl Core {
    /// 创建核心，并启动事件泵、封面泵与日志泵。
    ///
    /// `settings` 由 `main` 读一次后传入，`status_tx` 是给托盘的回传通道。
    pub fn new(
        settings: Settings,
        status_tx: flume::Sender<StatusSummary>,
        cx: &mut Context<Self>,
    ) -> Self {
        let (sink, events) = flume::bounded::<UiEvent>(EVENT_QUEUE);
        let (artwork_request_tx, artwork_request_rx) = flume::unbounded::<String>();
        let (artwork_tx, artwork_rx) = flume::bounded::<(String, Vec<u8>)>(ARTWORK_QUEUE);

        // 事件泵：通道线程 → Core。持有 WeakEntity，窗口/视图销毁不影响它，
        // 但 Core 自己释放时泵会自然退出，不会泄漏。
        cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv_async().await {
                let _ = this.update(cx, |core, cx| core.apply(event, cx));
            }
        })
        .detach();

        // 封面泵：网络下载在独立线程完成，这里只做解码 + 刷新。
        cx.spawn(async move |this, cx| {
            while let Ok((url, bytes)) = artwork_rx.recv_async().await {
                let image = decode_image(bytes);
                let _ = this.update(cx, |core, cx| {
                    if core.playing.artwork_url == url
                        && let Some(image) = image
                    {
                        core.playing.artwork = Some(Arc::new(image));
                        cx.notify();
                    }
                });
            }
        })
        .detach();

        // 封面下载线程：单线程串行下载，遇到连续换曲只保留最后一个地址，
        // 因此快速切歌不会堆线程，也不会把中间那几张图全下完。
        let download_tx = artwork_tx.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("myplay-artwork".to_owned())
            .spawn(move || {
                while let Ok(mut url) = artwork_request_rx.recv() {
                    while let Ok(next) = artwork_request_rx.try_recv() {
                        url = next;
                    }
                    if let Some(bytes) = fetch_artwork(&url) {
                        let _ = download_tx.send((url, bytes));
                    }
                }
            })
        {
            log::error!("无法启动封面线程：{error}");
        }

        // 日志落盘线程：文件 I/O 不占界面线程，句柄跨行复用。
        let (log_tx, log_rx) = flume::unbounded::<(String, String)>();
        if let Err(error) = std::thread::Builder::new()
            .name("myplay-log".to_owned())
            .spawn(move || {
                let mut writer = system::LogWriter::default();
                while let Ok((path, line)) = log_rx.recv() {
                    writer.write(&path, &line);
                }
            })
        {
            log::error!("无法启动日志线程：{error}");
        }

        let start_minimized_pending = settings.start_minimized;
        let mut core = Self {
            settings,
            status: RunState::Idle,
            detail: String::new(),
            snapshot: None,
            playing: NowPlaying::default(),
            peer: String::new(),
            stage: String::new(),
            log: VecDeque::new(),
            active_output: None,
            started_at: None,
            tracks: 0,
            reconnects: 0,
            start_minimized_pending,
            artwork_request_tx,
            sink,
            channel: None,
            log_tx,
            status_tx,
            last_status: None,
        };
        // 先推一次初始状态，托盘一挂上就有正确 tooltip。
        core.publish_status();
        core
    }

    // ------------------------------------------------------------ 生命周期

    /// 启动接收器（幂等：非 Idle/Failed 状态直接忽略）。
    pub fn start(&mut self, cx: &mut Context<Self>) {
        if !self.status.can_start() {
            return;
        }
        // 出错态可能还留着旧通道：先停干净再起，避免两个接收器抢端口/mDNS。
        if self.channel.is_some() {
            self.stop(cx);
        }
        self.set_status(RunState::Starting);
        match channels::start(self.settings.source, &self.settings, self.sink.clone()) {
            Ok(channel) => {
                self.snapshot = channel.snapshot().cloned();
                self.set_status(RunState::Running);
                self.detail = "已上线".to_owned();
                self.started_at = Some(Instant::now());
                self.tracks = 0;
                self.reconnects = 0;
                self.playing = NowPlaying::default();
                self.peer.clear();
                self.stage.clear();
                self.active_output = None;
                self.push_log("接收器已启动");
                self.channel = Some(channel);
            }
            Err(error) => {
                self.set_status(RunState::Failed);
                self.detail = format!("{error:#}");
                self.push_log(format!("启动失败：{error:#}"));
            }
        }
        self.changed(cx);
    }

    /// 停止接收器（幂等：没有通道、状态机也不允许停止时直接返回）。
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        // 启动失败时通道已经不在，但这仍是一次有效的「拉回 Idle」请求。
        if self.channel.is_none() && !self.status.can_stop() {
            return;
        }
        self.set_status(RunState::Stopping);
        if let Some(channel) = self.channel.take() {
            channel.stop();
        }
        self.set_status(RunState::Idle);
        self.detail = "未启动".to_owned();
        self.snapshot = None;
        self.started_at = None;
        // 停止后「信息」页不该残留上一次的播放与连接状态。
        self.playing = NowPlaying::default();
        self.peer.clear();
        self.stage.clear();
        self.active_output = None;
        self.tracks = 0;
        self.reconnects = 0;
        self.push_log("接收器已停止");
        self.changed(cx);
    }

    /// 播放 / 暂停（下发到通道）。
    pub fn toggle_playback(&mut self, cx: &mut Context<Self>) {
        let Some(channel) = &self.channel else {
            return;
        };
        channel.toggle();
        cx.notify();
    }

    /// 是否有一条通道在跑（界面按钮只看它，不看状态机的过渡态）。
    pub fn is_running(&self) -> bool {
        self.channel.is_some()
    }

    /// 当前正在跑的通道类型（未启动为 `None`），由通道句柄派生，不额外保存副本。
    pub fn active_source(&self) -> Option<Source> {
        self.is_running().then_some(self.settings.source)
    }

    /// 首次打开窗口时按配置最小化；只生效一次，重开窗口不再自动最小化。
    pub fn take_start_minimized(&mut self) -> bool {
        std::mem::take(&mut self.start_minimized_pending)
    }

    /// 清空事件日志（界面「清空」按钮）。
    pub fn clear_log(&mut self, cx: &mut Context<Self>) {
        self.log.clear();
        cx.notify();
    }

    // ------------------------------------------------------------ 配置

    /// 切换启动源；运行中则重启通道，避免「界面显示 DLNA 但跑着妙播」。
    pub fn set_source(&mut self, source: Source, cx: &mut Context<Self>) {
        if self.settings.source == source {
            return;
        }
        self.settings.source = source;
        self.settings.save();
        self.restart_if_running(cx);
    }

    /// 切换广播设备类型（下次启动生效）。
    pub fn set_device_kind(&mut self, kind: DeviceKind, cx: &mut Context<Self>) {
        if self.settings.device_type == kind {
            return;
        }
        self.settings.device_type = kind;
        self.settings.save();
        cx.notify();
    }

    /// 切换关闭窗口行为（下次关窗生效）。
    pub fn set_close_action(&mut self, action: CloseAction, cx: &mut Context<Self>) {
        if self.settings.close_action == action {
            return;
        }
        self.settings.close_action = action;
        self.settings.save();
        cx.notify();
    }

    /// 选择输出设备：写盘，运行中则重启接收器让它立即生效。
    pub fn set_output_device(&mut self, id: String, cx: &mut Context<Self>) {
        if self.settings.output_device == id {
            return;
        }
        self.settings.output_device = id;
        self.settings.save();
        self.restart_if_running(cx);
    }

    /// 开关开机自启：写盘 + 操作注册表 + 记日志。
    pub fn set_start_with_windows(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.settings.start_with_windows = enabled;
        match system::set_autostart(enabled) {
            Ok(()) => self.push_log(if enabled {
                "已开启开机自启"
            } else {
                "已关闭开机自启"
            }),
            Err(error) => self.push_log(format!("开机自启设置失败：{error:#}")),
        }
        self.settings.save();
        cx.notify();
    }

    /// 翻转开机自启。
    pub fn toggle_start_with_windows(&mut self, cx: &mut Context<Self>) {
        let enabled = !self.settings.start_with_windows;
        self.set_start_with_windows(enabled, cx);
    }

    /// 开关「启动后最小化到托盘」（下次启动生效）。
    pub fn toggle_start_minimized(&mut self, cx: &mut Context<Self>) {
        self.settings.start_minimized = !self.settings.start_minimized;
        self.settings.save();
        cx.notify();
    }

    /// 开关详细日志（下次启动生效）。
    pub fn toggle_verbose_log(&mut self, cx: &mut Context<Self>) {
        self.settings.verbose_log = !self.settings.verbose_log;
        self.settings.save();
        cx.notify();
    }

    /// 应用一次文本编辑（只管改文本；焦点切换属于界面局部状态）。
    ///
    /// 这里只改内存并刷新界面，落盘在 [`Self::commit_text`]，因此逐键输入不会逐键写盘。
    pub fn type_key(&mut self, field: TextField, edit: TextEdit<'_>, cx: &mut Context<Self>) {
        let target = field.text_mut(&mut self.settings);
        match edit {
            TextEdit::Backspace => {
                target.pop();
            }
            TextEdit::Insert(text) => {
                if text.is_empty() || text.chars().any(char::is_control) {
                    return;
                }
                target.push_str(text);
            }
            TextEdit::Ignore => return,
        }
        // 设备名不允许为空：删到空时立刻回填一个字符，避免广播无名设备。
        if field == TextField::Name && self.settings.name.is_empty() {
            self.settings.name.push('m');
        }
        cx.notify();
    }

    /// 文本编辑结束（失焦 / 回车）：落盘一次并重绘。
    pub fn commit_text(&mut self, cx: &mut Context<Self>) {
        self.settings.save();
        cx.notify();
    }

    /// 只落盘不重绘：关窗收尾用（窗口马上就没了，没必要再排一帧）。
    pub fn flush_settings(&mut self) {
        self.settings.save();
    }

    // ------------------------------------------------------------ 事件

    /// 应用一条通道事件（解析已在通道侧完成，这里只做状态更新）。
    fn apply(&mut self, event: UiEvent, cx: &mut Context<Self>) {
        // 关停之后通道还会吐出收尾事件（停止回报、媒体流结束）：一律不再写状态，
        // 否则「信息」页会在已停止的情况下继续变（比如重建次数跳 1）。
        if !self.is_running() {
            return;
        }
        match event {
            UiEvent::Ready { name, address } => {
                self.set_status(RunState::Running);
                self.detail = "已上线，等待投送".to_owned();
                self.push_log(format!("已上线：{name} @ {address}"));
            }
            UiEvent::Stage { remote, stage } => {
                if let Some(remote) = remote {
                    self.peer = remote;
                }
                if stage == "transport_disconnected" {
                    self.stage.clear();
                } else {
                    self.stage = stage_label(&stage).to_owned();
                    self.push_log(format!("握手：{}", stage_label(&stage)));
                }
            }
            UiEvent::Source { name } => self.push_log(format!("对端设备：{name}")),
            UiEvent::OutputReady { name } => {
                self.active_output = Some(name.clone());
                self.push_log(format!("输出设备：{name}"));
            }
            UiEvent::StreamStarted { codec, sample_rate } => {
                self.playing.codec = codec;
                self.playing.sample_rate = sample_rate;
                self.push_log("开始接收媒体流");
            }
            UiEvent::AudioFormat { bitrate } => self.playing.bitrate = bitrate,
            UiEvent::Media {
                title,
                artist,
                album,
                duration_ms,
                media_url,
                artwork_url,
                position_ms,
            } => {
                if let Some(url) = media_url {
                    self.playing.media_host = host_of(&url);
                }
                if let Some(position) = position_ms {
                    self.playing.position_ms = position;
                }
                if !title.is_empty() && self.playing.title != title {
                    self.tracks += 1;
                    self.push_log(format!("曲目：{artist} - {title}"));
                }
                self.playing.title = title;
                self.playing.artist = artist;
                self.playing.album = album;
                self.playing.duration_ms = duration_ms;
                self.load_artwork(artwork_url);
            }
            UiEvent::Progress {
                position_ms,
                duration_ms,
            } => {
                self.playing.position_ms = position_ms;
                if let Some(duration) = duration_ms
                    && duration > 0
                {
                    self.playing.duration_ms = duration;
                }
            }
            UiEvent::Playback { playing } => {
                self.playing.playing = playing;
                self.detail = if playing { "播放中" } else { "已暂停" }.to_owned();
            }
            UiEvent::Volume { percent } => self.playing.volume = Some(percent),
            UiEvent::StreamClosed => {
                self.reconnects += 1;
                self.push_log("媒体流结束");
            }
            UiEvent::Error { message } => {
                self.set_status(RunState::Failed);
                self.detail = message.clone();
                self.push_log(format!("错误：{message}"));
            }
            UiEvent::Warning { message } => self.push_log(format!("警告：{message}")),
            UiEvent::Core { kind, message } => {
                if message.is_empty() {
                    self.push_log(format!("核心：{kind}"));
                } else {
                    self.push_log(format!("核心：{kind} · {message}"));
                }
            }
        }
        self.changed(cx);
    }

    /// 请求下载封面（下载由后台线程串行完成，这里只记地址，避免同一张图重复下载）。
    fn load_artwork(&mut self, artwork_url: Option<String>) {
        let Some(url) = artwork_url else {
            return;
        };
        if url.is_empty() || url == self.playing.artwork_url {
            return;
        }
        self.playing.artwork_url = url.clone();
        self.playing.artwork = None;
        if self.artwork_request_tx.try_send(url).is_err() {
            // 队列满：放弃这一张，清掉地址让下次换曲还能重试。
            self.playing.artwork_url.clear();
        }
    }

    // ------------------------------------------------------------ 内部工具

    /// 唯一的状态写入口：非法迁移记一条警告但照样生效（生产路径不 panic）。
    fn set_status(&mut self, next: RunState) {
        if self.status.transition(next).is_none() {
            log::warn!("非法状态迁移：{:?} → {next:?}（已强制应用）", self.status);
        }
        self.status = next;
    }

    /// 运行中则「停 → 起」，否则只刷新界面。
    fn restart_if_running(&mut self, cx: &mut Context<Self>) {
        if self.is_running() {
            self.stop(cx);
            self.start(cx);
        } else {
            cx.notify();
        }
    }

    /// 状态变化后的统一收尾：刷新界面 + 回传托盘。
    fn changed(&mut self, cx: &mut Context<Self>) {
        self.publish_status();
        cx.notify();
    }

    /// 把当前状态压成 [`StatusSummary`] 推给托盘；内容没变就不推（也不分配）。
    fn publish_status(&mut self) {
        let playing = &self.playing;
        if self.last_status.as_ref().is_some_and(|last| {
            last.status == self.status
                && last.playing == playing.playing
                && last.title == playing.title
                && last.artist == playing.artist
        }) {
            return;
        }
        let summary = StatusSummary {
            status: self.status,
            title: playing.title.clone(),
            artist: playing.artist.clone(),
            playing: playing.playing,
        };
        // 托盘线程可能还没起来或已退出，发不出去就算了。
        let _ = self.status_tx.send(summary.clone());
        self.last_status = Some(summary);
    }

    /// 追加一行事件日志：界面留一份，落盘交给日志线程。
    fn push_log(&mut self, text: impl Into<String>) {
        let line = format!("{}  {}", time_stamp(), text.into());
        let _ = self
            .log_tx
            .send((self.settings.log_file.clone(), line.clone()));
        self.log.push_back(SharedString::from(line));
        while self.log.len() > LOG_LIMIT {
            self.log.pop_front();
        }
    }
}

/// `HH:MM:SS`（本地时区不做换算，仅用于日志行前缀）。
fn time_stamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    format!(
        "{:02}:{:02}:{:02}",
        (seconds / 3600) % 24,
        (seconds / 60) % 60,
        seconds % 60
    )
}

/// 握手阶段英文标识 → 界面中文标签。
fn stage_label(stage: &str) -> &'static str {
    match stage {
        "transport_connected" => "已连接",
        "secure_channel_established" => "鉴权通过",
        "identity_exchanged" => "身份已交换",
        "capabilities_exchanged" => "能力已协商",
        "media_session_established" => "媒体会话建立",
        "transport_disconnected" => "已断开",
        _ => "协商中",
    }
}

/// 按文件头识别图片格式并构造 gpui 图像（字节直接移交，不再复制一份）。
fn decode_image(bytes: Vec<u8>) -> Option<Image> {
    let format = if bytes.starts_with(&[0xFF, 0xD8]) {
        ImageFormat::Jpeg
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        ImageFormat::Png
    } else if bytes.starts_with(b"RIFF") && bytes.len() > 12 && &bytes[8..12] == b"WEBP" {
        ImageFormat::Webp
    } else if bytes.starts_with(b"GIF8") {
        ImageFormat::Gif
    } else {
        return None;
    };
    Some(Image::from_bytes(format, bytes))
}

/// 下载封面；超过大小上限或下载失败时返回 `None`。
fn fetch_artwork(url: &str) -> Option<Vec<u8>> {
    let response = ureq::get(url)
        .header("User-Agent", "myplay/0.1")
        .call()
        .ok()?;
    let mut bytes = Vec::new();
    response
        .into_body()
        .into_reader()
        .take(ARTWORK_LIMIT)
        .read_to_end(&mut bytes)
        .ok()?;
    (!bytes.is_empty()).then_some(bytes)
}

/// 从媒体 URL 里取出「主机:端口」，用于信息页显示。
fn host_of(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    rest.split(['/', '?']).next().unwrap_or(rest).to_owned()
}

#[cfg(test)]
mod tests {
    use super::{StatusSummary, TextField, host_of, stage_label};
    use crate::core::config::Settings;
    use crate::core::state::RunState;

    #[test]
    fn tooltip_shows_track_when_playing() {
        let summary = StatusSummary {
            status: RunState::Running,
            title: "晴天".to_owned(),
            artist: "周杰伦".to_owned(),
            playing: true,
        };
        assert_eq!(summary.tooltip(), "myplay · 播放中 · 周杰伦 - 晴天");
    }

    #[test]
    fn tooltip_falls_back_to_run_state() {
        let summary = StatusSummary {
            status: RunState::Failed,
            ..StatusSummary::default()
        };
        assert_eq!(summary.tooltip(), "myplay · 错误");
    }

    #[test]
    fn text_fields_address_their_own_string() {
        let mut settings = Settings::default();
        TextField::LogFile
            .text_mut(&mut settings)
            .push_str("/tmp/a.log");
        assert_eq!(settings.log_file, "/tmp/a.log");
        assert_eq!(settings.name, Settings::default().name);
        assert_eq!(TextField::LogFile.text(&settings), "/tmp/a.log");
        assert_eq!(TextField::Name.text(&settings), Settings::default().name);
    }

    #[test]
    fn host_of_drops_path_and_scheme() {
        assert_eq!(
            host_of("http://music.126.net:80/a.mp3?x=1"),
            "music.126.net:80"
        );
        assert_eq!(host_of("music.126.net:80"), "music.126.net:80");
    }

    #[test]
    fn unknown_stage_uses_generic_label() {
        assert_eq!(stage_label("whatever"), "协商中");
        assert_eq!(stage_label("secure_channel_established"), "鉴权通过");
    }
}
