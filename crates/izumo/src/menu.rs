use std::{collections::HashMap, rc::Rc};

use crate::AppBuilder;

/// Stable identifiers for the built-in menu items.
pub mod menu_id {
    pub const ABOUT: &str = "app-about";
    pub const HIDE: &str = "app-hide";
    pub const QUIT: &str = "app-quit";
    pub const UNDO: &str = "edit-undo";
    pub const REDO: &str = "edit-redo";
    pub const CUT: &str = "edit-cut";
    pub const COPY: &str = "edit-copy";
    pub const PASTE: &str = "edit-paste";
    pub const SELECT_ALL: &str = "edit-select-all";
    pub const VIEW_SOURCE: &str = "menu-view-source";
    pub const MINIMIZE: &str = "window-minimize";
    pub const CLOSE: &str = "window-close";
}

/// The standard application menu bar and callbacks for its items.
///
/// On macOS, install this on the main thread with [`AppBuilder::menu`]. Other
/// platforms currently leave the native menu bar unchanged.
pub struct MenuBar {
    application_name: String,
    handlers: HashMap<String, Rc<dyn Fn() + 'static>>,
    overrides: HashMap<String, Rc<dyn Fn() + 'static>>,
}

impl MenuBar {
    /// Create the standard App, Edit, View and Window menus.
    pub fn new(application_name: impl Into<String>) -> Self {
        Self {
            application_name: application_name.into(),
            handlers: HashMap::new(),
            overrides: HashMap::new(),
        }
    }

    /// Register a callback for a built-in menu item identifier.
    ///
    /// Callbacks run on the native UI thread. The built-in action, such as
    /// hiding the app or forwarding an Edit key event, runs before its
    /// callback.
    pub fn on_menu_event(
        mut self,
        id: impl Into<String>,
        handler: impl Fn() + 'static,
    ) -> Self {
        self.handlers.insert(id.into(), Rc::new(handler));
        self
    }

    /// Register a callback that replaces a built-in menu action.
    ///
    /// Callbacks run on the native UI thread. When an override is registered,
    /// its callback runs instead of the built-in action and any `on_menu_event`
    /// callback for that identifier.
    pub fn on_menu_override(
        mut self,
        id: impl Into<String>,
        handler: impl Fn() + 'static,
    ) -> Self {
        self.overrides.insert(id.into(), Rc::new(handler));
        self
    }

    fn install(self) -> Result<(), String> {
        #[cfg(target_os = "macos")]
        {
            return macos::install(self);
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = self;
            Ok(())
        }
    }
}

impl Default for MenuBar {
    fn default() -> Self {
        Self::new(env!("CARGO_PKG_NAME"))
    }
}

impl AppBuilder {
    /// Install the native menu bar.
    ///
    /// On macOS, call this while building the app on the main thread. Menu
    /// callbacks are registered by their string identifiers with
    /// [`MenuBar::on_menu_event`] or [`MenuBar::on_menu_override`].
    pub fn menu(self, menu: MenuBar) -> Self {
        if let Err(error) = menu.install() {
            eprintln!("izumo: unable to install native menu: {error}");
        }
        self
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::{cell::RefCell, ffi::CStr};

    use objc2::{
        define_class, msg_send, sel, DefinedClass, MainThreadOnly,
        rc::Retained,
        runtime::{AnyClass, AnyObject},
    };
    use objc2_foundation::{MainThreadMarker, NSObject, NSObjectProtocol, NSPoint, NSString};

    use super::{MenuBar, menu_id};

    const MOD_COMMAND: usize = 1 << 20;
    const MOD_SHIFT: usize = 1 << 17;
    const EVENT_KEY_DOWN: usize = 10;
    const EVENT_KEY_UP: usize = 11;
    const EVENT_FLAGS_CHANGED: usize = 12;
    const KEYCODE_COMMAND: u16 = 55;
    const KEYCODE_SHIFT: u16 = 56;

    struct MenuActionIvars {
        action: Box<dyn Fn() + 'static>,
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[name = "IzumoMenuAction"]
        #[thread_kind = MainThreadOnly]
        #[ivars = MenuActionIvars]
        struct MenuAction;

        unsafe impl NSObjectProtocol for MenuAction {}

        impl MenuAction {
            #[unsafe(method(performMenuAction:))]
            fn perform_menu_action(&self, _sender: &AnyObject) {
                (self.ivars().action)();
            }
        }
    );

    thread_local! {
        static ACTION_TARGETS: RefCell<Vec<Retained<MenuAction>>> = RefCell::new(Vec::new());
    }

    #[derive(Clone, Copy)]
    enum EditAction {
        Undo,
        Redo,
        Cut,
        Copy,
        Paste,
        SelectAll,
    }

    #[derive(Clone, Copy)]
    enum BuiltinAction {
        About,
        Hide,
        Quit,
        Edit(EditAction),
        ViewSource,
        Minimize,
        Close,
    }

    impl MenuBar {
        fn callback(&self, id: &'static str, action: BuiltinAction) -> Box<dyn Fn() + 'static> {
            let callback = self.handlers.get(id).cloned();
            let override_callback = self.overrides.get(id).cloned();
            Box::new(move || {
                if let Some(callback) = &override_callback {
                    callback();
                    return;
                }
                perform_builtin(action);
                if let Some(callback) = &callback {
                    callback();
                }
            })
        }
    }

    pub(super) fn install(menu_bar: MenuBar) -> Result<(), String> {
        let main_thread = MainThreadMarker::new()
            .ok_or_else(|| "menu installation must run on the main thread".to_string())?;
        let app = shared_application()?;
        let main_menu = new_object(c"NSMenu")?;
        set_title(&main_menu, "Main Menu");

        let app_menu = new_object(c"NSMenu")?;
        set_title(&app_menu, &menu_bar.application_name);
        add_action_item(
            &app_menu,
            &main_thread,
            &menu_bar,
            &format!("About {}", menu_bar.application_name),
            menu_id::ABOUT,
            "",
            0,
            BuiltinAction::About,
        )?;
        add_action_item(
            &app_menu,
            &main_thread,
            &menu_bar,
            "Hide",
            menu_id::HIDE,
            "h",
            MOD_COMMAND,
            BuiltinAction::Hide,
        )?;
        add_separator(&app_menu)?;
        add_action_item(
            &app_menu,
            &main_thread,
            &menu_bar,
            "Quit",
            menu_id::QUIT,
            "q",
            MOD_COMMAND,
            BuiltinAction::Quit,
        )?;
        add_submenu(&main_menu, &menu_bar.application_name, &app_menu)?;

        let edit_menu = new_object(c"NSMenu")?;
        set_title(&edit_menu, "Edit");
        add_action_item(
            &edit_menu,
            &main_thread,
            &menu_bar,
            "Undo",
            menu_id::UNDO,
            "z",
            MOD_COMMAND,
            BuiltinAction::Edit(EditAction::Undo),
        )?;
        add_action_item(
            &edit_menu,
            &main_thread,
            &menu_bar,
            "Redo",
            menu_id::REDO,
            "Z",
            MOD_COMMAND | MOD_SHIFT,
            BuiltinAction::Edit(EditAction::Redo),
        )?;
        add_separator(&edit_menu)?;
        add_action_item(
            &edit_menu,
            &main_thread,
            &menu_bar,
            "Cut",
            menu_id::CUT,
            "x",
            MOD_COMMAND,
            BuiltinAction::Edit(EditAction::Cut),
        )?;
        add_action_item(
            &edit_menu,
            &main_thread,
            &menu_bar,
            "Copy",
            menu_id::COPY,
            "c",
            MOD_COMMAND,
            BuiltinAction::Edit(EditAction::Copy),
        )?;
        add_action_item(
            &edit_menu,
            &main_thread,
            &menu_bar,
            "Paste",
            menu_id::PASTE,
            "v",
            MOD_COMMAND,
            BuiltinAction::Edit(EditAction::Paste),
        )?;
        add_action_item(
            &edit_menu,
            &main_thread,
            &menu_bar,
            "Select All",
            menu_id::SELECT_ALL,
            "a",
            MOD_COMMAND,
            BuiltinAction::Edit(EditAction::SelectAll),
        )?;
        add_submenu(&main_menu, "Edit", &edit_menu)?;

        let view_menu = new_object(c"NSMenu")?;
        set_title(&view_menu, "View");
        add_action_item(
            &view_menu,
            &main_thread,
            &menu_bar,
            "View Source",
            menu_id::VIEW_SOURCE,
            "u",
            MOD_COMMAND,
            BuiltinAction::ViewSource,
        )?;
        add_submenu(&main_menu, "View", &view_menu)?;

        let window_menu = new_object(c"NSMenu")?;
        set_title(&window_menu, "Window");
        add_action_item(
            &window_menu,
            &main_thread,
            &menu_bar,
            "Minimize",
            menu_id::MINIMIZE,
            "m",
            MOD_COMMAND,
            BuiltinAction::Minimize,
        )?;
        add_separator(&window_menu)?;
        add_action_item(
            &window_menu,
            &main_thread,
            &menu_bar,
            "Close",
            menu_id::CLOSE,
            "w",
            MOD_COMMAND,
            BuiltinAction::Close,
        )?;
        add_submenu(&main_menu, "Window", &window_menu)?;

        unsafe {
            let _: () = msg_send![&*app, setWindowsMenu: &*window_menu];
            let _: () = msg_send![&*app, setMainMenu: &*main_menu];
        }
        Ok(())
    }

    fn shared_application() -> Result<Retained<AnyObject>, String> {
        let class = AnyClass::get(c"NSApplication")
            .ok_or_else(|| "NSApplication is unavailable".to_string())?;
        let app: Retained<AnyObject> = unsafe { msg_send![class, sharedApplication] };
        Ok(app)
    }

    fn new_object(class_name: &'static CStr) -> Result<Retained<AnyObject>, String> {
        let class = AnyClass::get(class_name)
            .ok_or_else(|| format!("{} is unavailable", class_name.to_string_lossy()))?;
        let object: Retained<AnyObject> = unsafe { msg_send![class, new] };
        Ok(object)
    }

    fn set_title(object: &AnyObject, title: &str) {
        let title = NSString::from_str(title);
        unsafe {
            let _: () = msg_send![object, setTitle: &*title];
        }
    }

    fn add_submenu(
        parent: &AnyObject,
        title: &str,
        submenu: &AnyObject,
    ) -> Result<(), String> {
        let item = new_object(c"NSMenuItem")?;
        set_title(&item, title);
        unsafe {
            let _: () = msg_send![&*item, setSubmenu: submenu];
            let _: () = msg_send![parent, addItem: &*item];
        }
        Ok(())
    }

    fn add_separator(menu: &AnyObject) -> Result<(), String> {
        let class = AnyClass::get(c"NSMenuItem")
            .ok_or_else(|| "NSMenuItem is unavailable".to_string())?;
        let item: Retained<AnyObject> = unsafe { msg_send![class, separatorItem] };
        unsafe {
            let _: () = msg_send![menu, addItem: &*item];
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn add_action_item(
        menu: &AnyObject,
        main_thread: &MainThreadMarker,
        menu_bar: &MenuBar,
        title: &str,
        id: &'static str,
        key_equivalent: &str,
        modifiers: usize,
        action: BuiltinAction,
    ) -> Result<(), String> {
        let item = new_object(c"NSMenuItem")?;
        set_title(&item, title);
        let key_equivalent = NSString::from_str(key_equivalent);
        unsafe {
            let _: () = msg_send![&*item, setKeyEquivalent: &*key_equivalent];
            let _: () = msg_send![&*item, setKeyEquivalentModifierMask: modifiers];
            let _: () = msg_send![&*item, setAction: sel!(performMenuAction:)];
        }

        let target = action_target(main_thread, menu_bar.callback(id, action));
        let target_ptr = Retained::as_ptr(&target);
        unsafe {
            let _: () = msg_send![&*item, setTarget: target_ptr];
            let _: () = msg_send![menu, addItem: &*item];
        }
        ACTION_TARGETS.with(|targets| targets.borrow_mut().push(target));
        Ok(())
    }

    fn action_target(
        main_thread: &MainThreadMarker,
        action: Box<dyn Fn() + 'static>,
    ) -> Retained<MenuAction> {
        let this = main_thread.alloc::<MenuAction>().set_ivars(MenuActionIvars { action });
        unsafe { msg_send![super(this), init] }
    }

    fn perform_builtin(action: BuiltinAction) {
        match action {
            BuiltinAction::About => send_application_action(ApplicationAction::About),
            BuiltinAction::Hide => send_application_action(ApplicationAction::Hide),
            BuiltinAction::Quit => send_application_action(ApplicationAction::Quit),
            BuiltinAction::Edit(action) => dispatch_edit_key(action),
            BuiltinAction::ViewSource => {}
            BuiltinAction::Minimize => send_window_action(WindowAction::Minimize),
            BuiltinAction::Close => send_window_action(WindowAction::Close),
        }
    }

    enum ApplicationAction {
        About,
        Hide,
        Quit,
    }

    fn send_application_action(action: ApplicationAction) {
        let Ok(app) = shared_application() else {
            return;
        };
        unsafe {
            let sender = std::ptr::null_mut::<AnyObject>();
            match action {
                ApplicationAction::About => {
                    let _: () = msg_send![&*app, orderFrontStandardAboutPanel: sender];
                }
                ApplicationAction::Hide => {
                    let _: () = msg_send![&*app, hide: sender];
                }
                ApplicationAction::Quit => {
                    let _: () = msg_send![&*app, terminate: sender];
                }
            }
        }
    }

    enum WindowAction {
        Minimize,
        Close,
    }

    fn send_window_action(action: WindowAction) {
        let Ok(app) = shared_application() else {
            return;
        };
        let window: Option<Retained<AnyObject>> =
            unsafe { msg_send![&*app, keyWindow] };
        let Some(window) = window else {
            return;
        };
        unsafe {
            let sender = std::ptr::null_mut::<AnyObject>();
            match action {
                WindowAction::Minimize => {
                    let _: () = msg_send![&*window, performMiniaturize: sender];
                }
                WindowAction::Close => {
                    let _: () = msg_send![&*window, performClose: sender];
                }
            }
        }
    }

    fn dispatch_edit_key(action: EditAction) {
        let Ok(app) = shared_application() else {
            return;
        };
        let window: Option<Retained<AnyObject>> =
            unsafe { msg_send![&*app, keyWindow] };
        let Some(window) = window else {
            return;
        };
        let responder: Option<Retained<AnyObject>> =
            unsafe { msg_send![&*window, firstResponder] };
        let Some(responder) = responder else {
            return;
        };
        let window_number: isize = unsafe { msg_send![&*window, windowNumber] };

        let (key, key_code, shift) = match action {
            EditAction::Undo => ("z", 6, false),
            EditAction::Redo => ("Z", 6, true),
            EditAction::Cut => ("x", 7, false),
            EditAction::Copy => ("c", 8, false),
            EditAction::Paste => ("v", 9, false),
            EditAction::SelectAll => ("a", 0, false),
        };
        let empty = NSString::from_str("");
        let characters = NSString::from_str(key);

        send_key_event(
            &responder,
            window_number,
            EVENT_FLAGS_CHANGED,
            MOD_COMMAND,
            KEYCODE_COMMAND,
            &empty,
        );
        if shift {
            send_key_event(
                &responder,
                window_number,
                EVENT_FLAGS_CHANGED,
                MOD_COMMAND | MOD_SHIFT,
                KEYCODE_SHIFT,
                &empty,
            );
        }
        send_key_event(
            &responder,
            window_number,
            EVENT_KEY_DOWN,
            MOD_COMMAND | if shift { MOD_SHIFT } else { 0 },
            key_code,
            &characters,
        );
        send_key_event(
            &responder,
            window_number,
            EVENT_KEY_UP,
            MOD_COMMAND | if shift { MOD_SHIFT } else { 0 },
            key_code,
            &characters,
        );
        if shift {
            send_key_event(
                &responder,
                window_number,
                EVENT_FLAGS_CHANGED,
                MOD_COMMAND,
                KEYCODE_SHIFT,
                &empty,
            );
        }
        send_key_event(
            &responder,
            window_number,
            EVENT_FLAGS_CHANGED,
            0,
            KEYCODE_COMMAND,
            &empty,
        );
    }

    fn send_key_event(
        responder: &AnyObject,
        window_number: isize,
        event_type: usize,
        modifiers: usize,
        key_code: u16,
        characters: &NSString,
    ) {
        let Some(event_class) = AnyClass::get(c"NSEvent") else {
            return;
        };
        let event: Option<Retained<AnyObject>> = unsafe {
            msg_send![
                event_class,
                keyEventWithType: event_type,
                location: NSPoint::new(0.0, 0.0),
                modifierFlags: modifiers,
                timestamp: 0.0_f64,
                windowNumber: window_number,
                context: std::ptr::null_mut::<AnyObject>(),
                characters: characters,
                charactersIgnoringModifiers: characters,
                isARepeat: false,
                keyCode: key_code
            ]
        };
        let Some(event) = event else {
            return;
        };
        unsafe {
            match event_type {
                EVENT_FLAGS_CHANGED => {
                    let _: () = msg_send![responder, flagsChanged: &*event];
                }
                EVENT_KEY_DOWN => {
                    let _: () = msg_send![responder, keyDown: &*event];
                }
                EVENT_KEY_UP => {
                    let _: () = msg_send![responder, keyUp: &*event];
                }
                _ => {}
            }
        }
    }
}

