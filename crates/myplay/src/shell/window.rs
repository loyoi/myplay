//! 窗口生命周期：窗口是「按需」的渲染载体，关闭即销毁；视图实体常驻在全局里。
//!
//! 数据流：托盘/启动流程调用 [`open_main_window`] → 从 [`SharedView`] 取出常驻视图 →
//! 绑定窗口级行为（关闭策略）→ 作为窗口根视图返回。视图本身只持有界面局部状态，
//! 业务状态在 `core::app::Core`（全局 [`crate::core::app::SharedCore`]）。

use gpui_kit::*;

use crate::ui::SettingsView;

/// 产品标题：窗口标题与托盘 tooltip 共用这一份文案。
pub const APP_TITLE: &str = "myplay · 小米妙播接收器";

/// 窗口初始尺寸。
pub const WINDOW_SIZE: Size<Pixels> = Size {
    width: px(1000.),
    height: px(660.),
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
/// 主窗口是单例：已经开着就只把它拉到前台，不再开第二个。所有入口（启动流程、托盘左键、
/// 托盘菜单「显示窗口」）都走这里，单例判断只有这一处，托盘连点也不会重复建窗。
pub fn open_main_window(cx: &mut App) -> anyhow::Result<()> {
    if activate_main_window(cx) {
        return Ok(());
    }
    let bounds = WindowBounds::centered(WINDOW_SIZE, cx);
    cx.open_window(window_options(bounds), |window, cx| {
        let view = cx.global::<SharedView>().0.clone();
        view.update(cx, |view, cx| view.attach_window(window, cx));
        view
    })?;
    Ok(())
}

/// 找出根视图就是全局那个 [`SettingsView`] 的窗口并激活。
///
/// 返回 `true` 表示窗口本来就在。句柄可能指向正在销毁的窗口（`update` 失败），
/// 那种情况按「窗口不存在」处理，由调用方开新的。
fn activate_main_window(cx: &mut App) -> bool {
    let view_id = cx.global::<SharedView>().0.entity_id();
    let mut found = false;
    for handle in cx.windows() {
        let Some(handle) = handle.downcast::<SettingsView>() else {
            continue;
        };
        let is_main = handle
            .update(cx, |_view, window, cx| {
                if cx.entity().entity_id() != view_id {
                    return false;
                }
                // 最小化时平台实现会先还原，再切到前台。
                window.activate_window();
                true
            })
            .unwrap_or(false);
        found |= is_main;
    }
    found
}
