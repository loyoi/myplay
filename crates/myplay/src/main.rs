//! myplay 入口：Core（唯一业务状态源）+ GPUI 窗口 + 系统托盘。
//!
//! 生命周期设计：启动时先创建 [`Core`]（配置、状态机、通道、日志都在里面），再创建视图
//! （只有界面局部状态）。托盘命令直接打到 Core，因此窗口开开关关、甚至从不打开窗口，
//! 接收器都照常工作；重开窗口时视图读取的还是同一个 Core。
//!
//! 单实例：`main` 先抢数据目录里的文件锁，抢不到说明已有进程在跑——写下唤醒标记让对方
//! 把窗口显示出来，本进程直接退出（见 `platform::single_instance`）。

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod channels;
mod core;
mod platform;
mod shell;
mod ui;

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::*;

use crate::core::app::{Core, SharedCore, StatusSummary};
use crate::core::config::Settings;
use crate::shell::tray::TrayCommand;
use crate::shell::window::{
    SharedView, ensure_main_window_front, open_main_window, present_main_window,
};
use crate::ui::SettingsView;

/// 从全局取核心实体：托盘命令与窗口都不缓存副本，一律以全局为准。
fn shared_core(cx: &App) -> Entity<Core> {
    cx.global::<SharedCore>().0.clone()
}

/// 托盘命令 → Core 方法：命令与业务方法一一对应，不在这里复制业务逻辑。
fn run_core(cx: &mut App, action: fn(&mut Core, &mut Context<Core>)) {
    shared_core(cx).update(cx, action);
}

/// 把组件库主题对到应用自己的深色配色。
///
/// 日志页用的是组件库的只读文本框：它的文字、选中、光标颜色由组件主题决定
/// （默认是浅色主题，会在深色面板里变成一块白底深字），所以在窗口建立前先切深色并对齐取值。
fn apply_component_theme(cx: &mut App) {
    Theme::change(ThemeMode::Dark, None, cx);
    Theme::update(cx, |theme| {
        theme.background = rgb(0x16171b).into();
        theme.foreground = rgb(0xa8aebb).into();
        theme.muted_foreground = rgb(0x6f7683).into();
        theme.border = rgb(0x33363d).into();
        // 选中底色带透明度，压住文字而不是盖掉它。
        theme.selection = rgba(0x2f6fed59).into();
        theme.caret = rgb(0x2f6fed).into();
    });
}

fn main() {
    // 日志最先就位：单实例判定、配置读取与启动失败都要能落到控制台/日志文件。
    // 级别锁定 info：不读 `RUST_LOG`，也不受设置页「详细日志」影响，
    // 免得环境变量或历史配置把 release 进程刷成 debug 输出。
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .init();

    // 单实例：已经有进程在跑就把它的窗口叫出来，然后退出——两个进程会抢同一个
    // 接收端口和同一个托盘图标，而且配置写盘也会互相覆盖。
    let Some(instance) = platform::single_instance::claim() else {
        platform::single_instance::signal_existing();
        return;
    };

    let wake = instance.wake_signal();
    // GPUI 的启动闭包只跑一次（跑完就回主循环），所以锁不能交给闭包里的局部变量：
    // 这里直接不 drop，锁一直活到进程退出（进程崩了由系统释放）。
    std::mem::forget(instance);

    // 配置只读一次，之后由 Core 持有唯一副本。
    let settings = Settings::load();
    // 环境变量必须在任何线程启动之前设置（`set_var` 在多线程下不安全）。
    settings.apply_environment();

    gpui_kit::application()
        // 内嵌默认图标集（侧边栏的 GitHub 图标等 SVG 全从这里取）。
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            apply_component_theme(cx);
            // 窗口全部关闭也不退出进程：后台由托盘驻留。
            cx.set_quit_mode(QuitMode::Explicit);

            // 先 Core（唯一真相源），再视图（窗口销毁它也不受影响）。
            let (status_tx, status_rx) = flume::unbounded::<StatusSummary>();
            let core = cx.new(|cx| Core::new(settings.clone(), status_tx, cx));
            cx.set_global(SharedCore(core.clone()));

            let view = cx.new(|cx| SettingsView::new(core.clone(), cx));
            cx.set_global(SharedView(view));

            let (tray_tx, tray_rx) = flume::unbounded::<TrayCommand>();
            if let Err(error) = shell::tray::spawn(tray_tx, status_rx, wake) {
                log::error!("{error:#}");
            }

            // 上次关软件时接收器是开着的：参数原样，直接把接收器拉起来（等同再点一次启动）。
            // 开机自启时网卡往往还没就绪，这条路径失败会自动重试（见 `Core::restore_start`）。
            if core.read(cx).settings.restore_running {
                core.update(cx, |core, cx| core.restore_start(cx));
            }

            // 「启动后最小化到托盘」= 本次启动**不建窗口**：只留托盘图标。
            // 建了再最小化会让窗口先闪一下再消失，看着像窗口自己跳到后面去了。
            // 需要界面时点托盘左键，按需建窗并抢前台。
            if core.update(cx, |core, _| core.take_start_minimized()) {
                log::info!("启动即最小化：不打开主窗口，驻留托盘");
            } else {
                if let Err(error) = open_main_window(cx) {
                    log::error!("无法打开窗口：{error:#}");
                }
                ensure_main_window_front(cx);
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
                                present_main_window(cx);
                            });
                        }
                        TrayCommand::Start => cx.update(|cx| run_core(cx, Core::start)),
                        TrayCommand::Stop => cx.update(|cx| run_core(cx, Core::stop)),
                        TrayCommand::TogglePlayback => {
                            cx.update(|cx| run_core(cx, Core::toggle_playback));
                        }
                        TrayCommand::Quit => {
                            cx.update(|cx| {
                                // 退出只停通道（发 byebye），不改写「下次启动还原成运行」。
                                run_core(cx, Core::shutdown);
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
