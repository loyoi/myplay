//! 主界面：左侧 Tab 导航 + 右侧内容区。
//!
//! 三个页面：设置（可配置项）、信息（广播/播放/连接统计）、日志（事件流）。
//! 数据流：业务状态全部在 `core::app::Core`（唯一真相源）里，本视图只保留界面局部状态
//! （当前 Tab、文本焦点、下拉开关、输出设备列表）；用户意图一律转发给 Core 的方法，
//! Core 每次 `notify` 都触发本视图重绘。

use gpui_kit::base::Link;
use gpui_kit::component::{
    Sizable, h_flex,
    input::{Textarea, TextareaState},
    spinner::Spinner,
    v_flex,
};
use gpui_kit::*;

use crate::channels::Snapshot;
use crate::core::app::{Core, NowPlaying, TextEdit, TextField};
use crate::core::config::{CloseAction, DeviceKind, Source};
use crate::core::state::RunState;
use crate::platform::audio::{self, OutputDevice};
use std::time::{Duration, Instant};

/// 项目仓库地址（`just release` 推送的就是它）。
const GITHUB_REPO: &str = "https://github.com/loyoi/myplay";
/// 播放器封面边长。
const COVER_SIZE: Pixels = px(132.);
/// 进度条高度与圆角（2px 圆角≈胶囊形，跟随条高）。
const PROGRESS_HEIGHT: Pixels = px(3.);
const PROGRESS_RADIUS: Pixels = px(2.);
/// 加载遮罩出现前的保留时间：任务在这段时间内完成就根本不出遮罩，避免一闪而过。
const LOADING_SHOW_DELAY: Duration = Duration::from_millis(200);
/// 遮罩已出现后的最短停留时间，防止出现后立刻消失造成的闪烁。
const LOADING_MIN_VISIBLE: Duration = Duration::from_millis(200);
/// 遮罩轮询间隔：够细让遮罩及时出现，又不至于让界面每帧都醒一次。
const LOADING_TICK: Duration = Duration::from_millis(40);

/// 加载遮罩的当前阶段（纯界面时序状态，业务状态仍在 [`Core`] 里）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum LoadingPhase {
    /// 没有在途任务，也没有遮罩。
    #[default]
    Idle,
    /// 有任务在途，但还在 [`LOADING_SHOW_DELAY`] 缓冲内：此时遮罩不显示。
    Pending(Instant),
    /// 遮罩已经画出来了：至少留到 `since + LOADING_MIN_VISIBLE`。
    Visible(Instant),
}

impl LoadingPhase {
    /// 遮罩此刻是否应该显示（只负责时序，不管业务是否还在跑）。
    fn shows_overlay(self, now: Instant) -> bool {
        match self {
            Self::Idle => false,
            Self::Pending(since) => now.saturating_duration_since(since) >= LOADING_SHOW_DELAY,
            Self::Visible(_) => true,
        }
    }

    /// 按「任务是否还在途」推进阶段，返回新阶段。
    ///
    /// 这条迁移是遮罩不闪的全部逻辑，因此单独成函数（单测只测它）：
    /// - 任务开始 → `Pending(now)`，200 ms 内完成就从未显示过遮罩；
    /// - 遮罩出现后任务就结束 → 转 `Visible(now)` 记下「该开始计最短停留」；
    /// - 最短停留也满了 → 回 `Idle`。
    fn advance(self, busy: bool, now: Instant) -> Self {
        match (self, busy) {
            (Self::Idle, false) => Self::Idle,
            (Self::Idle, true) => Self::Pending(now),
            (Self::Pending(since), true) => {
                if now.saturating_duration_since(since) >= LOADING_SHOW_DELAY {
                    // 越过延迟门槛即进入可见阶段，最短停留从现在起算。
                    Self::Visible(now)
                } else {
                    Self::Pending(since)
                }
            }
            // 还在缓冲里就结束了：遮罩一次都没画出来，直接收。
            (Self::Pending(_), false) => Self::Idle,
            (Self::Visible(since), false) => {
                if now.saturating_duration_since(since) >= LOADING_MIN_VISIBLE {
                    Self::Idle
                } else {
                    Self::Visible(since)
                }
            }
            (Self::Visible(since), true) => Self::Visible(since),
        }
    }
}

/// 右侧页面。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    Settings,
    Info,
    Log,
}

impl Route {
    const ALL: [Route; 3] = [Route::Settings, Route::Info, Route::Log];

    const fn label(self) -> &'static str {
        match self {
            Route::Settings => "设置",
            Route::Info => "信息",
            Route::Log => "日志",
        }
    }

    const fn hint(self) -> &'static str {
        match self {
            Route::Settings => "设备名、类型、网络与行为",
            Route::Info => "广播信息、当前播放、连接统计",
            Route::Log => "接收器事件流",
        }
    }
}

/// L1 视图：只有界面局部状态，业务状态全在 [`Core`]。
pub struct SettingsView {
    /// 唯一业务状态源（窗口销毁后它仍然活着）。
    core: Entity<Core>,
    /// 当前右侧页面。
    route: Route,
    /// 正在编辑的文本字段。
    focused: Option<TextField>,
    /// 文本输入的焦点句柄。
    focus: FocusHandle,
    /// 日志页的只读文本框（不可编辑，但能拖选 + Ctrl+C 复制）；窗口创建时建一次。
    log_text: Option<Entity<TextareaState>>,
    /// 已经写进文本框的日志版本；和 `Core::log_revision` 不同才重拼整段文本。
    log_revision: u64,
    /// 输出设备下拉是否展开。
    audio_open: bool,
    /// 枚举到的输出设备（打开页面时枚举一次；切换设备不重新枚举）。
    audio_devices: Vec<OutputDevice>,
    /// 加载遮罩的时序阶段（延迟出现 + 最短停留）。
    loading: LoadingPhase,
    /// 遮罩轮询任务是否已经跑着（在途任务只开一个泵）。
    loading_ticking: bool,
}

impl SettingsView {
    /// 绑定一个已存在的 [`Core`]，并订阅它的刷新。
    pub fn new(core: Entity<Core>, cx: &mut Context<Self>) -> Self {
        // Core 每次 notify → 视图重绘。视图销毁后回调自然失效（弱引用）。
        cx.observe(&core, |_view, _core, cx| cx.notify()).detach();

        Self {
            core,
            route: Route::Settings,
            focused: None,
            focus: cx.focus_handle(),
            log_text: None,
            log_revision: 0,
            audio_open: false,
            audio_devices: audio::list_output_devices(),
            loading: LoadingPhase::Idle,
            loading_ticking: false,
        }
    }

    /// 绑定窗口级行为：关闭窗口时把待落盘的文本提交，再按设置决定「回托盘」还是「退出」。
    ///
    /// 这里**不走平台的默认销毁**（回调返回 `false`）：那条路会先用 `DestroyWindow` 干掉 HWND，
    /// GPUI 随后在窗口 `Drop` 里再销毁一次，Windows 就会刷「无效的窗口句柄」。
    /// 改成 `remove_window` 后由 GPUI 自己移除窗口，句柄只销毁一次。
    /// 业务状态在 [`Core`] 里，窗口重开不丢。
    pub fn attach_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 日志文本框是窗口级控件（需要 `Window` 才能建），窗口重开时沿用同一个实体。
        if self.log_text.is_none() {
            self.log_text = Some(cx.new(|cx| {
                let mut state = TextareaState::new(window, cx);
                // 占位文字覆盖「日志被清空」和「刚建窗还没有日志」两种情况。
                state.set_placeholder("暂无事件", window, cx);
                // 日志一行一条：不软换行，过长的行改用横向滚动条看完。
                state.set_soft_wrap(false, window, cx);
                state
            }));
            // 上次关了窗口再打开时 Core 里可能早就有日志：建完立刻同步一次。
            self.sync_log_text(window, cx);
        }
        let weak = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            let close_action = weak
                .update(cx, |view, cx| {
                    // 关窗是文本编辑的终点：把内存里的改动落盘（窗口要没了，不再重绘）。
                    view.focused = None;
                    view.core.update(cx, |core, _| core.flush_settings());
                    view.core.read(cx).settings.close_action
                })
                .unwrap_or(CloseAction::Quit);
            window.remove_window();
            if close_action == CloseAction::Quit {
                // 退出推迟到下一轮前台任务：此刻还在 Windows 的关闭消息里，
                // 直接退出会和窗口收尾打架。
                cx.defer(|cx| cx.quit());
            }
            false
        });
    }

    /// 翻转开机自启：意图交给 Core（它在后台写注册表），这里负责把遮罩计时拉起来。
    fn toggle_autostart(&mut self, cx: &mut Context<Self>) {
        self.core
            .update(cx, |core, cx| core.toggle_start_with_windows(cx));
        self.sync_loading(cx);
    }

    /// 推进加载遮罩：按「业务是否还在途」与时间缓冲计算当前应处于哪个阶段。
    ///
    /// 遮罩本身就是一层视觉反馈，所以完全由界面计时驱动：
    /// - 任务开始后先等 [`LOADING_SHOW_DELAY`]，快任务根本不出遮罩（不闪）；
    /// - 遮罩一旦出现，至少留 [`LOADING_MIN_VISIBLE`]；
    /// - 需要时只开一个轮询任务，空闲后自动退出，不成常驻计时器。
    fn sync_loading(&mut self, cx: &mut Context<Self>) {
        let busy = self.core.read(cx).autostart.is_applying();
        let before = self.loading;
        self.loading = self.loading.advance(busy, Instant::now());
        if self.loading != before {
            cx.notify();
        }
        // 还有待完成的事（在途任务或遮罩的最短停留）才开表。
        let waiting = busy || self.loading != LoadingPhase::Idle;
        if !waiting || self.loading_ticking {
            return;
        }
        self.loading_ticking = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(LOADING_TICK).await;
                // `Ok(true)` = 收尾；`Err(_)` = 视图已释放（窗口关了），泵自然退出。
                let finished = this.update(cx, |view, cx| {
                    let busy = view.core.read(cx).autostart.is_applying();
                    view.loading = view.loading.advance(busy, Instant::now());
                    cx.notify();
                    let finished = !busy && view.loading == LoadingPhase::Idle;
                    if finished {
                        view.loading_ticking = false;
                    }
                    finished
                });
                if !matches!(finished, Ok(false)) {
                    break;
                }
            }
        })
        .detach();
    }
}

// ---------------------------------------------------------------- 渲染

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 遮罩是否显示由时序阶段决定；在途任务由泵在后台推进，渲染本身不改状态。
        let overlay = self.loading.shows_overlay(Instant::now());
        let mut root = h_flex()
            .size_full()
            .relative()
            .bg(rgb(0x16171b))
            .text_color(rgb(0xdfe3ea))
            .child(self.render_sidebar(cx))
            .child(self.render_content(window, cx));
        if overlay {
            root = root.child(render_loading_overlay());
        }
        root
    }
}

/// 加载遮罩：盖住整个窗口内容并拦住点击（加遮罩的同时禁止重复操作）。
fn render_loading_overlay() -> impl IntoElement {
    div()
        .id("loading-overlay")
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        // 遮罩底色：带 alpha 的深色（`rgba` 的 hex 是 0xRRGGBBAA），约 67% 不透明。
        .bg(gpui_kit::rgba(0x0d0e11aa))
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(rgb(0x22252b))
                .border_1()
                .border_color(rgb(0x33363d))
                .child(Spinner::new().small().color(rgb(0xa8aebb).into()))
                .child(div().text_sm().text_color(rgb(0xdfe3ea)).child("正在应用…")),
        )
}

impl SettingsView {
    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.route;
        let mut column = v_flex()
            .w(px(184.))
            .h_full()
            .flex_shrink_0()
            .p_3()
            .gap_1()
            .bg(rgb(0x1a1c21))
            .child(
                v_flex()
                    .gap_1()
                    .pb_3()
                    .child(div().text_lg().text_color(rgb(0xf2f4f8)).child("myplay"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x8b919d))
                            .child("小米妙播接收器"),
                    ),
            );

        for route in Route::ALL {
            let selected = current == route;
            column = column.child(
                div()
                    .id(route.label())
                    .w_full()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .cursor_pointer()
                    .bg(if selected {
                        rgb(0x2a3550)
                    } else {
                        rgb(0x1a1c21)
                    })
                    .text_color(if selected {
                        rgb(0xf2f4f8)
                    } else {
                        rgb(0xa8aebb)
                    })
                    .text_sm()
                    .child(route.label())
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.route = route;
                        cx.notify();
                    })),
            );
        }

        column.child(div().flex_1()).child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x6f7683))
                        .child(concat!("v", env!("CARGO_PKG_VERSION"))),
                )
                .child(github_link()),
        )
    }

    fn render_content(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 只读 Core 时把需要的值一次取完，避免持有借用再去拿 `&mut Context`。
        let (status, running, note) = {
            let core = self.core.read(cx);
            (core.status, core.is_running(), core.detail.clone())
        };
        let status_color = match status {
            RunState::Running => rgb(0x3ecf8e),
            RunState::Failed => rgb(0xff6b81),
            _ => rgb(0x8b919d),
        };
        let route = self.route;
        // 未启动且没有待解释的原因：标题栏给一句下一步提示（徽标只说「未启动」）。
        let idle_hint = status == RunState::Idle && note.is_empty();

        let page: gpui_kit::AnyElement = match route {
            Route::Settings => self.render_settings_page(cx).into_any_element(),
            Route::Info => self.render_info_page(cx).into_any_element(),
            Route::Log => self.render_log_page(window, cx).into_any_element(),
        };

        let mut root = v_flex().flex_1().h_full().child(
            h_flex()
                .justify_between()
                .items_center()
                .px_4()
                .py_3()
                .gap_3()
                .bg(rgb(0x1a1c21))
                .child(
                    v_flex()
                        .gap_1()
                        .child(
                            div()
                                .text_lg()
                                .text_color(rgb(0xf2f4f8))
                                .child(route.label()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(0x8b919d))
                                .child(route.hint()),
                        ),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .children(idle_hint.then(|| {
                            div()
                                .text_xs()
                                .text_color(rgb(0x6f7683))
                                .child("点击「启动」开始接收")
                        }))
                        .child(
                            div()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .bg(rgb(0x22252b))
                                .text_xs()
                                .text_color(status_color)
                                .child(status.label()),
                        )
                        .child(if running {
                            action_button("停止", true, "btn-stop").on_click(cx.listener(
                                |view, _, _, cx| {
                                    view.core.update(cx, |core, cx| core.stop(cx));
                                },
                            ))
                        } else {
                            action_button("启动", false, "btn-start").on_click(cx.listener(
                                |view, _, _, cx| {
                                    view.core.update(cx, |core, cx| core.start(cx));
                                },
                            ))
                        }),
                ),
        );
        // 徽标放不下的原因（失败、等待网络、重试中）才占一行。
        if !note.is_empty() {
            root = root.child(status_note(&note, status));
        }
        root.child(page)
    }

    fn render_settings_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (start_with_windows, start_minimized, verbose_log) = {
            let core = self.core.read(cx);
            (
                core.settings.start_with_windows,
                core.settings.start_minimized,
                core.settings.verbose_log,
            )
        };

        v_flex()
            .id("page-settings")
            .overflow_y_scroll()
            .flex_1()
            .p_4()
            .gap_3()
            .child(
                panel("接收器")
                    .child(self.render_source(cx))
                    .child(self.render_field("设备名", TextField::Name, cx))
                    .child(self.render_device_kind(cx)),
            )
            .child(self.render_audio(cx))
            .child(
                panel("行为")
                    .child(self.render_close_action(cx))
                    .child(toggle_row(
                        "开机自启",
                        "toggle-boot",
                        start_with_windows,
                        cx.listener(|view, _, _, cx| {
                            view.toggle_autostart(cx);
                        }),
                    ))
                    .child(toggle_row(
                        "启动后最小化到托盘",
                        "toggle-min",
                        start_minimized,
                        cx.listener(|view, _, _, cx| {
                            view.core
                                .update(cx, |core, cx| core.toggle_start_minimized(cx));
                        }),
                    ))
                    .child(toggle_row(
                        "详细日志",
                        "toggle-verbose",
                        verbose_log,
                        cx.listener(|view, _, _, cx| {
                            view.core.update(cx, |core, cx| core.toggle_verbose_log(cx));
                        }),
                    )),
            )
            .child(
                panel("诊断")
                    .child(self.render_field("日志文件（留空＝不写）", TextField::LogFile, cx))
                    .child(self.render_field("退休实例（逗号分隔）", TextField::RetireLyra, cx))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x8b919d))
                            .child("广播身份固定：单条目 · 真实 MAC"),
                    ),
            )
    }

    fn render_info_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let core = self.core.read(cx);

        let uptime = core
            .started_at
            .map(|start| fmt_duration(start.elapsed().as_millis() as u64))
            .unwrap_or_else(|| "—".to_owned());

        let broadcast_rows: Vec<(&'static str, String)> = match &core.snapshot {
            Some(snapshot) => snapshot_rows(snapshot),
            None => match core.active_source() {
                // 输出设备放在播放卡片里，这里不再重复一行。
                Some(source) if source != Source::MiPlay => vec![
                    ("通道", source.label().to_owned()),
                    ("设备名", core.settings.name.clone()),
                ],
                _ => vec![("状态", "未启动".to_owned())],
            },
        };

        let stats_rows = if core.active_source() == Some(Source::Dlna) {
            vec![
                (
                    "媒体主机",
                    if core.playing.media_host.is_empty() {
                        "—".to_owned()
                    } else {
                        core.playing.media_host.clone()
                    },
                ),
                ("运行时长", uptime),
                ("本次曲目数", core.tracks.to_string()),
                ("媒体流重建", core.reconnects.to_string()),
            ]
        } else {
            vec![
                (
                    "对端",
                    if core.peer.is_empty() {
                        "—".to_owned()
                    } else {
                        core.peer.clone()
                    },
                ),
                (
                    "握手阶段",
                    if core.stage.is_empty() {
                        "—".to_owned()
                    } else {
                        core.stage.clone()
                    },
                ),
                ("运行时长", uptime),
                ("本次曲目数", core.tracks.to_string()),
                ("媒体流重建", core.reconnects.to_string()),
            ]
        };

        let player = self.render_now_playing(cx);

        v_flex()
            .id("page-info")
            .overflow_y_scroll()
            .flex_1()
            .p_4()
            .gap_3()
            .child(player)
            .child(card("广播信息", broadcast_rows))
            .child(card("连接与统计", stats_rows))
    }

    /// 「当前播放」：左侧封面 + 右侧曲名/元信息/进度条，按播放器排版。
    ///
    /// 空态与缺封面都用同尺寸占位，换曲或缺图时布局不跳动。
    fn render_now_playing(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let core = self.core.read(cx);
        let playing = &core.playing;
        let has_media = !playing.title.is_empty();

        let cover: AnyElement = match &playing.artwork {
            Some(image) => img(image.clone())
                .w(COVER_SIZE)
                .h(COVER_SIZE)
                .flex_shrink_0()
                .rounded_lg()
                .into_any_element(),
            None => div()
                .w(COVER_SIZE)
                .h(COVER_SIZE)
                .flex_shrink_0()
                .rounded_lg()
                .bg(rgb(0x24262c))
                .flex()
                .items_center()
                .justify_center()
                .text_xs()
                .text_color(rgb(0x6f7683))
                .child(if has_media { "无封面" } else { "未播放" })
                .into_any_element(),
        };

        let title = if has_media {
            playing.title.clone()
        } else {
            "未在播放".to_owned()
        };
        // 副标题只拼有内容的字段，避免出现「 ·  · 」这类空分隔。
        let subtitle = [playing.artist.as_str(), playing.album.as_str()]
            .into_iter()
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
        let subtitle = if has_media && subtitle.is_empty() {
            "未知艺术家".to_owned()
        } else if has_media {
            subtitle
        } else {
            "从手机投送后，这里会显示曲目、封面与进度".to_owned()
        };

        let meta = format_meta(playing);
        let output = core.active_output.clone().unwrap_or_else(|| "—".to_owned());
        let volume = playing
            .volume
            .map(|percent| format!("音量 {percent}%"))
            .unwrap_or_else(|| "音量 —".to_owned());

        let mut column = v_flex()
            .flex_1()
            .min_w(px(0.))
            .gap_1()
            .child(
                div()
                    .text_lg()
                    .text_color(rgb(0xf2f4f8))
                    .truncate()
                    .child(title),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0xa8aebb))
                    .truncate()
                    .child(subtitle),
            );

        // 进度条只在有媒体时出现：没有播放内容时它既没有信息量，也避免显示 00:00。
        if has_media {
            let fraction = if playing.duration_ms == 0 {
                0.0
            } else {
                (playing.position_ms as f32 / playing.duration_ms as f32).clamp(0.0, 1.0)
            };
            let progress_fill = (fraction > 0.0).then(|| {
                div()
                    .h_full()
                    .w(relative(fraction))
                    .rounded(PROGRESS_RADIUS)
                    .bg(rgb(0x2f6fed))
            });
            let total = if playing.duration_ms == 0 {
                "--:--".to_owned()
            } else {
                fmt_duration(playing.duration_ms)
            };
            let bar = h_flex()
                .w_full()
                .h(PROGRESS_HEIGHT)
                .rounded(PROGRESS_RADIUS)
                .bg(rgb(0x33363d))
                .children(progress_fill);
            column = column.child(div().w_full().pt_2().child(bar)).child(
                h_flex()
                    .justify_between()
                    .text_xs()
                    .text_color(rgb(0x8b919d))
                    .child(fmt_duration(playing.position_ms))
                    .child(total),
            );
        }

        if !meta.is_empty() {
            column = column.child(div().text_xs().text_color(rgb(0x8b919d)).child(meta));
        }

        if has_media {
            column = column.child(
                h_flex()
                    .w_full()
                    .gap_4()
                    .text_xs()
                    .text_color(rgb(0x8b919d))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .child(format!("输出：{output}")),
                    )
                    .child(div().flex_shrink_0().child(volume)),
            );
        }

        panel("当前播放").child(
            h_flex()
                .w_full()
                .items_center()
                .gap_4()
                .child(cover)
                .child(column),
        )
    }

    /// 日志页：整段日志放进只读文本框。
    ///
    /// 选中与复制全部交给控件（拖选 + Ctrl+C），`Core` 只提供文本；
    /// 内容同步在 [`Self::sync_log_text`] 里，这里只管渲染。
    fn render_log_page(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let total = self.core.read(cx).log.len();
        self.sync_log_text(window, cx);

        // 文本框在 `attach_window` 里就建好了；窗口生命周期外不会走到这里。
        let Some(state) = &self.log_text else {
            return div()
                .text_xs()
                .text_color(rgb(0x6f7683))
                .child("暂无事件")
                .into_any_element();
        };

        let body = div()
            .flex_1()
            .min_h_0()
            .rounded_md()
            .border_1()
            // 与其它卡片一致（`panel` / `card` 的底色与边框）。
            .border_color(rgb(0x33363d))
            .bg(rgb(0x1f2127))
            .text_xs()
            .child(
                // 多行输入默认是 `h_auto()`（只有一行高），这里显式撑满剩余高度。
                Textarea::new(state)
                    .readonly(true)
                    .appearance(false)
                    .bordered(false)
                    .h_full(),
            );

        v_flex()
            .id("page-log")
            .flex_1()
            .p_4()
            .gap_2()
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x8b919d))
                            .child(format!("共 {total} 条（最新在前，可拖选复制）")),
                    )
                    .child(
                        action_button("清空", false, "btn-clear-log").on_click(cx.listener(
                            |view, _, _, cx| {
                                view.core.update(cx, |core, cx| core.clear_log(cx));
                            },
                        )),
                    ),
            )
            .child(body)
            .into_any_element()
    }

    /// 把最新日志写进只读文本框：只在「有新行」且用户没在选文字时动它。
    ///
    /// `set_value` 会清掉选区并把视图拉回顶部，所以拿选区当「用户正在复制」的信号：
    /// 有选区就这一拍不动（版本号也不推进，下一条日志再同步）；
    /// 否则写完把阅读位置补回新插入的一行，正在看旧日志的人不会被顶走。
    fn sync_log_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.log_text.clone() else {
            return;
        };
        let revision = self.core.read(cx).log_revision;
        if revision == self.log_revision {
            return;
        }
        if !state.read(cx).selected_range().is_empty() {
            return;
        }
        self.log_revision = revision;
        let text = self.core.read(cx).log_text();
        state.update(cx, move |state, cx| {
            let mut offset = state.scroll_offset();
            state.set_value(text, window, cx);
            // 贴着顶部时不恢复：那正是「最新在前」的阅读位置。
            if offset.y > px(0.) {
                if let Some(line) = state.line_height() {
                    offset.y += line;
                }
                state.set_scroll_offset(offset, cx);
            }
        });
    }

    fn render_field(
        &self,
        label: &str,
        field: TextField,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let focused = self.focused == Some(field);
        let value = {
            let core = self.core.read(cx);
            field.text(&core.settings).to_owned()
        };
        let shown = if value.is_empty() && !focused {
            "—".to_owned()
        } else {
            value
        };

        v_flex().gap_1().child(field_caption(label)).child(
            div()
                .id(field.element_id())
                .w_full()
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(if focused {
                    rgb(0x2f6fed)
                } else {
                    rgb(0x33363d)
                })
                .bg(if focused {
                    rgb(0x263049)
                } else {
                    rgb(0x24262c)
                })
                .cursor_pointer()
                .track_focus(&self.focus)
                .on_click(cx.listener(move |view, _, window, cx| {
                    // 换字段即提交上一个字段（落盘一次），逐键输入不写盘。
                    if view.focused.is_some_and(|current| current != field) {
                        view.core.update(cx, |core, cx| core.commit_text(cx));
                    }
                    let handle = view.focus.clone();
                    view.focused = Some(field);
                    window.focus(&handle, cx);
                    cx.notify();
                }))
                .on_key_down(cx.listener(move |view, event: &KeyDownEvent, _window, cx| {
                    let key = event.keystroke.key.as_str();
                    // 焦点进出属于界面局部状态，留在视图里处理；顺带提交改动。
                    if key == "escape" || key == "enter" {
                        view.focused = None;
                        view.core.update(cx, |core, cx| core.commit_text(cx));
                        return;
                    }
                    let edit = match key {
                        "backspace" => TextEdit::Backspace,
                        _ => event
                            .keystroke
                            .key_char
                            .as_deref()
                            .map_or(TextEdit::Ignore, TextEdit::Insert),
                    };
                    view.core
                        .update(cx, |core, cx| core.type_key(field, edit, cx));
                }))
                .child(div().text_sm().text_color(rgb(0xe6e9ef)).child(shown)),
        )
    }

    /// 音频输出设备下拉：显示当前选择与实际正在使用的设备。
    fn render_audio(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (current_label, active_output) = {
            let core = self.core.read(cx);
            let current_label = if core.settings.output_device.is_empty() {
                "系统默认".to_owned()
            } else {
                self.audio_devices
                    .iter()
                    .find(|device| device.id == core.settings.output_device)
                    .map(|device| device.name.clone())
                    .unwrap_or_else(|| core.settings.output_device.clone())
            };
            (current_label, core.active_output.clone())
        };

        let mut panel = panel("音频").child(field_caption("输出设备")).child(
            h_flex()
                .id("audio-selector")
                .justify_between()
                .items_center()
                .w_full()
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(if self.audio_open {
                    rgb(0x2f6fed)
                } else {
                    rgb(0x33363d)
                })
                .bg(rgb(0x24262c))
                .cursor_pointer()
                .text_sm()
                .text_color(rgb(0xe6e9ef))
                .child(current_label)
                .child(div().text_xs().text_color(rgb(0x8b919d)).child("▾"))
                .on_click(cx.listener(|view, _, _, cx| {
                    view.audio_open = !view.audio_open;
                    cx.notify();
                })),
        );

        if self.audio_open {
            let mut list = v_flex().gap_1();
            // 「系统默认」永远在列表首位。
            list = list.child(self.audio_option(None, "系统默认".to_owned(), cx));
            for device in &self.audio_devices {
                list =
                    list.child(self.audio_option(Some(device.id.clone()), device.name.clone(), cx));
            }
            if self.audio_devices.is_empty() {
                list = list.child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x6f7683))
                        .child("未枚举到输出设备，只能使用系统默认"),
                );
            }
            panel = panel.child(list);
        }

        panel.child(
            div()
                .text_xs()
                .text_color(rgb(0x8b919d))
                .child(match active_output {
                    Some(name) => format!("正在使用：{name}"),
                    None => "未开始播放".to_owned(),
                }),
        )
    }

    fn audio_option(
        &self,
        id: Option<String>,
        label: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = {
            let core = self.core.read(cx);
            core.settings.output_device.as_str() == id.as_deref().unwrap_or_default()
        };
        // 元素 id 由领域值（设备 id）派生，不用列表下标。
        let element_id = match &id {
            Some(device_id) => ElementId::Name(SharedString::from(format!("audio-{device_id}"))),
            None => ElementId::from("audio-system-default"),
        };
        div()
            .id(element_id)
            .w_full()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(if selected {
                rgb(0x263049)
            } else {
                rgb(0x1f2127)
            })
            .text_sm()
            .text_color(if selected {
                rgb(0xf2f4f8)
            } else {
                rgb(0xdfe3ea)
            })
            .cursor_pointer()
            .child(label)
            .on_click(cx.listener(move |view, _, _, cx| {
                view.audio_open = false;
                let id = id.clone().unwrap_or_default();
                view.core
                    .update(cx, |core, cx| core.set_output_device(id, cx));
            }))
    }

    /// 启动源选择：小米妙播 / DLNA。
    fn render_source(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let source = self.core.read(cx).settings.source;
        let row = chip_row(&Source::ALL, Source::label, |candidate, text| {
            chip(text, source == candidate).on_click(cx.listener(move |view, _, _, cx| {
                view.core
                    .update(cx, |core, cx| core.set_source(candidate, cx));
            }))
        });
        v_flex()
            .gap_1()
            .child(field_caption("启动源"))
            .child(row)
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(0x8b919d))
                    .child(source.hint()),
            )
    }

    fn render_device_kind(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.core.read(cx).settings.device_type;
        let row = chip_row(&DeviceKind::ALL, DeviceKind::label, |kind, text| {
            chip(text, current == kind).on_click(cx.listener(move |view, _, _, cx| {
                view.core
                    .update(cx, |core, cx| core.set_device_kind(kind, cx));
            }))
        });
        v_flex().gap_1().child(field_caption("设备类型")).child(row)
    }

    fn render_close_action(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.core.read(cx).settings.close_action;
        let row = chip_row(&CloseAction::ALL, CloseAction::label, |action, text| {
            chip(text, current == action).on_click(cx.listener(move |view, _, _, cx| {
                view.core
                    .update(cx, |core, cx| core.set_close_action(action, cx));
            }))
        });
        v_flex()
            .gap_1()
            .child(field_caption("关闭窗口时"))
            .child(row)
    }
}

// ---------------------------------------------------------------- 小组件

/// 妙播广播信息 → 信息页的行。
fn snapshot_rows(snapshot: &Snapshot) -> Vec<(&'static str, String)> {
    vec![
        ("通道", "小米妙播".to_owned()),
        ("设备名", snapshot.name.clone()),
        ("设备类型", snapshot.device_type.to_owned()),
        ("地址", snapshot.local_ip.clone()),
        ("接口", snapshot.interface.clone()),
        ("广播 MAC", snapshot.hardware_address.clone()),
        ("mDNS", snapshot.mdns_service.clone()),
        ("Lyra 端口", snapshot.lyra_port.to_string()),
        ("公布模式", "单条目".to_owned()),
    ]
}

/// 播放参数单行文本：`MP3 · 44100 Hz · 320 kbps`（空字段自动跳过）。
fn format_meta(playing: &NowPlaying) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(3);
    if !playing.codec.is_empty() {
        parts.push(playing.codec.clone());
    }
    if playing.sample_rate > 0 {
        parts.push(format!("{} Hz", playing.sample_rate));
    }
    if playing.bitrate > 0 {
        parts.push(format!("{} kbps", playing.bitrate / 1000));
    }
    parts.join(" · ")
}

fn fmt_duration(ms: u64) -> String {
    let total = ms / 1000;
    format!("{:02}:{:02}", total / 60, total % 60)
}

fn panel(title: &'static str) -> gpui_kit::Div {
    v_flex()
        .gap_2()
        .p_3()
        .rounded_lg()
        .bg(rgb(0x1f2127))
        .child(section_title(title))
}

fn section_title(text: &str) -> impl IntoElement {
    div()
        .text_sm()
        .text_color(rgb(0xf2f4f8))
        .child(text.to_owned())
}

/// 状态补充说明：徽标说不清的那部分（失败原因、等待网络、重试中）。
///
/// 只在 `Core::detail` 非空时渲染，失败用错误色，其余用次要文字色。
fn status_note(text: &str, status: RunState) -> impl IntoElement {
    let color = if status == RunState::Failed {
        rgb(0xff6b81)
    } else {
        rgb(0x8b919d)
    };
    div()
        .px_4()
        .pt_2()
        .text_xs()
        .text_color(color)
        .child(text.to_owned())
}

fn field_caption(text: &str) -> impl IntoElement {
    div()
        .text_xs()
        .text_color(rgb(0x8b919d))
        .child(text.to_owned())
}

fn card(title: &'static str, rows: Vec<(&'static str, String)>) -> gpui_kit::Div {
    let mut column = panel(title);
    for (key, value) in rows {
        column = column.child(
            h_flex()
                .justify_between()
                .gap_4()
                .child(
                    div()
                        .w(px(96.))
                        .flex_shrink_0()
                        .text_xs()
                        .text_color(rgb(0x8b919d))
                        .child(key),
                )
                .child(
                    div()
                        .flex_1()
                        .text_sm()
                        .text_color(rgb(0xe6e9ef))
                        .child(value),
                ),
        );
    }
    column
}

/// 一组单选 chip：`label` 提供文案，`build` 负责选中态与点击行为（各调用点自己知道语义）。
fn chip_row<T: Copy>(
    values: &[T],
    label: fn(T) -> &'static str,
    mut build: impl FnMut(T, &'static str) -> gpui_kit::Stateful<gpui_kit::Div>,
) -> impl IntoElement {
    let mut row = h_flex().flex_wrap().gap_1();
    for value in values.iter().copied() {
        let text = label(value);
        row = row.child(build(value, text));
    }
    row
}

fn chip(label: &'static str, selected: bool) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(label)
        .px_2()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(if selected {
            rgb(0x2f6fed)
        } else {
            rgb(0x33363d)
        })
        .bg(if selected {
            rgb(0x2f6fed)
        } else {
            rgb(0x24262c)
        })
        .text_sm()
        .text_color(if selected {
            rgb(0xffffff)
        } else {
            rgb(0xdfe3ea)
        })
        .cursor_pointer()
        .child(label)
}

/// 侧边栏底部的 GitHub 入口：图标点击后交给系统浏览器打开项目仓库。
///
/// 用 `base::Link` 而不是普通 `div` + `on_click`：它自带角色、焦点与键盘激活语义。
fn github_link() -> impl IntoElement {
    Link::new("link-github")
        .href(GITHUB_REPO)
        .open_with(|href, _, _, cx| cx.open_url(href))
        .accessibility_label("在浏览器中打开 myplay 的 GitHub 仓库")
        .text_xs()
        .text_color(rgb(0x6f7683))
        .cursor_pointer()
        .hover(|style| style.text_color(rgb(0xdfe3ea)))
        // 图标跟随环境文字色（含 hover），因此不另设颜色。
        .child(gpui_kit::assets::IconName::Github)
}

fn action_button(
    label: &'static str,
    destructive: bool,
    id: &'static str,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .bg(if destructive {
            rgb(0xd03050)
        } else {
            rgb(0x2f6fed)
        })
        .text_color(rgb(0xffffff))
        .text_sm()
        .cursor_pointer()
        .child(label)
}

fn toggle_row(
    label: &'static str,
    id: &'static str,
    on: bool,
    handler: impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    // 整行可点：方框在文字前面，勾选态由方框填充 + 对勾表达。
    h_flex()
        .id(id)
        .items_center()
        .gap_2()
        .cursor_pointer()
        .on_click(handler)
        .child(
            div()
                .w(px(14.))
                .h(px(14.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(3.))
                .border_1()
                .border_color(if on { rgb(0x2f6fed) } else { rgb(0x4a4f5a) })
                .bg(if on { rgb(0x2f6fed) } else { rgb(0x1f2127) })
                .text_color(rgb(0xffffff))
                .text_xs()
                .child(if on { "✓" } else { " " }),
        )
        .child(div().text_sm().text_color(rgb(0xdfe3ea)).child(label))
}

#[cfg(test)]
mod tests {
    use super::{LOADING_MIN_VISIBLE, LOADING_SHOW_DELAY, LoadingPhase};
    use std::time::{Duration, Instant};

    /// 阶段推进只依赖「任务是否在途」与时间，因此用假时间点驱动，不碰真实计时器。
    fn at(base: Instant, millis: u64) -> Instant {
        base + Duration::from_millis(millis)
    }

    #[test]
    fn fast_task_never_shows_overlay() {
        let start = Instant::now();
        // 任务在 200 ms 缓冲内完成：全程都不该显示遮罩。
        let phase = LoadingPhase::Idle.advance(true, start);
        assert!(!phase.shows_overlay(at(start, 0)));
        let phase = phase.advance(true, at(start, 150));
        assert!(!phase.shows_overlay(at(start, 150)));
        let phase = phase.advance(false, at(start, 180));
        assert_eq!(phase, LoadingPhase::Idle);
        assert!(!phase.shows_overlay(at(start, 180)));
    }

    #[test]
    fn slow_task_shows_overlay_after_delay() {
        let start = Instant::now();
        let phase = LoadingPhase::Idle.advance(true, start);
        // 越过延迟门槛才可见。
        let phase = phase.advance(true, at(start, LOADING_SHOW_DELAY.as_millis() as u64));
        assert!(phase.shows_overlay(at(start, 200)));
    }

    #[test]
    fn overlay_holds_for_minimum_visible_time() {
        let start = Instant::now();
        let shown = LoadingPhase::Idle
            .advance(true, start)
            .advance(true, at(start, 200));
        assert!(shown.shows_overlay(at(start, 200)));

        // 任务立刻结束：遮罩进入「最短停留」并从此刻开始计时。
        let holding = shown.advance(false, at(start, 210));
        assert!(holding.shows_overlay(at(start, 210)));
        // 停留期内仍然可见，不会闪一下就没。
        let still = holding.advance(false, at(start, 210 + 100));
        assert!(still.shows_overlay(at(start, 310)));
        // 停留期满才收起。
        let done = still.advance(
            false,
            at(start, 210 + LOADING_MIN_VISIBLE.as_millis() as u64),
        );
        assert_eq!(done, LoadingPhase::Idle);
    }

    #[test]
    fn new_task_extends_existing_overlay() {
        let start = Instant::now();
        let shown = LoadingPhase::Idle
            .advance(true, start)
            .advance(true, at(start, 200));
        // 连续提交：已经可见的遮罩不该被重置回 Pending（否则会闪一下消失再出现）。
        let again = shown.advance(true, at(start, 400));
        assert!(again.shows_overlay(at(start, 400)));
    }
}
