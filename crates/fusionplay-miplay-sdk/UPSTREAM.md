# 上游来源

本目录是小米妙播（MiPlay）接收器 SDK 的内联源码副本，随本仓库一起维护。

| 项 | 值 |
|---|---|
| 上游仓库 | <https://github.com/rosienosiesie/FusionPlay-Android> |
| 内联提交 | `b43fe1130c3fdb233dcb41c2da174b858eb44cc2`（FusionPlay 1.2.4） |
| 上游路径 | `src/FusionPlay.MiPlaySdk` |
| crate | `fusionplay-miplay-sdk`（lib 名 `fusionplay_miplay_sdk`） |
| 许可 | `AGPL-3.0-only` |

内联之后本仓库是这份源码的唯一维护方：上游的后续提交不会自动进入这里，改动也只在本仓库生效。
需要与上游核对内容时，请按上表的提交号去上游仓库比对。

## 内联时做的改动

- 依赖版本上移到工作区根 `[workspace.dependencies]`，本清单只写 `workspace = true` + feature。
- path 依赖 `if-addrs` 指向工作区内的 `crates/fusionplay-if-addrs`。
- `cpal` 从 0.17 升到 0.18（与 `crates/myplay` 统一到同一套音频栈），`src/media.rs` 随之适配：
  `build_output_stream` 收值参 `StreamConfig`、错误类型统一为 `cpal::Error`、
  `StreamInstant::duration_since` 取值参且不再返回 `Option`。
- 其余依赖升到全仓最新版后的 API 适配：
  - `rand 0.10`：`rand::thread_rng()` → `rand::rng()`，`RngCore::fill_bytes` → `rand::fill`。
  - `hmac 0.13`：`Hmac::new_from_slice` 改由 `hmac::KeyInit` 提供（补 trait 导入）。
  - `p256 0.14`：`EncodedPoint` → `Sec1Point`；`EphemeralSecret::random(&mut OsRng)`（已废弃）
    → `EphemeralSecret::generate_from_rng(&mut rand::rng())`。
  - `aes-gcm 0.11`：`Nonce::from_slice`（已废弃）→ `Nonce::from([u8; 12])`。
- `[lints.clippy] too_many_arguments = "allow"`：RTSP/RTP 装配函数天然宽参数（流、密钥、
  时钟、统计各一路），拆分只增加封装成本。
- `media.rs` 的 `chunks_exact(N)` 改为 `as_chunks::<N>()`，`clippy -D warnings` 门禁要求。

## 未内联的上游文件

`Cargo.lock`、`target/` 属于构建产物；版本解析由工作区根 `Cargo.lock` 统一负责。
