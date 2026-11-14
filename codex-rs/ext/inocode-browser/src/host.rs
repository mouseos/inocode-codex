//! The CEF side: one browser (Alloy style: fewest processes, 65 13.4), a CDP port on 127.0.0.1, an
//! InoCode-only profile. With `parent_window` (the GUI's browser tab, 65 15.2) the browser is a
//! child window of the GUI's window and links that would open a new window open in it; otherwise it
//! has its own top-level window (Views). Closing it ends the message loop, and with it every process.
use cef::*;
use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};

pub struct Options {
    pub url: String,
    pub port: u16,
    pub cef_dir: PathBuf,
    pub profile: PathBuf,
    pub log_file: PathBuf,
    /// The window is closed when this process disappears (the GUI that started us).
    pub parent_pid: Option<u32>,
    /// The GUI window to live in as a child window (Windows: an HWND). None: a top-level window.
    pub parent_window: Option<isize>,
    /// Close when standard input says `close` or ends (the GUI that started us holds the pipe).
    pub stdin_control: bool,
}

static OPTIONS: OnceLock<Options> = OnceLock::new();
const ENV_CEF_DIR: &str = "INOCODE_BROWSER_CEF_DIR";

/// CEF dir for sub-processes (inherited environment).
pub fn child_cef_dir() -> Option<PathBuf> {
    std::env::var_os(ENV_CEF_DIR).map(PathBuf::from)
}

/// Make `libcef` loadable from the fetched directory. Must run before the first CEF call.
pub fn prepare_library(cef_dir: &std::path::Path) {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let w: Vec<u16> = cef_dir.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        unsafe { windows_sys::Win32::System::LibraryLoader::SetDllDirectoryW(w.as_ptr()) };
    }
    #[cfg(not(windows))]
    let _ = cef_dir;
}

/// Run as the browser process (`options` set) or as a CEF sub-process (`options` None).
pub fn run(options: Option<Options>) -> Result<(), String> {
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    let args = cef::args::Args::new();
    let cmd_line = args.as_cmd_line().ok_or("failed to parse command line")?;
    let is_browser = cmd_line.has_switch(Some(&CefString::from("type"))) != 1;

    let ret = execute_process(Some(args.as_main_args()), None, std::ptr::null_mut());
    if !is_browser {
        return if ret >= 0 { Ok(()) } else { Err("cannot execute non-browser process".into()) };
    }
    if ret != -1 {
        return Err("cannot execute browser process".into());
    }
    let opts = options.ok_or("browser process without options")?;
    let _ = std::fs::create_dir_all(&opts.profile);
    if let Some(p) = opts.log_file.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    std::env::set_var(ENV_CEF_DIR, &opts.cef_dir);

    let s = |p: &std::path::Path| CefString::from(p.to_string_lossy().as_ref());
    let settings = Settings {
        no_sandbox: 1,
        cache_path: s(&opts.profile),
        root_cache_path: s(&opts.profile),
        persist_session_cookies: 1,
        resources_dir_path: s(&opts.cef_dir),
        locales_dir_path: s(&opts.cef_dir.join("locales")),
        log_file: s(&opts.log_file),
        remote_debugging_port: opts.port as i32,
        ..Default::default()
    };
    let parent_pid = opts.parent_pid;
    let stdin_control = opts.stdin_control;
    let _ = OPTIONS.set(opts);

    let mut app = InoApp::new();
    if initialize(Some(args.as_main_args()), Some(&settings), Some(&mut app), std::ptr::null_mut()) != 1 {
        return Err("CEF initialize failed".into());
    }
    if let Some(pid) = parent_pid {
        std::thread::spawn(move || {
            wait_for_exit(pid);
            close_then_exit();
        });
    }
    if stdin_control {
        std::thread::spawn(|| {
            use std::io::BufRead;
            for line in std::io::stdin().lock().lines() {
                match line {
                    Ok(l) if l.trim() == "close" => break,
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
            close_then_exit();
        });
    }
    run_message_loop();
    shutdown();
    Ok(())
}

/// Close the browsers; if the message loop has not ended a few seconds later (an embedded window
/// whose parent is gone may never report its close), end the process. CEF's sub-processes follow.
fn close_then_exit() {
    if let Some(h) = Handler::instance() {
        h.lock().unwrap().close_all_browsers(true);
    }
    std::thread::sleep(std::time::Duration::from_secs(5));
    std::process::exit(0);
}

#[cfg(windows)]
fn wait_for_exit(pid: u32) {
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject, INFINITE, PROCESS_SYNCHRONIZE};
    unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if !h.is_null() {
            WaitForSingleObject(h, INFINITE);
        }
    }
}

#[cfg(not(windows))]
fn wait_for_exit(pid: u32) {
    // Linux: /proc/<pid> disappears when the parent exits. Other Unix: not implemented (unverified).
    let p = format!("/proc/{pid}");
    while std::path::Path::new(&p).exists() {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

// ---- handler -------------------------------------------------------------------------------

static HANDLER: OnceLock<Weak<Mutex<Handler>>> = OnceLock::new();

pub struct Handler {
    browsers: Vec<Browser>,
    weak_self: Weak<Mutex<Self>>,
}

impl Handler {
    fn new() -> Arc<Mutex<Self>> {
        Arc::new_cyclic(|weak| {
            let _ = HANDLER.set(weak.clone());
            Mutex::new(Self { browsers: Vec::new(), weak_self: weak.clone() })
        })
    }

    fn instance() -> Option<Arc<Mutex<Self>>> {
        HANDLER.get().and_then(|w| w.upgrade())
    }

    fn close_all_browsers(&mut self, force: bool) {
        if currently_on(ThreadId::UI) == 0 {
            if let Some(this) = self.weak_self.upgrade() {
                let mut task = CloseAll::new(this, force);
                post_task(ThreadId::UI, Some(&mut task));
            }
            return;
        }
        for b in &self.browsers {
            if let Some(host) = b.host() {
                host.close_browser(force.into());
            }
        }
    }
}

wrap_task! {
    struct CloseAll {
        inner: Arc<Mutex<Handler>>,
        force: bool,
    }
    impl Task {
        fn execute(&self) {
            self.inner.lock().unwrap().close_all_browsers(self.force);
        }
    }
}

wrap_client! {
    struct InoClient {
        inner: Arc<Mutex<Handler>>,
    }
    impl Client {
        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(InoDisplay::new(()))
        }
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(InoLifeSpan::new(self.inner.clone()))
        }
        fn keyboard_handler(&self) -> Option<KeyboardHandler> {
            Some(InoKeyboard::new(()))
        }
    }
}

wrap_display_handler! {
    struct InoDisplay {
        _unit: (),
    }
    impl DisplayHandler {
        fn on_title_change(&self, browser: Option<&mut Browser>, title: Option<&CefString>) {
            let mut browser = browser.cloned();
            if let Some(view) = browser_view_get_for_browser(browser.as_mut()) {
                if let Some(window) = view.window() {
                    window.set_title(title);
                }
            }
        }
    }
}

wrap_life_span_handler! {
    struct InoLifeSpan {
        inner: Arc<Mutex<Handler>>,
    }
    impl LifeSpanHandler {
        fn on_after_created(&self, browser: Option<&mut Browser>) {
            if let Some(b) = browser.cloned() {
                self.inner.lock().unwrap().browsers.push(b);
            }
        }
        // In the tab there is no room for another window: the link opens in the same browser.
        fn on_before_popup(
            &self,
            browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: ::std::os::raw::c_int,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: ::std::os::raw::c_int,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            let embedded = OPTIONS.get().is_some_and(|o| o.parent_window.is_some());
            if !embedded {
                return 0;
            }
            if let (Some(b), Some(url)) = (browser, target_url) {
                if let Some(frame) = b.main_frame() {
                    frame.load_url(Some(url));
                }
            }
            1
        }
        fn do_close(&self, _browser: Option<&mut Browser>) -> i32 {
            0
        }
        fn on_before_close(&self, browser: Option<&mut Browser>) {
            let mut inner = self.inner.lock().unwrap();
            if let Some(mut b) = browser.cloned() {
                if let Some(i) = inner.browsers.iter().position(|e| e.is_same(Some(&mut b)) != 0) {
                    inner.browsers.remove(i);
                }
            }
            if inner.browsers.is_empty() {
                quit_message_loop();
            }
        }
    }
}

// F12 opens DevTools (65 6.3). The key-event OS type differs per platform; Windows only for now.
#[cfg(windows)]
wrap_keyboard_handler! {
    struct InoKeyboard {
        _unit: (),
    }
    impl KeyboardHandler {
        fn on_pre_key_event(
            &self,
            browser: Option<&mut Browser>,
            event: Option<&KeyEvent>,
            _os_event: Option<&mut sys::MSG>,
            _is_keyboard_shortcut: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            const VK_F12: i32 = 0x7B;
            let (Some(browser), Some(event)) = (browser, event) else { return 0 };
            let down = sys::cef_key_event_type_t::from(event.type_) == sys::cef_key_event_type_t::KEYEVENT_RAWKEYDOWN;
            if down && event.windows_key_code == VK_F12 {
                if let Some(host) = browser.host() {
                    host.show_dev_tools(Some(&WindowInfo::default()), None, Some(&BrowserSettings::default()), None);
                }
                return 1;
            }
            0
        }
    }
}

#[cfg(not(windows))]
wrap_keyboard_handler! {
    struct InoKeyboard {
        _unit: (),
    }
    impl KeyboardHandler {}
}

// ---- window / app --------------------------------------------------------------------------

wrap_window_delegate! {
    struct InoWindowDelegate {
        browser_view: RefCell<Option<BrowserView>>,
    }
    impl ViewDelegate {
        fn preferred_size(&self, _view: Option<&mut View>) -> Size {
            Size { width: 1280, height: 800 }
        }
    }
    impl PanelDelegate {}
    impl WindowDelegate {
        fn on_window_created(&self, window: Option<&mut Window>) {
            let bv = self.browser_view.borrow();
            let (Some(window), Some(bv)) = (window, bv.as_ref()) else { return };
            let mut view = View::from(bv);
            window.add_child_view(Some(&mut view));
            window.show();
        }
        fn on_window_destroyed(&self, _window: Option<&mut Window>) {
            *self.browser_view.borrow_mut() = None;
        }
        fn can_close(&self, _window: Option<&mut Window>) -> i32 {
            let bv = self.browser_view.borrow();
            match bv.as_ref().and_then(|b| b.browser()).and_then(|b| b.host()) {
                Some(host) => host.try_close_browser(),
                None => 1,
            }
        }
        fn initial_show_state(&self, _window: Option<&mut Window>) -> ShowState {
            ShowState::NORMAL
        }
        fn window_runtime_style(&self) -> RuntimeStyle {
            RuntimeStyle::ALLOY
        }
    }
}

wrap_browser_view_delegate! {
    struct InoBrowserViewDelegate {
        _unit: (),
    }
    impl ViewDelegate {}
    impl BrowserViewDelegate {
        // target=_blank, window.open and DevTools get their own top-level window.
        fn on_popup_browser_view_created(
            &self,
            _browser_view: Option<&mut BrowserView>,
            popup: Option<&mut BrowserView>,
            _is_devtools: i32,
        ) -> i32 {
            let mut delegate = InoWindowDelegate::new(RefCell::new(popup.cloned()));
            window_create_top_level(Some(&mut delegate));
            1
        }
        fn browser_runtime_style(&self) -> RuntimeStyle {
            RuntimeStyle::ALLOY
        }
    }
}

wrap_app! {
    struct InoApp;
    impl App {
        fn on_before_command_line_processing(
            &self,
            process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            let is_browser = process_type.map(|p| p.to_string().is_empty()).unwrap_or(true);
            if let (true, Some(cl)) = (is_browser, command_line) {
                cl.append_switch(Some(&CefString::from("use-alloy-style")));
                // In the GUI's tab the browser's window is a child of another process's window, and
                // Chromium's own occlusion check takes it for hidden: the page stops painting.
                if OPTIONS.get().is_some_and(|o| o.parent_window.is_some()) {
                    cl.append_switch_with_value(
                        Some(&CefString::from("disable-features")),
                        Some(&CefString::from("CalculateNativeWinOcclusion")),
                    );
                }
            }
        }
        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(InoProcessHandler::new(RefCell::new(None)))
        }
    }
}

wrap_browser_process_handler! {
    struct InoProcessHandler {
        client: RefCell<Option<Client>>,
    }
    impl BrowserProcessHandler {
        fn on_context_initialized(&self) {
            let Some(opts) = OPTIONS.get() else { return };
            let mut client = InoClient::new(Handler::new());
            *self.client.borrow_mut() = Some(client.clone());
            let url = CefString::from(opts.url.as_str());
            #[cfg(windows)]
            if let Some(parent) = opts.parent_window {
                // Placed and sized by the GUI (SetWindowPos on this child window).
                let bounds = Rect { x: 0, y: 0, width: 800, height: 600 };
                let info = WindowInfo { runtime_style: RuntimeStyle::ALLOY, ..Default::default() }
                    .set_as_child(sys::HWND(parent as *mut sys::HWND__), &bounds);
                browser_host_create_browser(
                    Some(&info),
                    Some(&mut client),
                    Some(&url),
                    Some(&BrowserSettings::default()),
                    None,
                    None,
                );
                return;
            }
            let mut delegate = InoBrowserViewDelegate::new(());
            let view = browser_view_create(
                Some(&mut client),
                Some(&url),
                Some(&BrowserSettings::default()),
                None,
                None,
                Some(&mut delegate),
            );
            let mut window_delegate = InoWindowDelegate::new(RefCell::new(view));
            window_create_top_level(Some(&mut window_delegate));
        }
    }
}
