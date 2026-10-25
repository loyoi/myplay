//! 窗口生命周期：窗口是「按需」的渲染载体，关闭即销毁；视图实体常驻在全局里。
//!
//! 数据流：托盘/启动流程调用 [`open_main_window`] → 从 [`SharedView`] 取出常驻视图 →
//! 绑定窗口级行为（关闭策略）→ 作为窗口根视图返回。视图本身只持有界面局部状态，
//! 业务状态在 `core::app::Core`（全局 [`crate::core::app::SharedCore`]）。

use std::time::Duration;

use gpui_kit::*;

use crate::ui::SettingsView;

/// 产品标题：窗口标题与托盘 tooltip 共用这一份文案。
pub const APP_TITLE: &str = "myplay · 小米妙播接收器";

/// 窗口初始尺寸。
pub const WINDOW_SIZE: Size<Pixels> = Size {
    width: px(860.),
    height: px(560.),
};

/// 全局持有视图实体：窗口销毁后它仍然存活（界面局部状态，如当前 Tab）。
pub struct SharedView(pub Entity<SettingsView>);

impl Global for SharedView {}

fn window_options(bounds: WindowBounds) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(bounds),
        window_min_size: Some(Size {
            width: px(860.),
            height: px(560.),
        }),
        titlebar: Some(TitlebarOptions {
            title: Some(APP_TITLE.into()),
            ..Default::default()
        }),
        ..WindowOptions::default()
    }
}

/// 打开主窗口；窗口内容就是全局里那个视图实体，因此状态天然复用。
///
/// 主窗口是单例：已经开着就什么都不做（要它露脸再调 [`present_main_window`]）。
/// 所有入口（启动流程、托盘左键、托盘菜单「显示窗口」）都走这里，单例判断只有这一处，
/// 托盘连点也不会重复建窗。
pub fn open_main_window(cx: &mut App) -> anyhow::Result<()> {
    if main_window(cx).is_some() {
        return Ok(());
    }
    let bounds = WindowBounds::centered(WINDOW_SIZE, cx);
    cx.open_window(window_options(bounds), |window, cx| {
        let view = cx.global::<SharedView>().0.clone();
        view.update(cx, |view, cx| view.attach_window(window, cx));
        view
    })?;
    log::info!("主窗口已创建");
    Ok(())
}

/// 确认主窗口在前台需要等待的时间：首帧渲染与托盘图标上线之后，系统才可能把窗口压回后面。
const FOREGROUND_VERIFY_DELAY: Duration = Duration::from_millis(250);

/// 把主窗口切到前台（最小化状态会被还原）。
///
/// 托盘唤出、重复启动唤醒都走这里：一次就够，那一刻手里有用户的真实输入。
pub fn present_main_window(cx: &mut App) {
    grab_foreground(cx);
}

/// 启动后把主窗口放到前台。
///
/// 窗口创建时系统已经给过一次激活，但首帧渲染 / 托盘图标上线之后可能把它压回后面。
/// 所以先抢一次，隔一拍再确认一次；两次都不成就算了，不继续跟用户抢焦点。
pub fn ensure_main_window_front(cx: &mut App) {
    grab_foreground(cx);
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(FOREGROUND_VERIFY_DELAY)
            .await;
        cx.update(grab_foreground);
    })
    .detach();
}

/// 抢一次前台：先用 Win32（借前台线程的输入权限），拿不到再退回 GPUI 自己的激活。
///
/// 已经在最前时前两个调用都是空操作，所以正常路径不会产生日志。
fn grab_foreground(cx: &mut App) {
    if crate::platform::system::force_foreground(APP_TITLE) {
        return;
    }
    if let Some(handle) = main_window(cx)
        && let Err(error) = handle.update(cx, |_, window, _| window.activate_window())
    {
        log::warn!("抢前台：GPUI 激活失败（{error:#}）");
    }
}

/// 主窗口句柄：根视图类型就是 [`SettingsView`] 的那个窗口。
///
/// 按根视图类型认窗口（不是按 `App::active_window`）：窗口没被激活时后者可能为空，
/// 而主窗口的根视图永远只有这一个类型。
fn main_window(cx: &App) -> Option<WindowHandle<SettingsView>> {
    cx.windows()
        .into_iter()
        .find_map(|handle| handle.downcast::<SettingsView>())
}
