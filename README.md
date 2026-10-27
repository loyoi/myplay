# myplay

小米妙播 / DLNA 局域网音频接收器（Rust + GPUI，托盘常驻、窗口按需）。

## 快速开始

```powershell
cargo run -p myplay   # 在仓库根直接 `cargo run` 等价（根是虚拟工作区，默认成员只有应用）
```

| 操作 | 说明 |
|---|---|
| 设置 → 启动源 | 小米妙播（AAC 256k）/ DLNA（FLAC/WAV 可无损） |
| 设置 → 输出设备 | 下拉选择系统输出设备（切换后自动重启接收器） |
| 关闭窗口 | 窗口销毁，进程驻留托盘；点托盘图标重新打开，状态不丢 |
| 启动状态 | 启停按钮的状态会记住：上次关软件时接收器开着，下次启动自动回到运行（参数不变） |
| 托盘图标 | 左键打开窗口（已在就激活，不会重复开窗）；右键出菜单 |
| 启动后最小化 | 开启时启动不显示窗口，只留托盘图标；点托盘左键再打开界面 |
| 重复启动 | 已有进程在跑时再次启动只把那个实例的窗口叫出来，不会再起第二个进程 |
| 侧边栏底部 | 版本号旁的 GitHub 图标：点击用系统浏览器打开项目仓库 |

## 架构

见 [docs/architecture.md](docs/architecture.md)：四层（ui / shell / core / channels），
`core::app::Core` 是唯一业务状态源（窗口关闭、重开都不影响接收器）；界面只保留
Tab / 焦点 / 下拉等局部状态，托盘 tooltip 由 Core 回传的状态刷新。

## 工作区

仓库是 Cargo 工作区：`crates/myplay` 是唯一应用，其余三份接收器源码内联在 `crates/` 里由本仓库
自行维护，不再引用仓库外的 path。根 `Cargo.toml` 只做成员编排、profile 决策与**依赖版本声明**
（`[workspace.dependencies]` 是全仓库唯一的版本来源，成员清单只写 `workspace = true` + feature），
`Cargo.lock` 由根目录唯一持有。音频栈全仓库一套：`cpal 0.18` + `symphonia 0.6.1`。

```text
Cargo.toml                 # [workspace]：members / default-members / profile
crates/myplay/             # 桌面应用（ui / shell / core / channels / platform），唯一 bin
crates/fusionplay-miplay-sdk/   # 小米妙播协议与媒体接收
crates/airplay-receiver-core/   # DLNA DMR 核心（lib 名 fusionplay_core；AirPlay 通道已移除）
crates/fusionplay-if-addrs/     # if-addrs 适配层（crate 名仍为 if-addrs）
```

```powershell
cargo check --all-targets                       # 只校验应用（默认成员）
cargo check --workspace --all-targets           # 连三份内置源码一起校验
cargo clippy --workspace --all-targets -- -D warnings
```

## 发布

版本号只有一处来源：`crates/myplay/Cargo.toml` 的 `[package] version`；更新记录写在
[CHANGELOG.md](CHANGELOG.md)。两者都改好后，在仓库根执行一条命令：

```powershell
just release
```

脚本先校验：在 `master` 分支、`gh` 已登录、`CHANGELOG.md` 里有 `## <版本号>` 段落且非空、
`v<版本号>` tag 本地与远端都不存在、GitHub 上没有同名 Release。通过后打印版本、tag、产物与
更新内容摘要，**回车确认**才开始：release 构建 → 打包
`target/dist/myplay-<版本>-windows-x64.zip` → 提交 `Cargo.toml` / `Cargo.lock` / `CHANGELOG.md` →
打 tag 并推送到 `github` remote → `gh release create` 上传产物并打印 Release 地址。
工作区里其它未提交改动不会被提交。

## 内置源码与上游来源

三份源码都来自 FusionPlay-Android 的同一次提交，内联后由本仓库维护；上游后续提交不会自动
进入这里。每份目录内的 `UPSTREAM.md` 记录了内联时的具体改动。

| 目录 | 上游仓库 | 内联提交 | 上游路径 | 许可 |
|---|---|---|---|---|
| `crates/fusionplay-miplay-sdk` | [FusionPlay-Android](https://github.com/rosienosiesie/FusionPlay-Android) | `b43fe11` | `src/FusionPlay.MiPlaySdk` | AGPL-3.0-only |
| `crates/airplay-receiver-core` | 同上 | `b43fe11` | `src/AirPlayReceiver.Core` | AGPL-3.0-only |
| `crates/fusionplay-if-addrs` | 同上 | `b43fe11` | `src/FusionPlay.AndroidIfAddrs` | AGPL-3.0-only |

许可证与分发边界见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
