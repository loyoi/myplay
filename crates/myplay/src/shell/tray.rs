//! 系统托盘。
//!
//! 数据流：托盘在自己的线程里跑 Windows 消息循环（tray-icon 要求），菜单/图标事件
//! 经 flume 通道送到 GPUI 侧的任务；托盘线程不直接操作窗口，避免跨线程依赖 GPUI。
//! 反方向：GPUI 侧的 [`Core`](crate::core::app::Core) 把状态压成
//! [`StatusSummary`] 回传，托盘只拿它刷新 tooltip，不持有任何业务状态。
//! 另一条输入：每轮顺手看一眼单实例的 [`WakeSignal`]（重复启动留下的标记），
//! 有就直接当成「显示窗口」。

use std::time::Duration;

use anyhow::{Context as _, Result};
use tray_icon::TrayIconBuilder;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconEvent};

use crate::core::app::StatusSummary;
use crate::platform::single_instance::WakeSignal;
use crate::shell::window::APP_TITLE;

/// 托盘发出的命令。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayCommand {
    /// 显示/激活窗口。
    ShowWindow,
    /// 启动接收器。
    Start,
    /// 停止接收器。
    Stop,
    /// 播放/暂停。
    TogglePlayback,
    /// 退出程序。
    Quit,
}

/// 启动托盘线程。
///
/// `commands` 是托盘 → GPUI 的意图通道；`status` 是 GPUI → 托盘的显示回传通道；
/// `wake` 是「另一个实例请求显示窗口」的轮询点。
pub fn spawn(
    commands: flume::Sender<TrayCommand>,
    status: flume::Receiver<StatusSummary>,
    wake: WakeSignal,
) -> Result<()> {
    std::thread::Builder::new()
        .name("myplay-tray".to_owned())
        .spawn(move || {
            if let Err(error) = run(commands, status, wake) {
                log::error!("托盘初始化失败：{error:#}");
            }
        })
        .context("无法创建托盘线程")?;
    Ok(())
}

fn run(
    commands: flume::Sender<TrayCommand>,
    status: flume::Receiver<StatusSummary>,
    wake: WakeSignal,
) -> Result<()> {
    let show_item = MenuItem::new("显示窗口", true, None);
    let start_item = MenuItem::new("启动接收器", true, None);
    let stop_item = MenuItem::new("停止接收器", true, None);
    let play_item = MenuItem::new("播放 / 暂停", true, None);
    let quit_item = MenuItem::new("退出", true, None);

    let menu = Menu::new();
    menu.append_items(&[
        &show_item,
        &PredefinedMenuItem::separator(),
        &start_item,
        &stop_item,
        &play_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ])
    .context("无法构建托盘菜单")?;

    let (show_id, start_id, stop_id, play_id, quit_id) = (
        show_item.id().clone(),
        start_item.id().clone(),
        stop_item.id().clone(),
        play_item.id().clone(),
        quit_item.id().clone(),
    );

    // 菜单点击 → 命令。
    {
        let commands = commands.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let command = if event.id == show_id {
                TrayCommand::ShowWindow
            } else if event.id == start_id {
                TrayCommand::Start
            } else if event.id == stop_id {
                TrayCommand::Stop
            } else if event.id == play_id {
                TrayCommand::TogglePlayback
            } else if event.id == quit_id {
                TrayCommand::Quit
            } else {
                return;
            };
            let _ = commands.send(command);
        }));
    }

    // 左键单击 → 显示窗口。
    {
        let commands = commands.clone();
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let _ = commands.send(TrayCommand::ShowWindow);
            }
        }));
    }

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        // 左键只负责开窗口：Windows 上默认左右键都会弹菜单，这里把菜单留给右键。
        .with_menu_on_left_click(false)
        .with_tooltip(APP_TITLE)
        .with_icon(Icon::from_rgba(build_icon(), 32, 32).context("托盘图标无效")?)
        .build()
        .context("创建托盘图标失败")?;

    log::info!("托盘已就绪");
    pump_messages(&tray, &status, &wake, &commands);
    Ok(())
}

/// 托盘图标：32×32 圆角方块 + 中间一枚「播放三角」，纯代码生成，免外部资源文件。
fn build_icon() -> Vec<u8> {
    const SIZE: usize = 32;
    let mut rgba = vec![0_u8; SIZE * SIZE * 4];
    let center = (SIZE as f32 - 1.0) / 2.0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let index = (y * SIZE + x) * 4;
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let inside = dx.abs() < 14.0 && dy.abs() < 14.0;
            if !inside {
                continue;
            }
            // 圆角：四角按半径裁剪。
            let corner = (dx.abs() > 11.0 && dy.abs() > 11.0)
                && (dx.abs() - 11.0).hypot(dy.abs() - 11.0) > 3.0;
            if corner {
                continue;
            }
            let (r, g, b) = (0x2f, 0x6f, 0xed);
            rgba[index] = r;
            rgba[index + 1] = g;
            rgba[index + 2] = b;
            rgba[index + 3] = 0xff;

            // 播放三角：以中心为基准的等腰三角。
            let tri = dx > -4.0 && dx < 6.0 && dy.abs() < (6.0 - (dx + 4.0) * 0.75).max(0.0);
            if tri {
                rgba[index] = 0xff;
                rgba[index + 1] = 0xff;
                rgba[index + 2] = 0xff;
            }
        }
    }
    rgba
}

/// Windows 消息循环：tray-icon / muda 的事件依赖它派发。
///
/// 用 `PeekMessageW` 轮询而不是 `GetMessageW` 阻塞：每轮顺手把最新状态
/// 写进 tooltip、看一眼单实例的唤醒标记，没有消息时睡一小会儿（100 ms，几乎不占 CPU）。
fn pump_messages(
    tray: &TrayIcon,
    status: &flume::Receiver<StatusSummary>,
    wake: &WakeSignal,
    commands: &flume::Sender<TrayCommand>,
) {
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage, WM_QUIT,
    };

    let mut message = MSG::default();
    loop {
        // 先把消息队列掏空。
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            // WM_QUIT 表示进程要退出。
            if message.message == WM_QUIT {
                return;
            }
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }

        // 队列空了：刷新 tooltip（只取最后一条就行，中间的直接丢弃）。
        let mut latest = None;
        while let Ok(summary) = status.try_recv() {
            latest = Some(summary);
        }
        if let Some(summary) = latest
            && let Err(error) = tray.set_tooltip(Some(summary.tooltip()))
        {
            log::warn!("托盘 tooltip 更新失败：{error:#}");
        }

        // 另一个实例刚启动过：它留下的标记就是「把窗口叫出来」。
        if wake.take() {
            let _ = commands.send(TrayCommand::ShowWindow);
        }

        std::thread::sleep(Duration::from_millis(100));
    }
}
