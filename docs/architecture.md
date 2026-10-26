# myplay 架构（四层）

```
L1 ui/          按需：窗口关闭即销毁
L2 shell/       常驻：托盘 + 窗口生命周期
L3 core/        常驻：唯一真相源（配置/状态机/事件归一/Core）
L4 channels/    常驻：妙播 / DLNA 两条通道
```

四层都在应用成员 `crates/myplay/src/` 下；`crates/` 的另外四个成员是接收器源码（见下）。

## 工作区

```text
Cargo.toml                      # [workspace]：members / default-members / [workspace.dependencies]（唯一版本来源）/ profile
Cargo.lock                      # 工作区唯一锁文件
crates/myplay/                  # 应用：src/（四层）+ shell / platform，唯一 bin，自带 build.rs 与 assets/
crates/fusionplay-miplay-sdk/   # 小米妙播协议与媒体接收（AGPL-3.0-only）
crates/airplay-receiver-core/   # DLNA DMR 核心，lib 名 fusionplay_core（AGPL-3.0-only；AirPlay 已移除）
crates/fusionplay-if-addrs/     # if-addrs 适配层，crate 名仍为 if-addrs（AGPL-3.0-only）
```

- 三份接收器源码不再引用仓库外的 path，内联来源、提交号与内联时的改动见各目录的
  `UPSTREAM.md` 与根 `README.md`「内置源码与上游来源」。
- 依赖方向单向：`crates/myplay` → `airplay-receiver-core` → `if-addrs`；
  被内联的源码不得反向依赖 `myplay`，也不得碰 GPUI。
- 依赖版本只在根 `[workspace.dependencies]` 声明，成员清单只写 `workspace = true` + 自己的 feature；
  内联成员的 path 也只出现在根表。
- 音频栈全仓库一套：`cpal 0.18` + `symphonia 0.6.1`（应用与三份内联源码共用）。
  升级这两个 crate 等于改全仓库，必须做设备回归。
- profile 只在根生效：`[profile.dev] opt-level = 1` 针对应用，依赖与三份内联源码显式
  `opt-level = 2`（`package."*"` 不覆盖工作区成员）。
- `if-addrs` 在工作区里存在两份：`crates/fusionplay-if-addrs`（应用与核心使用的适配层）与
  crates.io 版本（`if-addrs` 自身依赖的 `getifs` 无关，其余来自 gpui 生态）；上游核心清单里的
  `[patch.crates-io]` 未沿用。
- **没有 AirPlay 通道**：`shairplay` 的 FusionPlay 分支补丁（DACP / MediaRemote）不在 crates.io
  版本里，因此 RAOP 音频引擎、配对存储、HLS 视频桥与 `Source::AirPlay` 已删除，
  核心只剩 DLNA DMR（SSDP / GENA / HTTP）。

## 分层职责

| 层 | 目录 | 职责 | 禁止 |
|---|---|---|---|
| L1 | `crates/myplay/src/ui/` | 渲染、输入、局部 UI 状态（tab/焦点/下拉/设备列表） | 持有通道句柄、直接解析通道事件、持有业务状态 |
| L2 | `crates/myplay/src/shell/` | 托盘图标与菜单（含 tooltip 显示）、窗口创建/销毁、全局视图实体 | 持有业务状态、直接调通道 |
| L3 | `crates/myplay/src/core/` | 配置、状态机、事件归一、`Core`（拥有通道与日志） | 依赖 gpui 之外的 UI 细节 |
| L4 | `crates/myplay/src/channels/` | 每条通道的启停与事件产出（统一 `Channel` trait） | 直接触达 UI / 托盘 |

## 已完成

- **L2**：`shell/tray.rs`（托盘线程 + 右键菜单 + 代码生成图标 + tooltip 回传刷新，左键只开窗口）、
  `shell/window.rs`（`SharedView` 全局、`APP_TITLE`、单例 `open_main_window` + `present_main_window` /
  `minimize_main_window`：按根视图类型认窗口，启动/托盘唤出都显式抢一次前台）
- **L3**：`core/config.rs`（配置与持久化）、`core/state.rs`（五态状态机，5 单测）、
  `core/event.rs`（妙播 SDK 事件 → `UiEvent`，4 单测）、
  `core/app.rs`（`Core`：唯一业务状态源，拥有配置/状态机/通道/日志/播放信息，5 单测）
- **L4**：`channels/mod.rs` 的 `Channel` trait 由 `miplay::Receiver` 与 `remote_core::CoreReceiver`
  直接实现（不再有转发包装类型）；`dlna_player.rs` 为 DLNA 本地播放（流式拉流、Range seek、
  服务端忽略 Range 时整包缓存 / 本地跳字节补偿、重采样到设备采样率、cpal 输出、进度回填 DMR；每首曲目开头打一行质量诊断日志：控制点声明的质量 vs 实测编码/字节/时长/码率）；
  `remote_core::translate` 是核心事件词表的唯一翻译点

## Core 归属（状态在 Core，界面只有局部状态）

| 状态 | 归属 | 说明 |
|---|---|---|
| `settings` `status` `detail` `snapshot` `playing` `peer` `stage` `log` | `Core` | 业务状态，进程级 |
| `channel` `sink` `artwork_request_tx` `log_tx` `status_tx` `started_at` `tracks` `reconnects` | `Core` | 私有，只能通过方法改动 |
| `active_output` `start_minimized_pending` | `Core` | 由事件/配置派生；运行中的通道类型由 `active_source()` 从 `channel` + `settings.source` 算出，不额外存副本 |
| `route` `focused` `focus` `log_scroll` `audio_open` `audio_devices` | `SettingsView` | 纯界面状态，重开窗口重置 |

### 数据流

```
channels (L4) ──EventSink(flume, 有界, 传 UiEvent)──▶ Core 事件泵 ──apply(UiEvent)──▶ Core 状态
                                                                     │
                                     ┌───────────────────────────────┤
                                     ▼                               ▼
                         cx.notify() ──▶ SettingsView 重绘   StatusSummary ──▶ 托盘 tooltip

用户意图（窗口按钮 / 托盘菜单）──▶ Core 方法（start/stop/toggle_playback/set_*/type_key）──▶ channels
```

- 事件归一只发生在通道侧：妙播 SDK 的 `Value` 走 `core::event::parse_value`，
  DLNA 的核心事件走 `remote_core::translate`；管道里传 `UiEvent`，不再序列化成字符串。
- `Core` 只依赖 `Context<Core>`，不碰 `Window`，因此可以脱离窗口存活。
- 所有泵都持 `WeakEntity<Core>`：Core 释放后泵自然退出，不会泄漏。
- 全局只有一个 `Core`（`core::app::SharedCore`）；`SettingsView` 通过
  `cx.observe(&core, …)` 订阅刷新，因此窗口销毁、重建都不影响接收器与播放状态。
- 托盘只拿 `StatusSummary`（状态 + 曲目 + 播放/暂停）拼 tooltip，不持有业务状态；
  主线程每轮只取最后一条，避免进度事件刷爆托盘线程。

### 后台线程（主线程不做慢操作）

| 线程 | 归属 | 职责 |
|---|---|---|
| `myplay-tray` | `shell/tray.rs` | Windows 消息循环 + tooltip 刷新 + 单实例唤醒标记轮询 |
| `myplay-core` | `channels/remote_core.rs` | DLNA 核心的 tokio 运行时 |
| `myplay-dlna-player` | `channels/dlna_player.rs` | 命令调度、cpal 输出流 |
| `myplay-dlna-decode` | `channels/dlna_player.rs` | 拉流 + 解码，按「代次」响应 Stop / 换曲 / seek |
| `myplay-artwork` | `core/app.rs` | 串行下载封面（连续换曲只保留最后一个地址），解码在封面泵里做 |
| `myplay-log` | `core/app.rs` | 事件日志落盘（`platform::system::LogWriter` 常驻句柄） |
| `myplay-autostart` | `core/app.rs` | 开机自启写注册表 + 落盘；结果经独立泵回到 `Core`（界面不阻塞） |

音频回调（cpal 线程）不分配内存、不做字符串处理：采样缓冲按输出格式一次性预留，
满时由 `Condvar` 唤醒解码线程，暂停/空缓冲输出静音。

DLNA 停止的顺序是固定的：`Core::stop` → 播放器收到 `Stop`（立刻静音并回报
`STOPPED / ready=false`）→ 核心线程等这条回报落地（上限 300 ms）→ `service.stop()`
（SSDP 发 `byebye`、HTTP 监听关闭）→ 再留 80 ms 让 GENA 通知离场，最后丢弃运行时。
这样控制点在设备消失前就能收到「已停止」，不会因为运行时先死而收不到通知。

### 已知限制：控制点切走时不会通知渲染器

UPnP-AV 没有「控制点已离开」的通知或心跳，渲染器只对 `SetAVTransportURI / Play /
Pause / Stop / Seek` 负责。实测（手机投送代理）：控制点把播放目标切回本机时**不发任何请求**，
于是本机继续播到媒体结束。这无法在接收端可靠推断——手机息屏或 App 进后台时
`GetPositionInfo` 轮询也可能停止，用「轮询静默」做自动停止会误杀正常播放。
因此停止只能由控制点下达指令，或用户从托盘 / 窗口点「停止」。

`dlna_player.rs` 里为排查这类问题保留了控制点请求日志（`DLNA 控制点请求：…`，
只记会改变状态的动作用、不记轮询查询）以及每首曲目一行的质量诊断日志。

### 质量显示的数据来源

「信息」页那行 `编码 · 采样率 · 码率` 在两个通道里来源不同，排查「显示不对」时先看这里：

| 字段 | 妙播 | DLNA |
|---|---|---|
| 编码 | 协议常量 `AAC`（妙播固定 AAC-LC） | symphonia 探测出的真实编码名；只有查不到时才退回 mime |
| 采样率 | 协议常量 48000（SDK 对每个 ADTS 头强校验必须 48 kHz） | symphonia 实际 probe 出的解码参数 |
| 码率 | 首帧标称值（`adts_bitrate`）→ 之后每 2 秒被**实测滑窗吞吐**刷新 | 总字节 × 8 ÷ 解码时长（文件级平均） |

妙播的实测码率与回送给手机的 `latency_feedback` 是同一个数（`MiPlayLatencyTelemetry` 的 2 秒
滑窗 RTP 吞吐）；DLNA 的每个字段都能在 `DLNA 质量诊断` 那行日志里对照核对。

### 妙播的歌词/画面对齐

手机端的歌词高亮按「发送方时间线 + 它认为的接收端时延」走，因此对齐取决于两件事：

1. **我们是否贴着发送方时间线在播**——SDK 用发送方的 timer-server 时钟做调度
   （`presentation_schedule_error_micros`），正误差补静音、负误差超阈值丢帧、再用 ≤0.25% 的
   速率微调，稳态应稳定在 ±2 ms（`MIPLAY_SYNC_DEADBAND_MILLIS`）。
2. **我们上报的时延是否等于实际时延**——每 250 ms 发一次 `VIDEO_LATENCY`，
   报的是**输出设备延迟**（而不是「队列 + 输出」）：播放已钉在 PTS 上，队列深度只是发送方的
   提前量，不让声音变晚。曾把「队列 + 输出」约 830 ms 当延迟上报，手机据此推进歌词，
   结果歌词比声音慢近一秒；队列目标本身仍由 `MIPLAY_GROUP_PLAY_DELAY_MILLIS`(800) 与
   `MIPLAY_MINIMUM_QUEUE_DELAY_MILLIS`(650) 决定，属于多房间齐奏的缓冲设计，
   局域网下不需要，但改动它会影响 sender 的提前量，非必要不动。
3. **手机端的歌词进度不由接收端出声时刻驱动**——实测把整体出声时刻在 ±200 ms 之间来回平移，
   歌词与声音的相对偏差没有可辨变化：手机是按自己的播放位置与滚动动画推进歌词的。
   因此代码里不保留任何「播放延迟补偿」旋钮，出声时刻严格贴发送方时间线；
   剩余差异属于手机端，接收端没有可用的调节手段。

判读方法：调度误差长期偏离 0 说明时序没贴住（可以临时在 `AacPlayer` 回调旁观察
`presentation_schedule_error_micros`）；上报时延偏离实际输出延迟说明回报口径错了。
两者都正常而歌词仍偏，则偏差在手机端的固定补偿或歌词滚动动画上，接收端无法干预。

## 本地验证（需人工执行）

```powershell
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p myplay   # 39 单测：state 5 / event 4 / app 5 / config 2 / dlna_player 16 / remote_core 3 / miplay 1 / audio 1 / system 1 / single_instance 1
cargo run -p myplay
```

手工回归清单：

- 关窗后进程常驻、音乐继续；托盘重开窗口状态不丢（tab 与焦点会回到默认，属界面局部状态）
- 开「启动后最小化到托盘」后重启：只有托盘图标、不出现窗口（首帧也不该闪）；点托盘左键才建窗并在前台
- 重复启动 exe：第二个进程直接退出，已在跑的实例把窗口叫到前台（关着窗口也会重新打开），不会出现两个托盘图标
- 两个通道各启动/停止/切换一次无回归
- 启停状态还原：启动后退出 → 再开仍自动运行；停止后退出 → 再开仍为停止（参数不变）
- 托盘「启动/停止/播放暂停」与窗口按钮行为一致；tooltip 随曲目与状态变化
- 日志页滚动正常、条数上限 1000；改「日志文件」后新行落到新路径
- 文本字段逐键输入不再卡顿，回车/失焦/关窗后改动落到 `settings.json`
