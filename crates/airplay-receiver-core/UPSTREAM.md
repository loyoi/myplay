# 上游来源

本目录是 FusionPlay 接收核心的内联源码副本，随本仓库一起维护。

| 项 | 值 |
|---|---|
| 上游仓库 | <https://github.com/rosienosiesie/FusionPlay-Android> |
| 内联提交 | `b43fe1130c3fdb233dcb41c2da174b858eb44cc2`（FusionPlay 1.2.4） |
| 上游路径 | `src/AirPlayReceiver.Core` |
| crate | `airplay-receiver-core`（lib 名 `fusionplay_core`） |
| 许可 | `AGPL-3.0-only` |

crate 名沿用上游（避免改动调用点的 `fusionplay_core::` 前缀），但内容已只剩 **DLNA DMR**。

## AirPlay 通道已移除

上游的 RAOP 实现建立在 `shairplay` 的 FusionPlay 分支之上（`LOCAL_PATCHES.md` 里的 DACP、
MediaRemote event channel、RTSP 分帧等十余项补丁）。crates.io 上的 `shairplay` 不含这些 API，
而 myplay 不打算自行维护那份 fork，因此整条 AirPlay 链路下线：

- 删除 `src/audio.rs`（RAOP 音频引擎与 `AudioHandler` 实现）、`src/pairing.rs`
  （`FilePairingStore`）、`src/video.rs`（HLS 视频桥）、`src/network_identity.rs`、
  `src/android_jni.rs`（Android 宿主桥）。
- 删除 `shairplay` 依赖，以及 `RemoteCommand` / `RemoteControl` / `HlsHandler` 相关的命令通道。
- `src/host.rs` 重写为只保留 `Settings { name, state_path }`、`PersistedState`（DLNA 设备 MAC）
  与 `start_dlna`；`run_receiver` / `run_receiver_with_arbiter` / `desktop_main` /
  `parse_settings_from` 随 AirPlay 一起删除。
- `lib.rs` 只导出 `DmrService` / `DmrController` / `CoreEvent` / `EventSink` / `Settings` / `start_dlna`。
- `Cargo.toml` 的依赖收缩到 DLNA 实际使用的部分
  （`anyhow` / `httparse` / `if-addrs` / `rand` / `serde` / `serde_json` / `socket2` / `tokio` / `tracing`）。

## 其他内联改动

- 依赖版本上移到工作区根 `[workspace.dependencies]`，本清单只写 `workspace = true` + feature；
  内部 path 依赖（`fusionplay-miplay-sdk`、`if-addrs`）的路径只在根表声明。
- 依赖升级适配：`rand 0.10`（`rand::thread_rng()` → `rand::rng()` / `rand::fill`）、
  `tokio` / `serde` 按根表版本。
- 移除 `[[bin]]` 两个可执行目标（`airplay-receiver-core`、`miplay-host`）及 `src/main.rs`、
  `src/bin/`：桌面宿主由 `crates/myplay` 提供。
- 移除 `[patch.crates-io]` 与 `[profile.release]`：前者只能在根清单生效，后者由根统一决策。
- `build.rs` 精简为只保留 Android 的 `c++_shared` 链接分支（上游的 VERSIONINFO 资源段随 bin 删除）。

## 未内联的上游文件

- `.cargo/config.toml`（写死 `target = "x86_64-pc-windows-gnullvm"`）与 `rust-toolchain.toml`
  （锁 gnullvm 工具链）：留在工作区里会劫持整个仓库的构建目标与工具链。
- `Cargo.lock`、`target/`、`miplay-state/`：构建产物与运行期状态。
