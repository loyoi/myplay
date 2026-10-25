//! 接收器状态机。
//!
//! 五态：`Idle → Starting → Running → Stopping → Idle`，任意阶段可因错误进入 `Failed`，
//! `Failed` 可以重新启动、直接停止，或被后续事件带回 `Running`（不是死胡同）。
//! 把「能不能启动/停止」收敛成方法，避免 UI、托盘、Core 各写一份判断。

/// 接收器运行状态。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RunState {
    /// 未启动。
    #[default]
    Idle,
    /// 正在启动（选网卡、起监听、等首帧）。
    Starting,
    /// 正在接收。
    Running,
    /// 正在停止（等核心收尾、发 byebye）。
    Stopping,
    /// 启动或运行中出错。
    Failed,
}

impl RunState {
    /// 界面上显示的文字。
    pub const fn label(self) -> &'static str {
        match self {
            Self::Idle => "未启动",
            Self::Starting => "启动中",
            Self::Running => "运行中",
            Self::Stopping => "停止中",
            Self::Failed => "错误",
        }
    }

    /// 是否可以接受「启动」意图。
    pub const fn can_start(self) -> bool {
        matches!(self, Self::Idle | Self::Failed)
    }

    /// 是否可以接受「停止」意图。
    pub const fn can_stop(self) -> bool {
        matches!(self, Self::Running | Self::Starting | Self::Failed)
    }

    /// 一次合法迁移：返回目标状态，非法请求返回 `None`。
    ///
    /// `core::app::Core::set_status` 是唯一写入口：非法迁移会被它记一条警告，
    /// 因此这张表实际执行，而不是装饰。
    pub fn transition(self, next: Self) -> Option<Self> {
        let allowed = match (self, next) {
            (Self::Idle, Self::Starting)
            // `Failed` 不是终态：既可以重试启动，也可以停下，或者被核心事件带回 `Running`。
            | (Self::Failed, Self::Starting)
            | (Self::Failed, Self::Running)
            | (Self::Failed, Self::Stopping)
            | (Self::Failed, Self::Idle)
            | (Self::Starting, Self::Running)
            | (Self::Starting, Self::Failed)
            | (Self::Starting, Self::Stopping)
            | (Self::Running, Self::Stopping)
            | (Self::Running, Self::Failed)
            | (Self::Stopping, Self::Idle)
            | (Self::Stopping, Self::Failed) => true,
            (current, target) => current == target,
        };
        if allowed { Some(next) } else { None }
    }
}

#[cfg(test)]
mod tests {
    use super::RunState;

    #[test]
    fn start_requires_idle_or_failed() {
        assert!(RunState::Idle.can_start());
        assert!(RunState::Failed.can_start());
        assert!(!RunState::Running.can_start());
        assert!(!RunState::Starting.can_start());
        assert!(!RunState::Stopping.can_start());
    }

    #[test]
    fn normal_lifecycle_is_allowed() {
        let path = [
            (RunState::Idle, RunState::Starting),
            (RunState::Starting, RunState::Running),
            (RunState::Running, RunState::Stopping),
            (RunState::Stopping, RunState::Idle),
        ];
        for (from, to) in path {
            assert_eq!(from.transition(to), Some(to), "{from:?} → {to:?}");
        }
    }

    #[test]
    fn illegal_transitions_are_rejected() {
        assert_eq!(RunState::Idle.transition(RunState::Running), None);
        assert_eq!(RunState::Running.transition(RunState::Starting), None);
        assert_eq!(RunState::Stopping.transition(RunState::Running), None);
        // 重复设置同一状态是幂等的。
        assert_eq!(
            RunState::Running.transition(RunState::Running),
            Some(RunState::Running)
        );
    }

    #[test]
    fn failures_can_happen_from_any_active_state() {
        for state in [RunState::Starting, RunState::Running, RunState::Stopping] {
            assert_eq!(state.transition(RunState::Failed), Some(RunState::Failed));
        }
    }

    #[test]
    fn failed_is_not_a_dead_end() {
        for next in [
            RunState::Starting,
            RunState::Running,
            RunState::Stopping,
            RunState::Idle,
        ] {
            assert_eq!(RunState::Failed.transition(next), Some(next), "{next:?}");
        }
    }
}
