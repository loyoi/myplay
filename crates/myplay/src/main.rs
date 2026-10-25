//! myplay 入口：Core（唯一业务状态源）+ GPUI 窗口 + 系统托盘。
//!
//! 生命周期设计：启动时先创建 [`Core`]（配置、状态机、通道、日志都在里面），再创建视图
//! （只有界面局部状态）。托盘命令直接打到 Core，因此窗口开开关关、甚至从不打开窗口，
//! 接收器都照常工作；重开窗口时视图读取的还是同一个 Core。

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod channels;
mod core;
mod platform;
mod shell;
mod ui;

use gpui_kit::*;

use crate::core::app::{Core, SharedCore, StatusSummary};
use crate::core::config::Settings;
use crate::shell::tray::TrayCommand;
use crate::shell::window::{SharedView, open_main_window};
use crate::ui::SettingsView;

/// 从全局取核心实体：托盘命令与窗口都不缓存副本，一律以全局为准。
fn shared_core(cx: &App) -> Entity<Core> {
    cx.global::<SharedCore>().0.clone()
}

/// 托盘命令 → Core 方法：命令与业务方法一一对应，不在这里复制业务逻辑。
fn run_core(cx: &mut App, action: fn(&mut Core, &mut Context<Core>)) {
    shared_core(cx).update(cx, action);
}

fn main() {
    // 配置只读一次，之后由 Core 持有唯一副本。
    let settings = Settings::load();
    // 日志级别锁定 info：不读 `RUST_LOG`，也不受设置页「详细日志」影响，
    // 免得环境变量或历史配置把 release 进程刷成 debug 输出。
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .init();
    // 环境变量必须在任何线程启动之前设置（`set_var` 在多线程下不安全）。
    settings.apply_environment();

    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        // 窗口全部关闭也不退出进程：后台由托盘驻留。
        cx.set_quit_mode(QuitMode::Explicit);

        // 先 Core（唯一真相源），再视图（窗口销毁它也不受影响）。
        let (status_tx, status_rx) = flume::unbounded::<StatusSummary>();
        let core = cx.new(|cx| Core::new(settings.clone(), status_tx, cx));
        cx.set_global(SharedCore(core.clone()));

        let view = cx.new(|cx| SettingsView::new(core.clone(), cx));
        cx.set_global(SharedView(view));

        let (tray_tx, tray_rx) = flume::unbounded::<TrayCommand>();
        if let Err(error) = shell::tray::spawn(tray_tx, status_rx) {
            log::error!("{error:#}");
        }

        if let Err(error) = open_main_window(cx) {
            log::error!("无法打开窗口：{error:#}");
        }

        let minimised = core.update(cx, |core, _| core.take_start_minimized());
        if minimised && let Some(any) = cx.active_window() {
            let _ = any.update(cx, |_, window, _| window.minimize_window());
        }

        cx.spawn(async move |cx| {
            while let Ok(command) = tray_rx.recv_async().await {
                match command {
                    TrayCommand::ShowWindow => {
                        cx.update(|cx| {
                            // 单例判断在 `open_main_window` 里：窗口已在就只激活它。
                            if let Err(error) = open_main_window(cx) {
                                log::error!("无法显示窗口：{error:#}");
                            }
                        });
                    }
                    TrayCommand::Start => cx.update(|cx| run_core(cx, Core::start)),
                    TrayCommand::Stop => cx.update(|cx| run_core(cx, Core::stop)),
                    TrayCommand::TogglePlayback => {
                        cx.update(|cx| run_core(cx, Core::toggle_playback));
                    }
                    TrayCommand::Quit => {
                        cx.update(|cx| {
                            run_core(cx, Core::stop);
                            cx.quit();
                        });
                        break;
                    }
                }
            }
        })
        .detach();
    });
}
