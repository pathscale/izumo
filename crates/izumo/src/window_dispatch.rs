//! Window operations are part of the standalone application handle.
//!
//! Native window attributes come from [`crate::WindowSpec`]. The handle
//! provides visibility changes, UI-thread work, and application events.

pub type BlitzWindowDispatcher = crate::AppHandle;
