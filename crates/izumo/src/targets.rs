use std::sync::{Arc, OnceLock, RwLock};

use blitz_control_protocol::in_process::DocumentControl;
use blitz_control_protocol::{DebugResponse, DiagnosticsRequest, TargetContext, TargetInfo};
use blitz_dom::Document;

#[derive(Clone)]
pub struct PageTarget {
    pub info: TargetInfo,
    pub owner_document_id: Option<u64>,
    pub mount_node_id: Option<u64>,
    pub viewport_node_id: Option<u64>,
}

type Provider = dyn Fn() -> Vec<PageTarget> + Send + Sync;
type Navigation = dyn Fn(u64, &str) -> Result<(), String> + Send + Sync;
type Diagnostics = dyn Fn(&mut dyn Document, DiagnosticsRequest) -> DebugResponse + Send + Sync;

static PROVIDER: OnceLock<RwLock<Option<Arc<Provider>>>> = OnceLock::new();
static NAVIGATION: OnceLock<RwLock<Option<Arc<Navigation>>>> = OnceLock::new();
static DIAGNOSTICS: OnceLock<RwLock<Option<Arc<Diagnostics>>>> = OnceLock::new();

pub fn set_page_target_provider(provider: impl Fn() -> Vec<PageTarget> + Send + Sync + 'static) {
    *PROVIDER.get_or_init(|| RwLock::new(None)).write().unwrap() = Some(Arc::new(provider));
}

pub fn set_page_navigation_handler(
    handler: impl Fn(u64, &str) -> Result<(), String> + Send + Sync + 'static,
) {
    *NAVIGATION
        .get_or_init(|| RwLock::new(None))
        .write()
        .unwrap() = Some(Arc::new(handler));
}

pub fn set_page_diagnostics_handler(
    handler: impl Fn(&mut dyn Document, DiagnosticsRequest) -> DebugResponse + Send + Sync + 'static,
) {
    *DIAGNOSTICS
        .get_or_init(|| RwLock::new(None))
        .write()
        .unwrap() = Some(Arc::new(handler));
}

pub(crate) fn has_provider() -> bool {
    PROVIDER
        .get_or_init(|| RwLock::new(None))
        .read()
        .unwrap()
        .is_some()
}

pub(crate) fn pages() -> Vec<PageTarget> {
    let provider = PROVIDER
        .get_or_init(|| RwLock::new(None))
        .read()
        .unwrap()
        .clone();
    provider.map_or_else(Vec::new, |provider| provider())
}

pub(crate) fn navigate(id: u64, url: &str) -> DebugResponse {
    let handler = NAVIGATION
        .get_or_init(|| RwLock::new(None))
        .read()
        .unwrap()
        .clone();
    match handler {
        Some(handler) => match handler(id, url) {
            Ok(()) => DebugResponse::Ack,
            Err(message) => DebugResponse::Error(blitz_control_protocol::DebugError {
                code: "navigationFailed".into(),
                message,
            }),
        },
        None => DebugResponse::Error(blitz_control_protocol::target::error(
            "unsupportedRequest",
            "the host has no page navigation handler",
        )),
    }
}

#[cfg(feature = "diagnostics")]
pub(crate) fn diagnostics(
    document: &mut dyn Document,
    request: DiagnosticsRequest,
) -> DebugResponse {
    let handler = DIAGNOSTICS
        .get_or_init(|| RwLock::new(None))
        .read()
        .unwrap()
        .clone();
    handler.map_or_else(
        || {
            DebugResponse::Error(blitz_control_protocol::target::error(
                "unsupportedRequest",
                "the host has no handler for this page diagnostic",
            ))
        },
        |handler| handler(document, request),
    )
}

pub(crate) struct TargetState {
    pub context: TargetContext,
    pub control: DocumentControl,
    #[cfg(feature = "diagnostics")]
    pub console_sequence: u64,
    #[cfg(feature = "diagnostics")]
    pub error_sequence: u64,
}

impl TargetState {
    pub fn new(context: TargetContext) -> Self {
        Self {
            context,
            control: DocumentControl::new(),
            #[cfg(feature = "diagnostics")]
            console_sequence: 0,
            #[cfg(feature = "diagnostics")]
            error_sequence: 0,
        }
    }
}

#[derive(Default)]
pub(crate) struct TargetRegistry {
    pub pages: Vec<PageTarget>,
    pub states: Vec<TargetState>,
}

impl TargetRegistry {
    pub fn refresh(&mut self, chrome: Option<TargetContext>) {
        self.pages = pages();
        self.states.retain(|state| {
            chrome.as_ref() == Some(&state.context)
                || self.pages.iter().any(|page| {
                    page.info.id == state.context.target_id
                        && page.info.generation == state.context.generation
                        && page.info.document_id == Some(state.context.document_id)
                })
        });
    }

    pub fn state(&mut self, context: &TargetContext) -> &mut TargetState {
        let index = self
            .states
            .iter()
            .position(|state| &state.context == context)
            .unwrap_or_else(|| {
                self.states.push(TargetState::new(context.clone()));
                self.states.len() - 1
            });
        &mut self.states[index]
    }
}
