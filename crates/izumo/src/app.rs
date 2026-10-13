use std::any::{Any, TypeId, type_name};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use serde::Serialize;
use serde_json::Value;
use winit::window::Window;

use crate::ScriptQueue;

pub use winit::window::Theme;

/// Errors from application configuration, setup and the native event loop.
pub type AppResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync + 'static>>;

/// A command consumes its JSON arguments and returns a JSON reply or rejection.
pub type CommandResult = Result<Value, String>;

/// Shared command implementation. Capture managed state by cloning its Arc.
pub type CommandHandler = Arc<dyn Fn(Value) -> CommandResult + Send + Sync + 'static>;

/// Native title bar presentation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TitleBarStyle {
    #[default]
    Visible,
    Transparent,
    Overlay,
}

/// Native window settings, independent of an application configuration file.
///
/// Dimensions and positions are logical pixels. Background color is RGBA.
/// The default starts hidden; call AppHandle::show from setup to reveal it.
#[derive(Debug, Clone)]
pub struct WindowSpec {
    pub label: String,
    pub identifier: String,
    pub url: String,
    pub title: String,
    pub width: f64,
    pub height: f64,
    pub min_width: Option<f64>,
    pub min_height: Option<f64>,
    pub max_width: Option<f64>,
    pub max_height: Option<f64>,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub resizable: bool,
    pub maximizable: bool,
    pub minimizable: bool,
    pub closable: bool,
    pub fullscreen: bool,
    pub focus: bool,
    pub focusable: bool,
    pub maximized: bool,
    pub visible: bool,
    pub transparent: bool,
    pub decorations: bool,
    pub always_on_bottom: bool,
    pub always_on_top: bool,
    pub content_protected: bool,
    pub background_color: Option<[u8; 4]>,
    pub theme: Option<Theme>,
    pub title_bar_style: TitleBarStyle,
    pub hidden_title: bool,
    pub no_redirection_bitmap: bool,
}

impl Default for WindowSpec {
    fn default() -> Self {
        Self {
            label: "main".into(),
            identifier: "io.pathscale.izumo".into(),
            url: "about:blank".into(),
            title: "izumo".into(),
            width: 1440.0,
            height: 960.0,
            min_width: Some(960.0),
            min_height: Some(640.0),
            max_width: None,
            max_height: None,
            x: None,
            y: None,
            resizable: true,
            maximizable: true,
            minimizable: true,
            closable: true,
            fullscreen: false,
            focus: true,
            focusable: true,
            maximized: false,
            visible: false,
            transparent: false,
            decorations: true,
            always_on_bottom: false,
            always_on_top: false,
            content_protected: false,
            background_color: Some([9, 13, 18, 255]),
            theme: None,
            title_bar_style: TitleBarStyle::Overlay,
            hidden_title: true,
            no_redirection_bitmap: false,
        }
    }
}

impl WindowSpec {
    fn validate(&self) -> Result<(), String> {
        if self.label.trim().is_empty()
            || self.identifier.trim().is_empty()
            || self.url.trim().is_empty()
        {
            return Err("window label, identifier and URL must be nonempty".into());
        }
        for (name, value) in [("width", self.width), ("height", self.height)] {
            if !value.is_finite() || value <= 0.0 {
                return Err(format!("window {name} must be finite and positive"));
            }
        }
        for (name, value) in [
            ("min_width", self.min_width),
            ("min_height", self.min_height),
            ("max_width", self.max_width),
            ("max_height", self.max_height),
        ] {
            if value.is_some_and(|value| !value.is_finite() || value <= 0.0) {
                return Err(format!("window {name} must be finite and positive"));
            }
        }
        for (name, value) in [("x", self.x), ("y", self.y)] {
            if value.is_some_and(|value| !value.is_finite()) {
                return Err(format!("window {name} must be finite"));
            }
        }
        for (name, minimum, maximum) in [
            ("width", self.min_width, self.max_width),
            ("height", self.min_height, self.max_height),
        ] {
            if let (Some(minimum), Some(maximum)) = (minimum, maximum)
                && minimum > maximum
            {
                return Err(format!("window minimum {name} exceeds maximum {name}"));
            }
        }
        Ok(())
    }
}

type ManagedState = Arc<dyn Any + Send + Sync>;
type Setup = Box<dyn FnOnce(&AppHandle) -> AppResult + 'static>;

struct AppShared {
    states: RwLock<HashMap<TypeId, ManagedState>>,
    commands: RwLock<HashMap<String, CommandHandler>>,
    scripts: ScriptQueue,
    window: Mutex<WindowSpec>,
    native: Mutex<Option<Arc<dyn Window>>>,
}

/// Thread-safe access to state, commands and the document-thread queue.
///
/// Managed state is held for the application lifetime. Its persistence remains
/// the embedder's responsibility.
#[derive(Clone)]
pub struct AppHandle {
    inner: Arc<AppShared>,
}

impl AppHandle {
    fn new() -> Self {
        Self {
            inner: Arc::new(AppShared {
                states: RwLock::new(HashMap::new()),
                commands: RwLock::new(HashMap::new()),
                scripts: ScriptQueue::default(),
                window: Mutex::new(WindowSpec::default()),
                native: Mutex::new(None),
            }),
        }
    }

    /// Retrieve the same Arc registered with AppBuilder::manage.
    pub fn state<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        let state = self
            .inner
            .states
            .read()
            .unwrap()
            .get(&TypeId::of::<T>())
            .cloned()?;
        state.downcast::<T>().ok()
    }

    /// Dispatch a command without holding the command table lock.
    ///
    /// The IPC bridge uses this method and queues the resulting reply.
    pub fn invoke(&self, name: &str, args: Value) -> CommandResult {
        let handler = self.inner.commands.read().unwrap().get(name).cloned();
        match handler {
            Some(handler) => handler(args),
            None => Err(format!("unknown izumo command: {name}")),
        }
    }

    /// Queue an event for the injected __IZUMO__ listener shim.
    pub fn emit<T: Serialize>(&self, event: &str, payload: T) -> AppResult {
        let event = serde_json::to_string(event)?;
        let payload = serde_json::to_string(&payload)?;
        self.inner
            .scripts
            .enqueue(format!("window.__IZUMO__._emit({event},{payload});"));
        Ok(())
    }

    /// Queue work for the UI and Boa document thread.
    ///
    /// This is asynchronous, including calls made from that thread. Callers
    /// must not block waiting for the queued task.
    pub fn run_on_main_thread(&self, task: impl FnOnce() + Send + 'static) -> AppResult {
        self.inner.scripts.enqueue_task(task);
        Ok(())
    }

    /// Reveal the window, including when native creation is still pending.
    pub fn show(&self) -> AppResult {
        self.set_visible(true)
    }

    /// Hide the window, including when native creation is still pending.
    pub fn hide(&self) -> AppResult {
        self.set_visible(false)
    }

    fn set_visible(&self, visible: bool) -> AppResult {
        self.inner.window.lock().unwrap().visible = visible;
        let shared = Arc::downgrade(&self.inner);
        self.run_on_main_thread(move || {
            let Some(shared) = shared.upgrade() else {
                return;
            };
            let native = shared.native.lock().unwrap().clone();
            if let Some(native) = native {
                let visible = shared.window.lock().unwrap().visible;
                native.set_visible(visible);
            }
        })
    }

    /// Snapshot the current settings, including visibility requested by setup.
    pub fn window_spec(&self) -> WindowSpec {
        self.inner.window.lock().unwrap().clone()
    }

    /// Shared ingress attached to the chrome document by the runtime.
    pub fn script_queue(&self) -> &ScriptQueue {
        &self.inner.scripts
    }

    /// Called on the UI thread from the shell's on_created callback.
    pub(crate) fn bind_native_window(&self, native: Arc<dyn Window>) {
        let visible = self.inner.window.lock().unwrap().visible;
        native.set_visible(visible);
        *self.inner.native.lock().unwrap() = Some(native);
    }

    /// Release the native window when the event loop exits.
    pub(crate) fn clear_native_window(&self) {
        self.inner.native.lock().unwrap().take();
    }
}

/// Application configuration consumed once by the native event loop.
pub struct App {
    handle: AppHandle,
    setup: Option<Setup>,
}

impl App {
    pub fn builder() -> AppBuilder {
        AppBuilder::default()
    }

    pub fn handle(&self) -> AppHandle {
        self.handle.clone()
    }

    pub fn window_spec(&self) -> WindowSpec {
        self.handle.window_spec()
    }

    /// Create the chrome window and drive its native event loop.
    pub fn run(self) -> AppResult {
        crate::runtime::run(self)
    }

    /// The runtime calls setup once after initializing its control surface and
    /// registering the document, before entering the document's poll cycle.
    pub(crate) fn run_setup(&mut self) -> AppResult {
        if let Some(setup) = self.setup.take() {
            setup(&self.handle)?;
        }
        Ok(())
    }
}

/// Registers application state, JSON commands and startup behavior.
pub struct AppBuilder {
    handle: AppHandle,
    setup: Option<Setup>,
    error: Option<String>,
}

impl Default for AppBuilder {
    fn default() -> Self {
        Self {
            handle: AppHandle::new(),
            setup: None,
            error: None,
        }
    }
}

impl AppBuilder {
    /// Share an existing state Arc without adding another Arc layer.
    ///
    /// Register each concrete state type once. Commands can capture a clone
    /// of this Arc; setup can retrieve it through AppHandle::state.
    pub fn manage<T: Any + Send + Sync>(mut self, state: Arc<T>) -> Self {
        let state: ManagedState = state;
        let duplicate = self
            .handle
            .inner
            .states
            .write()
            .unwrap()
            .insert(TypeId::of::<T>(), state)
            .is_some();
        if duplicate {
            self.error
                .get_or_insert_with(|| format!("duplicate managed state: {}", type_name::<T>()));
        }
        self
    }

    pub fn command(
        mut self,
        name: impl Into<String>,
        handler: impl Fn(Value) -> CommandResult + Send + Sync + 'static,
    ) -> Self {
        self.register_command(name.into(), Arc::new(handler));
        self
    }

    /// Register a collection of shared handlers with distinct names.
    pub fn commands<I, S>(mut self, handlers: I) -> Self
    where
        I: IntoIterator<Item = (S, CommandHandler)>,
        S: Into<String>,
    {
        for (name, handler) in handlers {
            self.register_command(name.into(), handler);
        }
        self
    }

    fn register_command(&mut self, name: String, handler: CommandHandler) {
        if name.trim().is_empty() {
            self.error
                .get_or_insert_with(|| "command name must be nonempty".into());
            return;
        }
        let duplicate = self
            .handle
            .inner
            .commands
            .write()
            .unwrap()
            .insert(name.clone(), handler)
            .is_some();
        if duplicate {
            self.error
                .get_or_insert_with(|| format!("duplicate command: {name}"));
        }
    }

    pub fn window(self, spec: WindowSpec) -> Self {
        *self.handle.inner.window.lock().unwrap() = spec;
        self
    }

    /// Run once on the UI thread. Return an error to abort startup.
    pub fn setup(mut self, setup: impl FnOnce(&AppHandle) -> AppResult + 'static) -> Self {
        self.setup = Some(Box::new(setup));
        self
    }

    pub fn handle(&self) -> AppHandle {
        self.handle.clone()
    }

    pub fn build(self) -> AppResult<App> {
        if let Some(error) = self.error {
            return Err(std::io::Error::other(error).into());
        }
        self.handle
            .window_spec()
            .validate()
            .map_err(std::io::Error::other)?;
        Ok(App {
            handle: self.handle,
            setup: self.setup,
        })
    }

    pub fn run(self) -> AppResult {
        self.build()?.run()
    }
}
