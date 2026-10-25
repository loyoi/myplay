# 第三方组件与许可证说明

`crates/` 下的三份源码是 FusionPlay-Android 的内联副本（来源与提交见
[README](README.md#内置源码与上游来源)，逐包改动见各自的 `UPSTREAM.md`）。
本文件只记录分发边界，不构成法律意见。

## myplay 自身的许可状态

`crates/myplay` 的原创代码当前没有单独的 `LICENSE` 文件；但三份内置源码均为
**AGPL-3.0-only**（`fusionplay-miplay-sdk`、`airplay-receiver-core`、`fusionplay-if-addrs`），
因此整个产物对外分发时必须按 AGPL-3.0-only 处理：提供对应源码，且不得附加额外限制。
AGPL 正文可在 <https://www.gnu.org/licenses/agpl-3.0.txt> 取得。

## Rust 依赖

Rust 依赖主要采用 MIT、Apache-2.0、BSD、MPL-2.0、Zlib 或这些许可证的多重组合；
没有许可证字段为空的依赖。精确版本以根目录 `Cargo.lock` 为准，逐包许可见各 crate 的
`license` 元数据与包内 `LICENSE`。

## 已移除的组件

AirPlay（RAOP）通道连同内置的 `shairplay 0.7.0` 源码副本一起移除：FusionPlay 侧的
DACP / MediaRemote 补丁没有随 crates.io 版本发布，因此这些代码不再参与构建，也不再随本仓库分发。

## 协议名称

小米妙播接收逻辑由 `crates/fusionplay-miplay-sdk` 的 Rust 源码实现，不打包或启动
MiPCAudio、MAFSvr、MiConnect、MiCont、小米电脑管家或厂商账号服务。源码中的协议名称、
服务标识与兼容性测试向量仅用于网络互操作。
