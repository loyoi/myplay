# 上游来源

本目录是 FusionPlay 的 `if-addrs` 适配层（面向 Android 5 的网卡枚举 API）的内联源码副本，
随本仓库一起维护。crate 名仍是 `if-addrs`、lib 名仍是 `if_addrs`，以便沿用上游调用点。

| 项 | 值 |
|---|---|
| 上游仓库 | <https://github.com/rosienosiesie/FusionPlay-Android> |
| 内联提交 | `b43fe1130c3fdb233dcb41c2da174b858eb44cc2`（FusionPlay 1.2.4） |
| 上游路径 | `src/FusionPlay.AndroidIfAddrs` |
| crate | `if-addrs` 0.15.0（lib 名 `if_addrs`） |
| 许可 | `AGPL-3.0-only` |
| 后端 | `getifs 0.6.1` |

## 与 crates.io 同名 crate 的关系

工作区里同时存在两个 `if-addrs 0.15.0`：本目录的 path 版本（`myplay` 与两份接收器核心使用）
和 crates.io 版本（`shairplay → mdns-sd` 使用）。两者源不同、互不替换，锁文件里各自成条目；
这份适配层不接管 mDNS 的网卡枚举。

## 内联时做的改动

没有源码改动：只把引用它的 manifest 从 `../FusionPlay.AndroidIfAddrs` 改为
`../fusionplay-if-addrs`。
