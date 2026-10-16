use std::sync::Arc;
use std::task::Context;

use blitz_dom::Document;

/// Work invoked by a backend during its document poll, on the document thread.
pub type DocumentPollHook =
    Box<dyn FnMut(&mut dyn Document, Option<&Context<'_>>) -> bool + 'static>;

/// A document and the host operations selected by its embedder before mounting.
///
/// The host must support the root and every script document mounted below it.
/// Construction, script bootstrap and host installation belong to the factory;
/// the runtime installs its IPC bridge before the first native poll.
pub struct ApplicationDocument {
    pub document: Box<dyn Document>,
    pub host: Arc<dyn DocumentHost>,
}

impl ApplicationDocument {
    pub fn new(document: Box<dyn Document>, host: Arc<dyn DocumentHost>) -> Self {
        Self { document, host }
    }
}

/// Engine operations used by the application shell.
///
/// Implementations resolve document types at the engine boundary. They must
/// preserve the backend's execution limits, event delivery and redraw wakeups.
/// Methods receive documents only on their owning thread; the shared host must
/// not retain document references or transfer engine state across threads.
/// There are no fallback implementations: an embedder must supply every
/// operation enabled by its build, including control of mounted child pages.
pub trait DocumentHost: Send + Sync + 'static {
    /// Return the actual document URL, set before installing the IPC bridge.
    fn current_url(&self, document: &dyn Document) -> Option<url::Url>;

    fn eval(&self, document: &mut dyn Document, source: &str);

    fn eval_json(
        &self,
        document: &mut dyn Document,
        source: &str,
    ) -> Result<serde_json::Value, String>;

    fn set_ipc_handler(&self, document: &mut dyn Document, handler: Box<dyn Fn(String)>);

    /// Add a hook without replacing hooks previously installed by the embedder.
    fn add_poll_hook(&self, document: &mut dyn Document, hook: DocumentPollHook);

    /// Service the optional debug controller through the selected engine.
    /// Return false for documents the backend cannot inspect, leaving queued
    /// requests available for another window. The backend resolves its concrete
    /// DebugDocument type here, on the document's owning thread.
    #[cfg(feature = "debug-control")]
    fn service_debug_controller(
        &self,
        document: &mut dyn Document,
        controller: &mut blitz_script::DebugController,
        animation_time: f64,
    ) -> bool;

    #[cfg(all(feature = "agent-control", unix))]
    fn agent(
        &self,
        document: &mut dyn Document,
        control: &mut blitz_control_protocol::in_process::DocumentControl,
        request: blitz_control_protocol::AgentControlRequest,
    ) -> blitz_control_protocol::DebugResponse;

    /// Handle capture, snapshot, metrics and idle requests using the target's
    /// retained control state. Advance revisions as each observation requires.
    #[cfg(all(feature = "diagnostics", unix))]
    fn diagnostics(
        &self,
        document: &mut dyn Document,
        control: &mut blitz_control_protocol::in_process::DocumentControl,
        request: blitz_control_protocol::DiagnosticsRequest,
    ) -> blitz_control_protocol::DebugResponse;

    /// Read ordered console and error streams, retaining cursor and loss data.
    #[cfg(all(feature = "diagnostics", unix))]
    fn diagnostic_batch(
        &self,
        document: &dyn Document,
        console_after: u64,
        errors_after: u64,
        limit: usize,
    ) -> blitz_script::DiagnosticBatch;
}
