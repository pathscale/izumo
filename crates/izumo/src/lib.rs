//! Standalone Blitz and Boa application shell.
//!
//! With the `runtime` feature, [`App`] owns a native window and its winit event
//! loop. [`WindowSpec`] supplies the native window settings. Embedders provide
//! a chrome document through [`set_document_factory`], register named JSON
//! commands, and share application state through [`AppBuilder::manage`].
//!
//! [`AppHandle`] is the thread-safe application ingress. Events and UI tasks
//! enter [`ScriptQueue`] and run on the Boa document's owning thread.
//!
//! The Blitz control surface belongs to `blitz-control-protocol`: its
//! vocabulary, document core, socket transport and in-process transport are
//! not re-exported here. An embedder that names a control type depends on that
//! crate directly and registers process lifecycle behavior with
//! `blitz_control_protocol::lifecycle`.
//!
//! This crate retains `set_agent_control_enabled` and
//! `apply_runtime_debug_options` for its own listener. The `diagnostics`
//! feature enables the collectors used by that listener.

#[cfg(feature = "runtime")]
mod app;
#[cfg(feature = "runtime")]
pub use app::{
    App, AppBuilder, AppHandle, AppResult, CommandHandler, CommandResult, Theme, TitleBarStyle,
    WindowSpec,
};
#[cfg(feature = "runtime")]
mod menu;
#[cfg(feature = "runtime")]
pub use menu::{MenuBar, menu_id};
#[cfg(feature = "runtime")]
mod ipc;
#[cfg(feature = "runtime")]
pub use ipc::{attach_ipc_handler, set_remote_ipc_policy};
#[cfg(feature = "runtime")]
mod script_queue;
#[cfg(feature = "runtime")]
pub use script_queue::ScriptQueue;
#[cfg(feature = "runtime")]
mod runtime;
#[cfg(feature = "runtime")]
mod surfaces;
#[cfg(feature = "runtime")]
pub use surfaces::{PageSurface, SurfaceEvent, set_page_surface_hooks};
#[cfg(all(feature = "runtime", feature = "agent-control", unix))]
mod targets;
#[cfg(all(feature = "runtime", feature = "agent-control", unix))]
pub use targets::{
    PageTarget, set_page_diagnostics_handler, set_page_navigation_handler, set_page_target_provider,
};
#[cfg(all(target_os = "macos", feature = "runtime"))]
mod window_effects;
/// The two capability switches an owner sets, as one decision.
///
/// `blitz-traits` owns the type because the collectors it governs are spread
/// across the shell and script crates. It is the argument of this crate's own
/// [`apply_runtime_debug_options`], so it is named here rather than leaving an
/// embedder to depend on `blitz-traits` for one struct.
#[cfg(feature = "agent-control")]
pub use blitz_traits::profiling::DebugOptions as RuntimeDebugOptions;
#[cfg(all(feature = "agent-control", unix))]
#[cfg(feature = "runtime")]
pub use runtime::{agent_control_enabled, apply_runtime_debug_options, set_agent_control_enabled};
#[cfg(feature = "agent-control")]
#[cfg(feature = "runtime")]
pub use runtime::{
    begin_deep_profiling, deep_profiling_enabled, deep_profiling_permitted,
    set_deep_profiling_permitted,
};
#[cfg(feature = "runtime")]
pub use runtime::{set_document_factory, set_runtime_trace};
/// Apply reusable macOS window glass without exposing AppKit to embedders.
#[cfg(all(target_os = "macos", feature = "runtime"))]
pub use window_effects::set_window_glass;
#[cfg(feature = "runtime")]
mod window_dispatch;
#[cfg(feature = "runtime")]
pub use window_dispatch::BlitzWindowDispatcher;
