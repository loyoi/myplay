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
  cpal 输出、进度回填 DMR）；`remote_core::translate` 是核心事件词表的唯一翻译点

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

音频回调（cpal 线程）不分配内存、不做字符串处理：采样缓冲按输出格式一次性预留，
满时由 `Condvar` 唤醒解码线程，暂停/空缓冲输出静音。

DLNA 停止的顺序是固定的：`Core::stop` → 播放器收到 `Stop`（立刻静音并回报
`STOPPED / ready=false`）→ 核心线程等这条回报落地（上限 300 ms）→ `service.stop()`
（SSDP 发 `byebye`、HTTP 监听关闭）→ 再留 80 ms 让 GENA 通知离场，最后丢弃运行时。
这样控制点在设备消失前就能收到「已停止」，不会因为运行时先死而收不到通知。

## 本地验证（需人工执行）

```powershell
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p myplay   # 25 单测：state 5 / event 4 / app 5 / config 2 / dlna_player 2 / remote_core 3 / miplay 1 / audio 1 / system 1 / single_instance 1
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
