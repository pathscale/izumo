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
use blitz_control_protocol::WindowComposition;
#[cfg(all(feature = "agent-control", unix))]
use blitz_control_protocol::journal::DiagnosticJournal;
#[cfg(all(feature = "agent-control", unix))]
use blitz_control_protocol::latest::Once;
#[cfg(all(feature = "agent-control", unix))]
use blitz_control_protocol::{
    TargetCapabilities, TargetCommand, TargetContext, TargetId, TargetInfo, TargetOptions,
    TargetRequest,
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
#[cfg(target_os = "macos")]
use crate::TitleBarStyle;
use crate::document::{ApplicationDocument, DocumentHost};
use crate::ipc::attach_ipc_handler_for_url;
use crate::{App, AppHandle, AppResult, WindowSpec};
#[cfg(all(feature = "diagnostics", unix))]
use blitz_control_protocol::DiagnosticsRequest;
#[cfg(all(feature = "agent-control", unix))]
use blitz_control_protocol::server::{
    AgentControlServer, ControlBridge, ControlBridgeRequest, Host,
};
#[cfg(all(feature = "agent-control", unix))]
use blitz_control_protocol::{AgentControlRequest, DebugError, DebugEvent, DebugResponse};

type DocumentFactory = dyn Fn(&str) -> Result<ApplicationDocument, String> + Send + Sync + 'static;
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
/// Return [`ApplicationDocument`] with the selected engine's [`DocumentHost`].
/// The host must also support every child document mounted under this root.
///
/// The chrome document and all scripts it executes must be trusted. The factory
/// must set the document's actual source URL before returning it. IPC from an
/// HTTP or HTTPS requested or actual document URL is denied unless explicitly
/// permitted by [`crate::set_remote_ipc_policy`].
pub fn set_document_factory(
    factory: impl Fn(&str) -> Result<ApplicationDocument, String> + Send + Sync + 'static,
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
    events: Arc<DiagnosticJournal>,
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
            runtime.server = Some(AgentControlServer::start_with_journal(
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

fn create_document(url: &str) -> AppResult<ApplicationDocument> {
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
    #[cfg(all(feature = "agent-control", unix))]
    host: Arc<dyn DocumentHost>,
    app: App,
    setup_result: Arc<Mutex<Option<AppResult>>>,
    #[cfg(all(feature = "agent-control", unix))]
    receiver: Receiver<RuntimeMessage>,
    ready: bool,
    surfaces_suspended: bool,
    surface_visibility: std::collections::HashMap<crate::surfaces::PageSurface, bool>,
    #[cfg(all(feature = "agent-control", unix))]
    _agent_control: Arc<Mutex<AgentControlRuntime>>,
    /// The in-process transport over this window's document.
    ///
    /// It performs the actions and holds what one request leaves for the next:
    /// the revision, the injected pointer, which buttons it holds, and the
    /// capture surface. All of that was here, in fields beside a second copy of
    /// the action dispatch that had already drifted from the browser's.
    #[cfg(all(feature = "agent-control", unix))]
    targets: crate::targets::TargetRegistry,
    #[cfg(all(feature = "agent-control", unix))]
    paint_revision: u64,
    #[cfg(all(feature = "agent-control", unix))]
    control_events: Arc<DiagnosticJournal>,
}

#[cfg(all(feature = "agent-control", unix))]
fn with_target_document(
    blitz: &mut BlitzApplication<VelloWindowRenderer>,
    page: Option<&crate::targets::PageTarget>,
    context: &TargetContext,
    prepare_layout: bool,
    execute: impl FnOnce(&mut dyn blitz_dom::Document) -> DebugResponse,
) -> DebugResponse {
    let unavailable = || {
        DebugResponse::Error(blitz_control_protocol::target::error(
            "targetUnavailable",
            "the target has no mounted script document",
        ))
    };
    let owner = page
        .and_then(|page| page.owner_document_id)
        .unwrap_or(context.document_id);
    let Some(root) = blitz
        .windows
        .values_mut()
        .map(|view| view.doc.as_mut())
        .find(|document| document.inner().id() as u64 == owner)
    else {
        return unavailable();
    };
    if let Some(page) = page {
        let Some(mount) = page.mount_node_id else {
            return unavailable();
        };
        let viewport = if prepare_layout {
            let inner = root.inner();
            let mut viewport = inner.viewport().clone();
            let size = page
                .viewport_node_id
                .and_then(|id| inner.get_node(blitz_dom::NodeId::from_u64(id)))
                .map(|node| node.final_layout().size);
            size.filter(|size| size.width > 0.0 && size.height > 0.0)
                .map(|size| {
                    let scale = viewport.scale();
                    viewport.window_size =
                        ((size.width * scale) as u32, (size.height * scale) as u32);
                    viewport
                })
        } else {
            None
        };
        let mut inner = root.inner_mut();
        let Some(child) = inner.subdoc_mut(blitz_dom::NodeId::from_u64(mount)) else {
            return unavailable();
        };
        if child.id() as u64 != context.document_id {
            return DebugResponse::Error(blitz_control_protocol::target::error(
                "staleDocument",
                "the mount now owns a different document",
            ));
        }
        let document: &mut dyn blitz_dom::Document = child;
        if let Some(viewport) = viewport {
            let changed = document.inner().viewport() != &viewport;
            if changed {
                document.inner_mut().set_viewport(viewport);
                document.inner_mut().resolve(0.0);
            }
        }
        execute(document)
    } else {
        if root.inner().id() as u64 != context.document_id {
            return DebugResponse::Error(blitz_control_protocol::target::error(
                "staleDocument",
                "the chrome document changed",
            ));
        }
        execute(root)
    }
}

#[cfg(all(feature = "agent-control", unix))]
impl RuntimeApplication {
    fn chrome_context(&mut self) -> Option<TargetContext> {
        self.blitz
            .get_mut()
            .windows
            .values_mut()
            .map(|view| view.doc.as_mut())
            .next()
            .map(|document| TargetContext {
                target_id: if crate::targets::has_provider() {
                    TargetId::chrome()
                } else {
                    TargetId::Tab(0)
                },
                generation: if crate::targets::has_provider() {
                    0
                } else {
                    (u64::from(std::process::id()) << 32) | document.inner().id() as u64
                },
                document_id: document.inner().id() as u64,
            })
    }

    fn refresh_targets(&mut self) {
        let chrome = self.chrome_context();
        self.targets.refresh(chrome);
    }

    fn list_targets(&mut self) -> DebugResponse {
        let mut targets: Vec<TargetInfo> = self
            .blitz
            .get_mut()
            .windows
            .values_mut()
            .map(|view| view.doc.as_mut())
            .take(1)
            .map(|document| TargetInfo {
                id: if crate::targets::has_provider() {
                    TargetId::chrome()
                } else {
                    TargetId::Tab(0)
                },
                url: self
                    .host
                    .current_url(document)
                    .map_or_else(String::new, |url| url.to_string()),
                title: "Chrome".into(),
                active: true,
                generation: if crate::targets::has_provider() {
                    0
                } else {
                    (u64::from(std::process::id()) << 32) | document.inner().id() as u64
                },
                document_id: Some(document.inner().id() as u64),
                capabilities: TargetCapabilities {
                    agent: true,
                    diagnostics: cfg!(feature = "diagnostics"),
                    capture: cfg!(feature = "diagnostics"),
                    console: cfg!(feature = "diagnostics"),
                    runtime_errors: cfg!(feature = "diagnostics"),
                    web_gpu: false,
                },
            })
            .collect();
        targets.extend(self.targets.pages.iter().map(|page| page.info.clone()));
        DebugResponse::Targets(targets)
    }

    fn handle_builtin_agent(&mut self, request: AgentControlRequest) -> DebugResponse {
        self.handle_legacy_target(TargetCommand::Agent(request))
    }

    #[cfg(feature = "diagnostics")]
    fn handle_builtin_diagnostics(&mut self, request: DiagnosticsRequest) -> DebugResponse {
        self.handle_legacy_target(TargetCommand::Diagnostics(request))
    }

    fn handle_legacy_target(&mut self, command: TargetCommand) -> DebugResponse {
        let mut request = TargetRequest {
            options: TargetOptions::default(),
            command,
        };
        if !request.instance_scoped()
            && !crate::targets::has_provider()
            && let Some(context) = self.chrome_context()
        {
            request.options.expected_generation = Some(context.generation);
            request.options.expected_document_id = Some(context.document_id);
        }
        match self.handle_targeted(request) {
            DebugResponse::Targeted(reply) => *reply.response,
            response => response,
        }
    }

    fn handle_targeted(&mut self, request: TargetRequest) -> DebugResponse {
        if let Err(error) = request.validate_options() {
            return DebugResponse::Error(error);
        }
        self.refresh_targets();
        // Discovery, process lifecycle and platform composition retain instance scope.
        if request.instance_scoped() {
            return match request.command {
                TargetCommand::Agent(AgentControlRequest::ListTargets) => self.list_targets(),
                TargetCommand::Agent(AgentControlRequest::Quit) => DebugResponse::Ack,
                TargetCommand::Agent(AgentControlRequest::Relaunch) => {
                    blitz_control_protocol::lifecycle::lifecycle_handler()
                        .map(|handler| handler(AgentControlRequest::Relaunch))
                        .unwrap_or_else(|| match relaunch_current_process() {
                            Ok(()) => DebugResponse::Ack,
                            Err(error) => DebugResponse::Error(DebugError {
                                code: "relaunchFailed".into(),
                                message: error.to_string(),
                            }),
                        })
                }
                #[cfg(feature = "diagnostics")]
                TargetCommand::Diagnostics(DiagnosticsRequest::WindowComposition) => {
                    DebugResponse::WindowComposition(native_window_composition())
                }
                _ => DebugResponse::Error(blitz_control_protocol::target::error(
                    "diagnosticsUnavailable",
                    "diagnostics collection is disabled",
                )),
            };
        }
        if request.options.all_targets {
            return if cfg!(feature = "diagnostics") {
                DebugResponse::Ack
            } else {
                DebugResponse::Error(blitz_control_protocol::target::error(
                    "diagnosticsUnavailable",
                    "diagnostic observation is disabled",
                ))
            };
        }
        let target_id = request.options.target.as_ref().map_or_else(
            || {
                if crate::targets::has_provider() {
                    TargetId::chrome()
                } else {
                    TargetId::Tab(0)
                }
            },
            |target| target.id(),
        );
        let page = if target_id == TargetId::chrome()
            || (!crate::targets::has_provider() && target_id == TargetId::Tab(0))
        {
            None
        } else {
            match self
                .targets
                .pages
                .iter()
                .find(|page| page.info.id == target_id)
            {
                Some(page) => Some(page.clone()),
                None => {
                    return DebugResponse::Error(blitz_control_protocol::target::error(
                        "unknownTarget",
                        "the tab is closed or unknown",
                    ));
                }
            }
        };
        let context = match &page {
            Some(page) => match page.info.document_id {
                Some(document_id) => TargetContext {
                    target_id,
                    generation: page.info.generation,
                    document_id,
                },
                None => {
                    return DebugResponse::Error(blitz_control_protocol::target::error(
                        "targetUnavailable",
                        "the tab has not mounted a document",
                    ));
                }
            },
            None => match self.chrome_context() {
                Some(context) => context,
                None => {
                    return DebugResponse::Error(blitz_control_protocol::target::error(
                        "targetUnavailable",
                        "the chrome document is unavailable",
                    ));
                }
            },
        };
        if let Err(error) = context.validate(&request) {
            return DebugResponse::Error(error);
        }
        let state = self.targets.state(&context);
        let host = Arc::clone(&self.host);
        let result = with_target_document(
            self.blitz.get_mut(),
            page.as_ref(),
            &context,
            true,
            |document| match request.command {
                TargetCommand::Agent(AgentControlRequest::Navigate { url }) => {
                    match context.target_id {
                        TargetId::Tab(id) => crate::targets::navigate(id, &url),
                        _ => DebugResponse::Error(blitz_control_protocol::target::error(
                            "unsupportedRequest",
                            "the chrome document cannot navigate",
                        )),
                    }
                }
                TargetCommand::Agent(request) => host.agent(document, &mut state.control, request),
                #[cfg(feature = "diagnostics")]
                TargetCommand::Diagnostics(request) => match request {
                    DiagnosticsRequest::Observe { .. } => DebugResponse::Ack,
                    DiagnosticsRequest::Capture(_)
                    | DiagnosticsRequest::Snapshot(_)
                    | DiagnosticsRequest::Metrics
                    | DiagnosticsRequest::WaitForIdle => {
                        host.diagnostics(document, &mut state.control, request)
                    }
                    request => crate::targets::diagnostics(document, request),
                },
                #[cfg(not(feature = "diagnostics"))]
                TargetCommand::Diagnostics(_) => {
                    DebugResponse::Error(blitz_control_protocol::target::error(
                        "diagnosticsUnavailable",
                        "diagnostics collection is disabled",
                    ))
                }
            },
        );
        result.with_context(context)
    }

    fn publish_target_events(&mut self) {
        self.refresh_targets();
        #[cfg(feature = "diagnostics")]
        if self._agent_control.lock().unwrap().server.is_some() {
            let mut routes: Vec<(TargetContext, Option<crate::targets::PageTarget>)> = Vec::new();
            if let Some(context) = self.chrome_context() {
                routes.push((context, None));
            }
            routes.extend(self.targets.pages.iter().filter_map(|page| {
                page.info.document_id.map(|document_id| {
                    (
                        TargetContext {
                            target_id: page.info.id.clone(),
                            generation: page.info.generation,
                            document_id,
                        },
                        Some(page.clone()),
                    )
                })
            }));
            for (context, page) in routes {
                let state = self.targets.state(&context);
                let journal = &self.control_events;
                let host = Arc::clone(&self.host);
                with_target_document(
                    self.blitz.get_mut(),
                    page.as_ref(),
                    &context,
                    false,
                    |document| {
                        let batch = host.diagnostic_batch(
                            document,
                            state.console_sequence,
                            state.error_sequence,
                            1000,
                        );
                        state.console_sequence = batch.console.next_sequence;
                        state.error_sequence = batch.errors.next_sequence;
                        for (stream, lost) in [
                            (
                                blitz_control_protocol::DebugStream::Console,
                                batch.console.lost,
                            ),
                            (
                                blitz_control_protocol::DebugStream::RuntimeErrors,
                                batch.errors.lost,
                            ),
                        ] {
                            if lost > 0 {
                                journal.publish(
                                    context.clone(),
                                    DebugEvent::DiagnosticLoss { stream, lost },
                                );
                            }
                        }
                        let mut entries: Vec<_> = batch
                            .console
                            .entries
                            .into_iter()
                            .map(|entry| (false, entry))
                            .chain(batch.errors.entries.into_iter().map(|entry| (true, entry)))
                            .collect();
                        entries.sort_by_key(|(_, entry)| entry.order);
                        for (error, entry) in entries {
                            let event = if error {
                                DebugEvent::RuntimeError(
                                    blitz_control_protocol::RuntimeErrorEntry {
                                        sequence: entry.sequence,
                                        message: entry.message,
                                        stack: entry.stack,
                                        revisions: blitz_control_protocol::RevisionSet {
                                            document: state.control.revision(),
                                            ..Default::default()
                                        },
                                    },
                                )
                            } else {
                                DebugEvent::Console(blitz_control_protocol::ConsoleEntry {
                                    sequence: entry.sequence,
                                    level: entry.level,
                                    message: entry.message,
                                })
                            };
                            journal.publish(context.clone(), event);
                        }
                        DebugResponse::Ack
                    },
                );
            }
        }
    }
}

#[cfg(all(feature = "agent-control", unix))]
impl RuntimeApplication {
    fn handle_targeted_control(
        &mut self,
        event_loop: &dyn ActiveEventLoop,
        request: TargetRequest,
    ) -> DebugResponse {
        let quit = matches!(
            &request.command,
            TargetCommand::Agent(AgentControlRequest::Quit)
        );
        let relaunch = matches!(
            &request.command,
            TargetCommand::Agent(AgentControlRequest::Relaunch)
        );
        let delegated = blitz_control_protocol::lifecycle::lifecycle_handler().is_some();
        let response = self.handle_targeted(request);
        if matches!(response.unscoped(), DebugResponse::Ack) && (quit || (relaunch && !delegated)) {
            event_loop.exit();
        }
        response
    }

    fn handle_runtime_request(
        &mut self,
        event_loop: &dyn ActiveEventLoop,
        request: ControlBridgeRequest,
    ) -> DebugResponse {
        match request {
            ControlBridgeRequest::Targeted(request) => {
                self.handle_targeted_control(event_loop, request)
            }
            ControlBridgeRequest::Agent(AgentControlRequest::Quit) => {
                event_loop.exit();
                DebugResponse::Ack
            }
            ControlBridgeRequest::Agent(AgentControlRequest::Relaunch) => {
                let delegated = blitz_control_protocol::lifecycle::lifecycle_handler().is_some();
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
            ControlBridgeRequest::Diagnostics(request) => self.handle_builtin_diagnostics(request),
        }
    }

    fn drain_runtime_messages(&mut self, event_loop: &dyn ActiveEventLoop) {
        while let Ok(message) = self.receiver.try_recv() {
            match message {
                RuntimeMessage::Control { request, response } => {
                    let result = self.handle_runtime_request(event_loop, request);
                    response.fill(result);
                }
            }
        }
    }
}

impl RuntimeApplication {
    fn surface_marks(&self) -> Vec<(crate::surfaces::PageSurface, u64)> {
        let blitz = self.blitz.borrow();
        crate::surfaces::pages()
            .into_iter()
            .filter_map(|page| {
                let view = blitz
                    .windows
                    .values()
                    .find(|view| view.doc.id() as u64 == page.owner_document_id)?;
                let root = view.doc.inner();
                if page.document_id == page.owner_document_id {
                    return Some((page, root.paint_revision.get()));
                }
                let child = root
                    .get_node(blitz_dom::NodeId::from_u64(page.mount_node_id))?
                    .element_data()?
                    .sub_doc_data()?;
                (child.id() as u64 == page.document_id)
                    .then(|| (page, child.inner().paint_revision.get()))
            })
            .collect()
    }

    fn observe_surfaces(
        &mut self,
        painted: &[(crate::surfaces::PageSurface, u64)],
        submitted: bool,
    ) {
        let pages = crate::surfaces::pages();
        let mut events = Vec::new();
        {
            let mut blitz = self.blitz.borrow_mut();
            self.surface_visibility
                .retain(|page, _| pages.contains(page));
            for page in pages {
                let mut visible = false;
                let mut real_paint = false;
                for view in blitz
                    .windows
                    .values_mut()
                    .filter(|view| view.doc.id() as u64 == page.owner_document_id)
                {
                    let window_visible = !self.surfaces_suspended
                        && view.is_visible
                        && view.window.is_visible().unwrap_or(true)
                        && view.window.is_minimized() != Some(true);
                    let mut root = view.doc.inner_mut();
                    let mount = blitz_dom::NodeId::from_u64(page.mount_node_id);
                    let on_screen = window_visible && root.surface_on_screen(mount);
                    if page.document_id == page.owner_document_id {
                        visible |= on_screen;
                        let revision = root.paint_revision.get();
                        real_paint |= on_screen
                            && submitted
                            && painted
                                .iter()
                                .any(|(old, before)| *old == page && revision > *before);
                    } else if let Some(child) = root.subdoc_mut(mount) {
                        if child.id() as u64 != page.document_id {
                            continue;
                        }
                        visible |= on_screen;
                        let revision = child.inner().paint_revision.get();
                        real_paint |= on_screen
                            && submitted
                            && painted
                                .iter()
                                .any(|(old, before)| *old == page && revision > *before);
                        child.set_page_visible(on_screen);
                    }
                }
                if self.surface_visibility.insert(page, visible) != Some(visible) {
                    events.push(crate::surfaces::SurfaceEvent::Visibility { page, visible });
                }
                if real_paint {
                    events.push(crate::surfaces::SurfaceEvent::Painted { page });
                }
            }
        }
        for event in events {
            crate::surfaces::publish(event);
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
        self.surfaces_suspended = false;
        self.observe_surfaces(&[], false);
        #[cfg(all(feature = "agent-control", unix))]
        self.publish_target_events();
        runtime_trace("runtime can_create_surfaces completed");
    }

    fn destroy_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        self.blitz.get_mut().destroy_surfaces(event_loop);
    }

    fn resumed(&mut self, event_loop: &dyn ActiveEventLoop) {
        runtime_trace("runtime resumed entered");
        self.blitz.get_mut().resumed(event_loop);
        self.surfaces_suspended = false;
        self.observe_surfaces(&[], false);
        runtime_trace("runtime resumed completed");
    }

    fn suspended(&mut self, event_loop: &dyn ActiveEventLoop) {
        self.surfaces_suspended = true;
        self.observe_surfaces(&[], false);
        self.blitz.get_mut().suspended(event_loop);
    }

    fn window_event(
        &mut self,
        event_loop: &dyn ActiveEventLoop,
        window_id: winit::window::WindowId,
        event: WinitWindowEvent,
    ) {
        let marks = self.surface_marks();
        let before = self
            .blitz
            .borrow()
            .windows
            .get(&window_id)
            .map(|view| view.submitted_paint);
        self.blitz
            .get_mut()
            .window_event(event_loop, window_id, event);
        let paint_committed = before.is_some_and(|before| {
            self.blitz
                .borrow()
                .windows
                .get(&window_id)
                .is_some_and(|view| view.submitted_paint > before)
        });
        self.observe_surfaces(&marks, paint_committed);
        #[cfg(all(feature = "agent-control", unix))]
        self.publish_target_events();
        #[cfg(all(feature = "agent-control", unix))]
        if paint_committed && agent_control_enabled() {
            self.paint_revision = self.paint_revision.saturating_add(1);
            if let Some(context) = self.chrome_context() {
                self.control_events.publish(
                    context,
                    DebugEvent::PaintCommitted {
                        revision: self.paint_revision,
                    },
                );
            }
            for page in &self.targets.pages {
                if page.info.active
                    && page.info.capabilities.agent
                    && let Some(document_id) = page.info.document_id
                {
                    self.control_events.publish(
                        TargetContext {
                            target_id: page.info.id.clone(),
                            generation: page.info.generation,
                            document_id,
                        },
                        DebugEvent::PaintCommitted {
                            revision: self.paint_revision,
                        },
                    );
                }
            }
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
        #[cfg(all(feature = "agent-control", unix))]
        self.publish_target_events();
    }

    fn about_to_wait(&mut self, event_loop: &dyn ActiveEventLoop) {
        #[cfg(all(feature = "agent-control", unix))]
        self.drain_runtime_messages(event_loop);
        if event_loop.exiting() {
            return;
        }
        self.blitz.get_mut().about_to_wait(event_loop);
        self.observe_surfaces(&[], false);
        #[cfg(all(feature = "agent-control", unix))]
        self.publish_target_events();
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
        // One bounded ordered journal, with a cursor owned by each connection.
        let event_sender = DiagnosticJournal::new(4096);
        let event_receiver = Arc::clone(&event_sender);
        // The embedder's persisted control state is applied during setup.
        // The CLI rescue starts earlier so QA can attach during startup.
        let server = if std::env::args().any(|arg| arg == "--blitz-control") {
            Some(AgentControlServer::start_with_journal(
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
    let _host = register_window(&handle, &mut blitz, app.window_spec())?;
    let setup_result = Arc::new(Mutex::new(None));
    let application = RuntimeApplication {
        blitz: RefCell::new(blitz),
        #[cfg(all(feature = "agent-control", unix))]
        host: _host,
        app,
        setup_result: Arc::clone(&setup_result),
        #[cfg(all(feature = "agent-control", unix))]
        receiver,
        ready: false,
        surfaces_suspended: true,
        surface_visibility: Default::default(),
        #[cfg(all(feature = "agent-control", unix))]
        _agent_control: agent_control,
        #[cfg(all(feature = "agent-control", unix))]
        targets: crate::targets::TargetRegistry::default(),
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
        .args(
            std::env::args_os()
                .skip(1)
                .filter(|arg| arg != std::ffi::OsStr::new("--blitz-control")),
        )
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
) -> AppResult<Arc<dyn DocumentHost>> {
    runtime_trace("native window registration entered");
    let ApplicationDocument { mut document, host } = create_document(&config.url)?;
    attach_ipc_handler_for_url(
        document.as_mut(),
        Arc::clone(&host),
        handle.clone(),
        url::Url::parse(&config.url).ok(),
    );

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
    let window = BlitzShellWindowConfig::with_attributes(document, renderer, attributes)
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
    Ok(host)
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
