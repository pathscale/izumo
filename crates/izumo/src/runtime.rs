use std::cell::RefCell;
#[cfg(all(feature = "agent-control", unix))]
use std::sync::Weak;
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(all(feature = "agent-control", unix))]
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

#[cfg(not(feature = "hybrid-renderer"))]
use anyrender_vello::VelloWindowRenderer;
#[cfg(feature = "hybrid-renderer")]
use anyrender_vello_hybrid::VelloHybridWindowRenderer as VelloWindowRenderer;
use blitz_script::ScriptDocument;
use blitz_shell::{
    BlitzApplication, BlitzShellProxy, WindowConfig as BlitzShellWindowConfig,
    create_default_event_loop,
};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize};
use winit::event::WindowEvent as WinitWindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::monitor::Fullscreen;
use winit::window::{WindowAttributes, WindowButtons, WindowLevel};

#[cfg(target_os = "macos")]
use winit::platform::macos::{ApplicationHandlerExtMacOS, WindowAttributesMacOS};

#[cfg(target_os = "windows")]
use winit::platform::windows::WindowAttributesWindows;

#[cfg(all(feature = "diagnostics", unix))]
use blitz_control_protocol::document::snapshot_document;
#[cfg(all(feature = "agent-control", unix))]
use blitz_control_protocol::latest::{Latest, Once};
#[cfg(all(feature = "diagnostics", unix))]
use blitz_control_protocol::{
    DebugSnapshot, DiagnosticsRequest, SnapshotRequest, WindowComposition,
};
/*
 * The control surface is `blitz-control-protocol`, not this crate.
 *
 * The vocabulary, the core that reads and drives a document, the socket and
 * the in-process transport over it all live there. What is left here is a
 * window runtime that happens to own a document, wiring its own document into
 * that surface: `DocumentControl` performs the actions, this decides which
 * window's document to perform them on and what to do about `Relaunch` and
 * `Quit`, which are about the process rather than the document.
 *
 * Nothing is re-exported. A consumer that names a control type depends on the
 * crate that defines it, because a runtime that forwards one has not removed
 * the edge, only renamed it.
 */
use crate::{App, AppHandle, AppResult, WindowSpec, attach_ipc_handler};
#[cfg(target_os = "macos")]
use crate::TitleBarStyle;
#[cfg(all(feature = "agent-control", unix))]
use blitz_control_protocol::in_process::DocumentControl;
#[cfg(all(feature = "agent-control", unix))]
use blitz_control_protocol::server::{
    AgentControlServer, ControlBridge, ControlBridgeRequest, Host,
};
#[cfg(all(feature = "agent-control", unix))]
use blitz_control_protocol::{AgentControlRequest, DebugError, DebugEvent, DebugResponse};

type DocumentFactory = dyn Fn(&str) -> Result<ScriptDocument, String> + Send + Sync + 'static;
type RuntimeTrace = dyn Fn(&str) + Send + Sync + 'static;

static DOCUMENT_FACTORY: OnceLock<RwLock<Option<Arc<DocumentFactory>>>> = OnceLock::new();
static RUNTIME_TRACE: OnceLock<RwLock<Option<Arc<RuntimeTrace>>>> = OnceLock::new();
#[cfg(all(feature = "agent-control", unix))]
static AGENT_CONTROL_RUNTIME: OnceLock<Mutex<Option<Weak<Mutex<AgentControlRuntime>>>>> =
    OnceLock::new();

/// Set the document source used by App::run for its chrome window.
///
/// The factory receives WindowSpec::url. The runtime attaches the application
/// IPC bridge and ScriptQueue before the document's first native poll.
pub fn set_document_factory(
    factory: impl Fn(&str) -> Result<ScriptDocument, String> + Send + Sync + 'static,
) {
    *DOCUMENT_FACTORY
        .get_or_init(|| RwLock::new(None))
        .write()
        .unwrap() = Some(Arc::new(factory));
}

/// Install lifecycle tracing for embedders diagnosing native runtime startup.
pub fn set_runtime_trace(trace: impl Fn(&str) + Send + Sync + 'static) {
    *RUNTIME_TRACE
        .get_or_init(|| RwLock::new(None))
        .write()
        .unwrap() = Some(Arc::new(trace));
}

#[cfg(all(feature = "agent-control", unix))]
struct AgentControlRuntime {
    bridge: ControlBridge,
    events: Arc<Latest<DebugEvent>>,
    server: Option<AgentControlServer>,
}

/// Enable or disable the complete local agent-control interface at runtime.
///
/// Disabled means no listener and no discovery descriptor. The runtime starts
/// disabled; embedders opt in after loading their owner-controlled setting.
#[cfg(all(feature = "agent-control", unix))]
pub fn set_agent_control_enabled(enabled: bool) -> std::io::Result<()> {
    let runtime = AGENT_CONTROL_RUNTIME
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .as_ref()
        .and_then(Weak::upgrade)
        .ok_or_else(|| std::io::Error::other("the Blitz runtime is not initialized"))?;
    let mut runtime = runtime.lock().unwrap();
    match (enabled, runtime.server.is_some()) {
        (true, false) => {
            runtime.server = Some(AgentControlServer::start_with_events(
                runtime.bridge.clone(),
                Host {
                    name: env!("CARGO_PKG_NAME").into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                    diagnostics: cfg!(feature = "diagnostics"),
                },
                Arc::clone(&runtime.events),
            )?)
        }
        (false, true) => runtime.server = None,
        _ => {}
    }
    Ok(())
}

/// Whether the complete local agent-control interface is currently listening.
#[cfg(all(feature = "agent-control", unix))]
pub fn agent_control_enabled() -> bool {
    AGENT_CONTROL_RUNTIME
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .as_ref()
        .and_then(Weak::upgrade)
        .is_some_and(|runtime| runtime.lock().unwrap().server.is_some())
}

/// Apply both debug capabilities as one runtime category decision.
///
/// Deep profiling is always stopped when inspection/control is off, even if an
/// embedder passes an inconsistent pair of booleans. Disabling it also clears
/// retained samples so a later trace starts at its activation boundary.
#[cfg(all(feature = "agent-control", unix))]
pub fn apply_runtime_debug_options(
    options: blitz_traits::profiling::DebugOptions,
) -> std::io::Result<()> {
    // Permission first, then the server. The server takes a profiling session
    // as it starts listening, and that session is only available once sampling
    // is permitted: starting it first meant the consumer asked for samples
    // before the answer could be yes, and collection never began.
    set_deep_profiling_permitted(options.effective_deep_profiling());
    set_agent_control_enabled(options.inspection_and_agent_control)?;
    // The listener may predate this permission change. Refresh its session in
    // place; otherwise a server created while profiling was off holds `None`
    // forever and diagnostics silently return `script: null` after the owner
    // turns profiling on.
    // Only a build that can collect diagnostics holds a sampling session, so
    // only that build has one to refresh.
    #[cfg(feature = "diagnostics")]
    if options.inspection_and_agent_control {
        let runtime = AGENT_CONTROL_RUNTIME
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap()
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or_else(|| std::io::Error::other("the Blitz runtime is not initialized"))?;
        if let Some(server) = runtime.lock().unwrap().server.as_mut() {
            server.refresh_deep_profiling();
        }
    }
    Ok(())
}

/// Permit or forbid the intrusive performance collectors shipped with the
/// runtime. Inspection/control remains independently selectable.
///
/// This grants permission and starts nothing. Collection runs only while a
/// consumer holds a session from [`begin_deep_profiling`], because samples with
/// no reader have no value and are not free: the readers here are the inspector
/// and `blitz-bench`, and both are separate processes, so the ordinary case for
/// an embedder that merely had the setting on was to collect for nobody.
///
/// Forbidding also releases retained samples, so a capability that is off holds
/// nothing.
#[cfg(feature = "agent-control")]
pub fn set_deep_profiling_permitted(permitted: bool) {
    blitz_shell::set_deep_profiling_permitted(permitted);
}

/// Ask for samples for as long as the returned session is held.
///
/// `None` when the profile does not permit sampling. An out-of-process consumer
/// cannot hold this directly, so the request handler serving it holds one for
/// the life of the request.
#[cfg(feature = "agent-control")]
#[must_use = "sampling stops as soon as the session is dropped"]
pub fn begin_deep_profiling() -> Option<blitz_shell::DeepProfilingSession> {
    blitz_shell::begin_deep_profiling()
}

/// Whether the owner has permitted sampling, regardless of any consumer.
#[cfg(feature = "agent-control")]
pub fn deep_profiling_permitted() -> bool {
    blitz_traits::profiling::deep_profiling_permitted()
}

/// Whether collection is actually running: permitted, and a consumer attached.
#[cfg(feature = "agent-control")]
pub fn deep_profiling_enabled() -> bool {
    blitz_traits::profiling::deep_profiling_enabled()
}

fn runtime_trace(message: &str) {
    let callback = RUNTIME_TRACE
        .get_or_init(|| RwLock::new(None))
        .read()
        .unwrap()
        .clone();
    if let Some(callback) = callback {
        callback(message);
    }
}

fn create_document(url: &str) -> AppResult<ScriptDocument> {
    let factory = DOCUMENT_FACTORY
        .get_or_init(|| RwLock::new(None))
        .read()
        .unwrap()
        .clone()
        .ok_or_else(|| std::io::Error::other("Blitz document factory is not configured"))?;
    factory(url).map_err(|error| std::io::Error::other(error).into())
}

#[cfg(all(feature = "agent-control", unix))]
enum RuntimeMessage {
    Control {
        request: ControlBridgeRequest,
        response: Arc<Once<DebugResponse>>,
    },
}

struct RuntimeApplication {
    blitz: RefCell<BlitzApplication<VelloWindowRenderer>>,
    app: App,
    setup_result: Arc<Mutex<Option<AppResult>>>,
    #[cfg(all(feature = "agent-control", unix))]
    receiver: Receiver<RuntimeMessage>,
    ready: bool,
    #[cfg(all(feature = "agent-control", unix))]
    _agent_control: Arc<Mutex<AgentControlRuntime>>,
    /// The in-process transport over this window's document.
    ///
    /// It performs the actions and holds what one request leaves for the next:
    /// the revision, the injected pointer, which buttons it holds, and the
    /// capture surface. All of that was here, in fields beside a second copy of
    /// the action dispatch that had already drifted from the browser's.
    #[cfg(all(feature = "agent-control", unix))]
    control: DocumentControl,
    #[cfg(all(feature = "agent-control", unix))]
    paint_revision: u64,
    #[cfg(all(feature = "agent-control", unix))]
    control_events: Arc<Latest<DebugEvent>>,
}

impl RuntimeApplication {

    #[cfg(all(feature = "diagnostics", unix))]
    fn agent_document(&mut self) -> Option<&mut ScriptDocument> {
        self.blitz
            .get_mut()
            .windows
            .values_mut()
            .find_map(|view| view.try_downcast_doc_mut::<ScriptDocument>())
    }

    /// Answer one agent-control request against this window's document.
    ///
    /// Reading the tree and performing an action are the in-process transport's
    /// job, and it does them the same way for every host. What is left here is
    /// what only a runtime can answer: which window's document is the active
    /// one, and what `Relaunch` and `Quit` mean for a process it started.
    #[cfg(all(feature = "agent-control", unix))]
    fn handle_builtin_agent(&mut self, request: AgentControlRequest) -> DebugResponse {
        match request {
            AgentControlRequest::Inspect { .. } | AgentControlRequest::Act(_) => {
                let Self { blitz, control, .. } = self;
                let Some(document) = blitz
                    .get_mut()
                    .windows
                    .values_mut()
                    .find_map(|view| view.try_downcast_doc_mut::<ScriptDocument>())
                else {
                    return DebugResponse::Error(DebugError {
                        code: "documentUnavailable".into(),
                        message: "no active script document".into(),
                    });
                };
                control.agent(document, request)
            }
            // The embedder's answer if it registered one, and this runtime's
            // own if it did not. Whose answer it is is not this crate's to
            // decide: an application that must drain a database before it
            // restarts registers with `blitz_control_protocol::lifecycle`, and
            // does not reach a window runtime to do it.
            AgentControlRequest::Relaunch => blitz_control_protocol::lifecycle::lifecycle_handler()
                .map(|handler| handler(AgentControlRequest::Relaunch))
                .unwrap_or_else(|| match relaunch_current_process() {
                    Ok(()) => DebugResponse::Ack,
                    Err(error) => DebugResponse::Error(DebugError {
                        code: "relaunchFailed".into(),
                        message: error.to_string(),
                    }),
                }),
            AgentControlRequest::Quit => DebugResponse::Ack,
            _ => DebugResponse::Error(DebugError {
                code: "unsupportedRequest".into(),
                message: "this runtime does not implement that agent-control request".into(),
            }),
        }
    }

    #[cfg(all(feature = "diagnostics", unix))]
    fn handle_builtin_diagnostics(&mut self, request: DiagnosticsRequest) -> DebugResponse {
        match request {
            DiagnosticsRequest::Snapshot(request) => self
                .collect_diagnostics(request)
                .map(DebugResponse::Snapshot)
                .unwrap_or_else(DebugResponse::Error),
            DiagnosticsRequest::Metrics => self
                .collect_diagnostics(SnapshotRequest {
                    include_dom: false,
                    include_layout: false,
                    include_computed_style: false,
                    node_ids: Vec::new(),
                })
                .map(|snapshot| DebugResponse::Metrics(snapshot.metrics))
                .unwrap_or_else(DebugResponse::Error),
            DiagnosticsRequest::WaitForIdle => self
                .collect_diagnostics(SnapshotRequest {
                    include_dom: false,
                    include_layout: false,
                    include_computed_style: false,
                    node_ids: Vec::new(),
                })
                .map(|snapshot| DebugResponse::Idle(snapshot.revisions))
                .unwrap_or_else(DebugResponse::Error),
            // The one diagnostic only a runtime can answer. A renderer capture
            // cannot see whether an opaque platform window or a tinted native
            // glass view was placed behind its transparent root, and this crate
            // is what put it there.
            DiagnosticsRequest::WindowComposition => {
                DebugResponse::WindowComposition(native_window_composition())
            }
            DiagnosticsRequest::Capture(request) => self
                .capture_image(request)
                .map(DebugResponse::Captured)
                .unwrap_or_else(DebugResponse::Error),
            DiagnosticsRequest::Observe { .. } => DebugResponse::Error(DebugError {
                code: "streamingUnavailable".into(),
                message: "diagnostic subscriptions are not implemented; request snapshots or \
                          metrics"
                    .into(),
            }),
            _ => DebugResponse::Error(DebugError {
                code: "unsupportedRequest".into(),
                message: "this runtime does not implement that diagnostics request".into(),
            }),
        }
    }

    /// Draw the document offscreen and hand back the pixels.
    ///
    /// The point of this is that it is the *same* paint. `blitz_paint::paint_scene`
    /// is the entry point the window renderer uses, so a capture cannot agree
    /// with a broken frame or disagree with a good one: whatever the window
    /// would show is what lands in this buffer.
    #[cfg(all(feature = "diagnostics", unix))]
    fn capture_image(
        &mut self,
        request: blitz_control_protocol::CaptureRequest,
    ) -> Result<blitz_control_protocol::CapturedImage, DebugError> {
        let Self { blitz, control, .. } = self;
        let document = blitz
            .get_mut()
            .windows
            .values_mut()
            .find_map(|view| view.try_downcast_doc_mut::<ScriptDocument>())
            .ok_or_else(|| DebugError {
                code: "documentUnavailable".into(),
                message: "no active script document".into(),
            })?;
        control.capture(document, request)
    }
}

impl RuntimeApplication {
    #[cfg(all(feature = "diagnostics", unix))]
    fn collect_diagnostics(
        &mut self,
        request: SnapshotRequest,
    ) -> Result<DebugSnapshot, DebugError> {
        let revision = self.control.next_revision();
        let document = self.agent_document().ok_or_else(|| DebugError {
            code: "documentUnavailable".into(),
            message: "no active script document".into(),
        })?;
        snapshot_document(document, request, revision)
    }
}

#[cfg(all(feature = "agent-control", unix))]
impl RuntimeApplication {
    fn drain_runtime_messages(&mut self, event_loop: &dyn ActiveEventLoop) {
        while let Ok(message) = self.receiver.try_recv() {
            match message {
                RuntimeMessage::Control { request, response } => {
                    let result = match request {
                        ControlBridgeRequest::Agent(AgentControlRequest::Quit) => {
                            event_loop.exit();
                            DebugResponse::Ack
                        }
                        ControlBridgeRequest::Agent(AgentControlRequest::Relaunch) => {
                            let delegated =
                                blitz_control_protocol::lifecycle::lifecycle_handler().is_some();
                            let result = self.handle_builtin_agent(AgentControlRequest::Relaunch);
                            if !delegated && matches!(result, DebugResponse::Ack) {
                                event_loop.exit();
                            }
                            result
                        }
                        ControlBridgeRequest::Agent(request) => self.handle_builtin_agent(request),
                        // A build without `diagnostics` answers rather than
                        // failing to compile the arm. The transport forwards
                        // every request now, because whether a host can collect
                        // a snapshot is the host's answer and not something a
                        // socket can infer; `Host::diagnostics` is what keeps
                        // the tool out of `tools/list` so a client is not
                        // invited to ask in the first place.
                        #[cfg(not(feature = "diagnostics"))]
                        ControlBridgeRequest::Diagnostics(_) => DebugResponse::Error(DebugError {
                            code: "diagnosticsUnavailable".into(),
                            message: "this build has no diagnostics feature; rebuild with \
                                      izumo/diagnostics"
                                .into(),
                        }),
                        // Answered here, and only here. There used to be a
                        // `set_diagnostics_handler` an embedder could install
                        // to answer instead, which is a runtime proxying an
                        // inspection service: the collectors, the offscreen
                        // paint and the snapshot readers are all
                        // `blitz-control-protocol`, and an embedder that wants
                        // to serve them serves them itself rather than through
                        // the crate that owns the window. Nothing installed
                        // one.
                        #[cfg(feature = "diagnostics")]
                        ControlBridgeRequest::Diagnostics(request) => {
                            self.handle_builtin_diagnostics(request)
                        }
                    };
                    response.fill(result);
                }
            }
        }
    }
}

impl ApplicationHandler for RuntimeApplication {
    #[cfg(target_os = "macos")]
    fn macos_handler(&mut self) -> Option<&mut dyn ApplicationHandlerExtMacOS> {
        Some(self)
    }

    fn can_create_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        runtime_trace("runtime can_create_surfaces entered");
        if event_loop.exiting() {
            return;
        }
        if !self.ready {
            self.ready = true;
            runtime_trace("application setup started");
            if let Err(error) = self.app.run_setup() {
                *self.setup_result.lock().unwrap() = Some(Err(error));
                event_loop.exit();
                return;
            }
            runtime_trace("application setup completed");
        }
        self.blitz.get_mut().can_create_surfaces(event_loop);
        runtime_trace("runtime can_create_surfaces completed");
    }

    fn destroy_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        self.blitz.get_mut().destroy_surfaces(event_loop);
    }

    fn resumed(&mut self, event_loop: &dyn ActiveEventLoop) {
        runtime_trace("runtime resumed entered");
        self.blitz.get_mut().resumed(event_loop);
        runtime_trace("runtime resumed completed");
    }

    fn suspended(&mut self, event_loop: &dyn ActiveEventLoop) {
        self.blitz.get_mut().suspended(event_loop);
    }

    fn window_event(
        &mut self,
        event_loop: &dyn ActiveEventLoop,
        window_id: winit::window::WindowId,
        event: WinitWindowEvent,
    ) {
        #[cfg(all(feature = "agent-control", unix))]
        let paint_committed = matches!(event, WinitWindowEvent::RedrawRequested);
        self.blitz
            .get_mut()
            .window_event(event_loop, window_id, event);
        #[cfg(all(feature = "agent-control", unix))]
        if paint_committed {
            self.paint_revision = self.paint_revision.saturating_add(1);
            self.control_events.set_now(DebugEvent::PaintCommitted {
                revision: self.paint_revision,
            });
        }
    }

    fn proxy_wake_up(&mut self, event_loop: &dyn ActiveEventLoop) {
        // Control requests run before renderer wake work so an exit request
        // cannot be swallowed by a renderer callback.
        #[cfg(all(feature = "agent-control", unix))]
        self.drain_runtime_messages(event_loop);
        if event_loop.exiting() {
            return;
        }
        self.blitz.get_mut().proxy_wake_up(event_loop);
    }

    fn about_to_wait(&mut self, event_loop: &dyn ActiveEventLoop) {
        #[cfg(all(feature = "agent-control", unix))]
        self.drain_runtime_messages(event_loop);
        if event_loop.exiting() {
            return;
        }
        self.blitz.get_mut().about_to_wait(event_loop);
    }
}

#[cfg(target_os = "macos")]
impl ApplicationHandlerExtMacOS for RuntimeApplication {
    fn standard_key_binding(
        &mut self,
        event_loop: &dyn ActiveEventLoop,
        window_id: winit::window::WindowId,
        action: &str,
    ) {
        self.blitz
            .get_mut()
            .standard_key_binding(event_loop, window_id, action);
    }
}

impl Drop for RuntimeApplication {
    fn drop(&mut self) {
        self.app.handle().clear_native_window();
        #[cfg(target_os = "macos")]
        {
            if let Some(slot) = NATIVE_WINDOW.get() {
                slot.lock().unwrap().take();
            }
            NATIVE_SURFACE_TRANSPARENT.store(false, Ordering::Release);
        }
    }
}

/// Create one chrome window and run the native loop on the calling thread.
pub(crate) fn run(app: App) -> AppResult {
    runtime_trace("application run entered");
    runtime_trace("native event-loop construction started");
    let event_loop = create_default_event_loop();
    runtime_trace("native event-loop construction completed");
    let (proxy, blitz_receiver) = BlitzShellProxy::new(event_loop.create_proxy());
    #[cfg(all(feature = "agent-control", unix))]
    let (sender, receiver) = channel();
    #[cfg(all(feature = "agent-control", unix))]
    let (agent_control, control_events) = {
        let control_proxy = proxy.clone();
        let bridge: ControlBridge = Arc::new(move |request| {
            let response = Once::new();
            let receiver = Arc::clone(&response);
            if sender
                .send(RuntimeMessage::Control { request, response })
                .is_err()
            {
                receiver.fill(DebugResponse::Error(DebugError {
                    code: "runtimeUnavailable".into(),
                    message: "the native event loop has stopped".into(),
                }));
            } else {
                control_proxy.wake_up();
            }
            receiver
        });
        // A watch channel retains only the newest revision. Inspection is
        // therefore bounded even when a client stalls or an animation
        // presents much faster than the client can consume notifications.
        let event_sender = Latest::new();
        let event_receiver = Arc::clone(&event_sender);
        // The embedder's persisted control state is applied during setup.
        // The CLI rescue starts earlier so QA can attach during startup.
        let server = if std::env::args().any(|arg| arg == "--blitz-control") {
            Some(AgentControlServer::start_with_events(
                bridge.clone(),
                Host {
                    name: env!("CARGO_PKG_NAME").into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                    diagnostics: cfg!(feature = "diagnostics"),
                },
                event_receiver,
            )?)
        } else {
            None
        };
        let runtime = Arc::new(Mutex::new(AgentControlRuntime {
            bridge,
            events: event_sender.clone(),
            server,
        }));
        *AGENT_CONTROL_RUNTIME
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap() = Some(Arc::downgrade(&runtime));
        (runtime, event_sender)
    };
    let mut blitz = BlitzApplication::new(proxy, blitz_receiver);
    #[cfg(feature = "debug-control")]
    if let Some(mut controller) =
        blitz_script::DebugController::start_from_env(env!("CARGO_PKG_VERSION"))?
    {
        let (width, height) = debug_screenshot_size_from_env().unwrap_or((1344, 932));
        controller = controller.with_cpu_screenshot(width, height);
        blitz.set_debug_controller(controller);
    }
    let handle = app.handle();
    register_window(&handle, &mut blitz, app.window_spec())?;
    let setup_result = Arc::new(Mutex::new(None));
    let application = RuntimeApplication {
        blitz: RefCell::new(blitz),
        app,
        setup_result: Arc::clone(&setup_result),
        #[cfg(all(feature = "agent-control", unix))]
        receiver,
        ready: false,
        #[cfg(all(feature = "agent-control", unix))]
        _agent_control: agent_control,
        #[cfg(all(feature = "agent-control", unix))]
        control: DocumentControl::new(),
        #[cfg(all(feature = "agent-control", unix))]
        paint_revision: 0,
        #[cfg(all(feature = "agent-control", unix))]
        control_events,
    };
    runtime_trace("native event loop run_app started");
    let loop_result = event_loop.run_app(application);
    runtime_trace("native event loop run_app returned");
    if let Some(result) = setup_result.lock().unwrap().take() {
        result?;
    }
    loop_result?;
    Ok(())
}

#[cfg(all(feature = "agent-control", unix))]
fn relaunch_current_process() -> std::io::Result<()> {
    let executable = std::env::current_exe()?;
    #[cfg(target_os = "macos")]
    if let Some(bundle) = containing_app_bundle(&executable) {
        let mut command = std::process::Command::new("/usr/bin/open");
        command.arg("-n");
        command.arg(bundle).spawn()?;
        return Ok(());
    }
    std::process::Command::new(executable)
        .args(std::env::args_os().skip(1))
        .spawn()?;
    Ok(())
}

#[cfg(all(feature = "agent-control", target_os = "macos"))]
fn containing_app_bundle(executable: &std::path::Path) -> Option<std::path::PathBuf> {
    let macos = executable.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    (macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && bundle.extension()? == "app")
        .then(|| bundle.to_path_buf())
}

#[cfg(feature = "debug-control")]
fn debug_screenshot_size_from_env() -> Option<(u32, u32)> {
    let value = std::env::var("TAURI_BLITZ_DRIVER_SCREENSHOT_SIZE").ok()?;
    let (width, height) = value.split_once('x')?;
    let width = width.parse().ok()?;
    let height = height.parse().ok()?;
    (width > 0 && height > 0).then_some((width, height))
}

fn register_window(
    handle: &AppHandle,
    application: &mut BlitzApplication<VelloWindowRenderer>,
    config: WindowSpec,
) -> AppResult {
    runtime_trace("native window registration entered");
    let mut document = create_document(&config.url)?;
    handle.script_queue().attach_to(&mut document);
    attach_ipc_handler(&mut document, handle.clone());

    let attributes = window_attributes(&config);
    let handle_for_creation = handle.clone();
    let background_color = config
        .background_color
        .map(|[red, green, blue, alpha]| peniko::Color::from_rgba8(red, green, blue, alpha))
        .unwrap_or(peniko::Color::WHITE);
    // A transparent window needs transparent surface compositing and a clear
    // frame. An opaque window uses the embedder's background color.
    #[cfg(not(feature = "hybrid-renderer"))]
    let renderer = if config.transparent {
        VelloWindowRenderer::with_options(
            anyrender_vello::VelloRendererOptions::default()
                .composite_alpha_mode(anyrender::CompositeAlphaMode::Transparent)
                .base_color(peniko::Color::TRANSPARENT),
        )
    } else {
        VelloWindowRenderer::with_options(
            anyrender_vello::VelloRendererOptions::default().base_color(background_color),
        )
    };
    // The hybrid pipeline, same decision through its own options type.
    //
    // It is the renderer that can carry CSS filters: `anyrender_vello_hybrid`
    // forwards them to `vello_hybrid`, which renders a filtered layer into an
    // intermediate texture and applies the filter there. Classic vello has no
    // filter parameter at all, so a build that wants `filter:` wants this one.
    #[cfg(feature = "hybrid-renderer")]
    let renderer = if config.transparent {
        VelloWindowRenderer::with_options(
            anyrender_vello_hybrid::VelloHybridRendererOptions::default()
                .composite_alpha_mode(anyrender::CompositeAlphaMode::Transparent)
                .base_color(peniko::Color::TRANSPARENT),
        )
    } else {
        VelloWindowRenderer::with_options(
            anyrender_vello_hybrid::VelloHybridRendererOptions::default()
                .base_color(background_color),
        )
    };
    let window =
        BlitzShellWindowConfig::with_attributes(Box::new(document), renderer, attributes)
            .with_on_created(move |native| {
                // Setup runs after attributes were snapshotted. Binding replays
                // the current visibility, including show() on a hidden window.
                #[cfg(target_os = "macos")]
                {
                    let slot = NATIVE_WINDOW.get_or_init(|| Mutex::new(None));
                    if let Ok(mut guard) = slot.lock() {
                        *guard = Some(Arc::clone(&native));
                    }
                }
                handle_for_creation.bind_native_window(native);
            });
    #[cfg(target_os = "macos")]
    NATIVE_SURFACE_TRANSPARENT.store(config.transparent, Ordering::Release);
    application.add_window(window);
    runtime_trace("native window queued");
    Ok(())
}

#[cfg(target_os = "macos")]
static NATIVE_WINDOW: std::sync::OnceLock<Mutex<Option<Arc<dyn winit::window::Window>>>> =
    std::sync::OnceLock::new();

#[cfg(target_os = "macos")]
static NATIVE_SURFACE_TRANSPARENT: AtomicBool = AtomicBool::new(false);

#[cfg(all(feature = "diagnostics", unix, target_os = "macos"))]
fn native_window_composition() -> WindowComposition {
    crate::window_effects::composition(NATIVE_SURFACE_TRANSPARENT.load(Ordering::Acquire))
}

#[cfg(all(feature = "diagnostics", unix, not(target_os = "macos")))]
fn native_window_composition() -> WindowComposition {
    WindowComposition::default()
}

/// Run an embedder-owned macOS integration against the native window.
///
/// TRB deliberately exposes the window without interpreting the callback. App
/// chrome, platform effects, and their dependencies remain in the application.
#[cfg(target_os = "macos")]
pub(crate) fn with_native_window(callback: impl FnOnce(&dyn winit::window::Window)) -> bool {
    let Some(slot) = NATIVE_WINDOW.get() else {
        return false;
    };
    let Ok(guard) = slot.lock() else {
        return false;
    };
    let Some(window) = guard.as_ref() else {
        return false;
    };
    callback(window.as_ref());
    true
}

fn window_attributes(config: &WindowSpec) -> WindowAttributes {
    let mut attributes = WindowAttributes::default()
        .with_title(config.title.clone())
        .with_surface_size(LogicalSize::new(config.width, config.height))
        .with_resizable(config.resizable)
        .with_maximized(config.maximized)
        .with_visible(config.visible)
        .with_decorations(config.decorations)
        .with_content_protected(config.content_protected)
        // Surface compositing and the renderer's clear color must agree.
        .with_transparent(config.transparent)
        .with_theme(config.theme)
        .with_active(config.focus);

    if let (Some(x), Some(y)) = (config.x, config.y) {
        attributes = attributes.with_position(LogicalPosition::new(x, y));
    }
    if let (Some(width), Some(height)) = (config.min_width, config.min_height) {
        attributes = attributes.with_min_surface_size(LogicalSize::new(width, height));
    }
    if let (Some(width), Some(height)) = (config.max_width, config.max_height) {
        attributes = attributes.with_max_surface_size(LogicalSize::new(width, height));
    }
    if config.fullscreen {
        attributes = attributes.with_fullscreen(Some(Fullscreen::Borderless(None)));
    }
    if config.always_on_top {
        attributes = attributes.with_window_level(WindowLevel::AlwaysOnTop);
    } else if config.always_on_bottom {
        attributes = attributes.with_window_level(WindowLevel::AlwaysOnBottom);
    }

    let mut buttons = WindowButtons::empty();
    if config.closable {
        buttons |= WindowButtons::CLOSE;
    }
    if config.minimizable {
        buttons |= WindowButtons::MINIMIZE;
    }
    if config.maximizable {
        buttons |= WindowButtons::MAXIMIZE;
    }
    attributes = attributes.with_enabled_buttons(buttons);

    #[cfg(target_os = "macos")]
    {
        let macos_attributes = match config.title_bar_style {
            TitleBarStyle::Visible => {
                WindowAttributesMacOS::default().with_title_hidden(config.hidden_title)
            }
            TitleBarStyle::Transparent => WindowAttributesMacOS::default()
                .with_titlebar_transparent(true)
                .with_title_hidden(config.hidden_title),
            TitleBarStyle::Overlay => WindowAttributesMacOS::default()
                .with_titlebar_transparent(true)
                .with_title_hidden(config.hidden_title)
                .with_fullsize_content_view(true),
        };
        attributes = attributes.with_platform_attributes(Box::new(macos_attributes));
    }

    #[cfg(target_os = "windows")]
    {
        let windows_attributes = WindowAttributesWindows::default()
            .with_no_redirection_bitmap(config.no_redirection_bitmap);
        attributes = attributes.with_platform_attributes(Box::new(windows_attributes));
    }

    attributes
}

