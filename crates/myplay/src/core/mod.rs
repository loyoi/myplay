//! L3 核心层：配置、状态机、事件归一与应用核心。
//!
//! - [`config`]：配置模型与持久化
//! - [`state`]：接收器五态状态机
//! - [`event`]：三条通道的事件归一为 `UiEvent`
//! - [`app`]：[`Core`](app::Core)（唯一业务状态源，拥有通道与日志）

pub mod app;
pub mod config;
pub mod event;
pub mod state;
