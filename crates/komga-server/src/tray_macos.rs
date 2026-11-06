//! macOS menu bar integration. AppKit requires the status item and its event loop
//! on the main thread, so in a GUI session the main thread runs NSApplication and
//! the server runs on a background thread instead.

use std::cell::RefCell;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use dispatch2::DispatchQueue;
use objc2::runtime::AnyObject;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSAboutPanelOptionApplicationName, NSAboutPanelOptionApplicationVersion,
    NSAboutPanelOptionCredits, NSAboutPanelOptionVersion, NSAlert, NSAlertSecondButtonReturn,
    NSAlertStyle, NSApplication, NSApplicationActivationPolicy, NSColor, NSEvent,
    NSEventModifierFlags, NSEventType, NSFont, NSFontAttributeName, NSForegroundColorAttributeName,
    NSLinkAttributeName, NSMenu, NSMenuItem, NSMutableParagraphStyle,
    NSParagraphStyleAttributeName, NSRunningApplication, NSTextAlignment,
    NSWindowCollectionBehavior,
};
use objc2_foundation::{
    ns_string, NSActivityOptions, NSDictionary, NSMutableAttributedString, NSPoint, NSProcessInfo,
    NSRange, NSString, NSURL,
};
use objc2_service_management::{SMAppService, SMAppServiceStatus};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

const MENU_BAR_ICON_PNG: &[u8] = include_bytes!("../resources/windows/tray-icon.png");

/// Set while the main thread is handed to NSApplication; unset, there is no event
/// loop to own a menu bar item (no graphic access, e.g. SSH or a LaunchDaemon).
static APP_RUNNING: AtomicBool = AtomicBool::new(false);

/// The launchd label of the login item, `packaging/macos/login-agent.plist`.
const LOGIN_AGENT_LABEL: &str = "date.kmworks.server.login";

/// Set once the server listens: a failure before it is a startup error, one
/// after it is a crash for launchd to restart.
static SERVING: AtomicBool = AtomicBool::new(false);

pub fn mark_serving() {
    SERVING.store(true, Ordering::SeqCst);
}

/// Whether the listener came up: after it, an error ends the server rather
/// than a launch, so the startup alert no longer applies.
pub fn serving() -> bool {
    SERVING.load(Ordering::SeqCst)
}

/// Whether launchd started this process as the login item. launchd sets
/// `XPC_SERVICE_NAME` to the job label, but child processes inherit it, so the
/// parent must be launchd as well (`login-agent.sh` execs the server).
fn launched_by_login_agent() -> bool {
    std::env::var_os("XPC_SERVICE_NAME").is_some_and(|name| name == LOGIN_AGENT_LABEL)
        && unsafe { libc::getppid() } == 1
}

/// The login item restarts the server whenever it exits unsuccessfully. A
/// startup failure (invalid config, port in use) would fail the same way on
/// every restart, so it ends with a successful exit instead and stays down.
pub fn startup_failure_exits_cleanly() -> bool {
    launched_by_login_agent() && !SERVING.load(Ordering::SeqCst)
}

/// The login item's copy of a server that is already running. Turning Open at
/// Login on starts the login item right away while the server that turned it on
/// keeps running, so this copy finds the data directory in use. The busy index
/// alone is not enough: it reports any failure to take its lock as busy, even
/// on a filesystem without file locks, so another running instance of the app
/// must confirm it.
pub fn duplicate_of_running_server(error: &anyhow::Error) -> bool {
    launched_by_login_agent()
        && error.chain().any(|cause| {
            cause
                .downcast_ref::<komga_search::Error>()
                .is_some_and(komga_search::Error::index_in_use)
        })
        && another_instance_running()
}

/// Whether another process of this app is running in the user's session.
fn another_instance_running() -> bool {
    let current = NSRunningApplication::currentApplication();
    let Some(bundle_id) = current.bundleIdentifier() else {
        return false;
    };
    let pid = current.processIdentifier();
    NSRunningApplication::runningApplicationsWithBundleIdentifier(&bundle_id)
        .iter()
        .any(|app| app.processIdentifier() != pid)
}

/// A plain launch from Finder, the Dock or Spotlight while Open at Login is on
/// starts the login item instead, so launchd restarts that server after a
/// crash as well. Launches with arguments or from a terminal run as before.
/// True when the login item took over and this process should exit.
pub fn hand_off_to_login_item() -> bool {
    use clap::Parser;
    if std::env::args_os().len() > 1
        || crate::desktop::app_bundle_path().is_none()
        || std::io::stderr().is_terminal()
        || launched_by_login_agent()
    {
        return false;
    }
    let Ok(cli) = crate::config::Cli::try_parse() else {
        return false;
    };
    let Some(service) = login_item_service(&crate::config::config_dir(&cli)) else {
        return false;
    };
    std::process::Command::new("/bin/launchctl")
        .args(["kickstart", &service])
        .status()
        .is_ok_and(|status| status.success())
}

/// The login item's launchd service target when Open at Login is on and runs
/// the server with this data directory, so starting it brings up the same server.
fn login_item_service(config_dir: &Path) -> Option<String> {
    if !autostart_enabled() {
        return None;
    }
    let saved = std::fs::read(login_config_path()?).ok()?;
    (saved == config_dir.as_os_str().as_encoded_bytes())
        .then(|| format!("gui/{}/{LOGIN_AGENT_LABEL}", unsafe { libc::getuid() }))
}

thread_local! {
    static LOGIN_SERVICE: RefCell<Option<objc2::rc::Retained<SMAppService>>> = const { RefCell::new(None) };
}

thread_local! {
    // TrayIcon is main-thread only; it lives here between start() and stop()
    static STATUS_ITEM: RefCell<Option<TrayIcon>> = const { RefCell::new(None) };
    // the menu event handler must be Send+Sync, so the check item (Rc-based)
    // cannot move into it; muda dispatches menu events on the main thread, where
    // the item is reachable again
    static AUTOSTART_ITEM: RefCell<Option<CheckMenuItem>> = const { RefCell::new(None) };
    static UPDATE_ITEM: RefCell<Option<MenuItem>> = const { RefCell::new(None) };
    static TRAY_ICON: RefCell<Option<TrayIcon>> = const { RefCell::new(None) };
    // the badge the icon currently shows, so it is only swapped on a change
    static BADGE: std::cell::Cell<crate::desktop::Badge> =
        const { std::cell::Cell::new(crate::desktop::Badge::None) };
}

pub struct Tray;

/// Folders the menu opens in Finder.
pub struct TrayPaths {
    pub config_dir: PathBuf,
    pub logs_dir: PathBuf,
}

/// Runs `server` to completion. In a GUI session the main thread is handed to
/// NSApplication for the menu bar and `server` runs on its own thread; otherwise
/// `server` runs right here, exactly as on other platforms.
pub fn run_app<F>(server: F) -> anyhow::Result<()>
where
    F: FnOnce() -> anyhow::Result<()> + Send + 'static,
{
    let Some(mtm) = MainThreadMarker::new() else {
        return server();
    };
    if !session_has_graphic_access() {
        return server();
    }

    let app = NSApplication::sharedApplication(mtm);
    // no Dock icon and no visible app menu: the menu bar item is the whole UI
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    install_edit_menu(&app, mtm);

    // a windowless accessory app is eligible for App Nap, which would throttle
    // timers, CPU and I/O while the server is busy; idle system sleep stays allowed
    let process = NSProcessInfo::processInfo();
    let activity = process.beginActivityWithOptions_reason(
        NSActivityOptions::UserInitiatedAllowingIdleSystemSleep,
        ns_string!("KMServer"),
    );

    APP_RUNNING.store(true, Ordering::SeqCst);
    let handle = std::thread::Builder::new()
        .name("server".into())
        .spawn(move || {
            // stops the event loop however the server ends, panics included,
            // so the main thread never waits in app.run() forever
            let _stop = StopAppOnDrop;
            server()
        })?;
    app.run();
    APP_RUNNING.store(false, Ordering::SeqCst);

    unsafe { process.endActivity(&activity) };
    match handle.join() {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    fn SessionGetInfo(session: u32, session_id: *mut u32, attributes: *mut u32) -> i32;
}

/// Whether this process may draw on a display. An SSH login, or a launchd job
/// outside a GUI login, lacks it even while the same user is logged in at the
/// console, where an AppKit status item would otherwise still show up.
fn session_has_graphic_access() -> bool {
    const CALLER_SECURITY_SESSION: u32 = u32::MAX;
    const SESSION_HAS_GRAPHIC_ACCESS: u32 = 0x0010;
    let mut session_id = 0;
    let mut attributes = 0;
    let status =
        unsafe { SessionGetInfo(CALLER_SECURITY_SESSION, &mut session_id, &mut attributes) };
    status == 0 && attributes & SESSION_HAS_GRAPHIC_ACCESS != 0
}

struct StopAppOnDrop;

impl Drop for StopAppOnDrop {
    fn drop(&mut self) {
        DispatchQueue::main().exec_async(|| {
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            let app = NSApplication::sharedApplication(mtm);
            app.stop(None);
            // stop only takes effect after the run loop handles another event
            if let Some(event) = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
                NSEventType::ApplicationDefined,
                NSPoint::new(0.0, 0.0),
                NSEventModifierFlags::empty(),
                0.0,
                0,
                None,
                0,
                0,
                0,
            ) {
                app.postEvent_atStart(&event, true);
            }
        });
    }
}

/// Last-resort error surface for a fatal startup error of KMServer.app opened
/// from Finder, at login or by the updater: with no terminal attached the app
/// would otherwise quit without a word. Terminal launches already see the error
/// printed, and a bare binary or a session without graphic access has nowhere to
/// show an alert.
pub fn show_startup_error(error: &anyhow::Error) {
    if crate::desktop::app_bundle_path().is_none()
        || std::io::stderr().is_terminal()
        || !session_has_graphic_access()
    {
        return;
    }
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    // a menu bar app is never frontmost by itself, so the alert would open behind
    // other windows; activate() needs macOS 14 and the app supports macOS 13
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);

    let alert = NSAlert::new(mtm);
    // on every Space, so it isn't left behind on a full-screen app's Space
    alert.window().setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary,
    );
    alert.setAlertStyle(NSAlertStyle::Warning);
    alert.setMessageText(ns_string!("KMServer couldn't start"));
    alert.setInformativeText(&NSString::from_str(&format!(
        "{error:#}\n\nThe log files have more details."
    )));
    // the first button is the default; Quit describes what happens either way
    alert.addButtonWithTitle(ns_string!("Quit"));
    let logs_dir = startup_logs_dir();
    if logs_dir.is_some() {
        alert.addButtonWithTitle(ns_string!("Show Logs"));
    }
    if alert.runModal() == NSAlertSecondButtonReturn {
        if let Some(dir) = logs_dir {
            // waited on: the process exits right after, which would end a
            // background `open` before it ran
            let _ = std::process::Command::new("/usr/bin/open")
                .arg(dir)
                .status();
        }
    }
}

/// The logs folder of the data directory the failed start used, when file
/// logging got as far as creating it.
fn startup_logs_dir() -> Option<PathBuf> {
    use clap::Parser;
    let cli = crate::config::Cli::try_parse().ok()?;
    let dir = crate::config::config_dir(&cli).join("logs");
    dir.is_dir().then_some(dir)
}

/// Opens the web UI in the default browser.
pub fn open_in_browser(port: u16) {
    open(std::ffi::OsStr::new(&format!("http://localhost:{port}/")));
}

/// Returns None without a running NSApplication (no GUI session) or when the
/// status item cannot be created; the server runs unaffected either way.
pub fn start(
    port: u16,
    paths: TrayPaths,
    shutdown: tokio::sync::watch::Sender<bool>,
    update: crate::service::desktop_update::DesktopUpdate,
) -> Option<Tray> {
    if !APP_RUNNING.load(Ordering::SeqCst) {
        return None;
    }
    let (ready_tx, ready_rx) = mpsc::channel();
    let initial = update.clone();
    let follow = update.clone();
    let changes = update.subscribe();
    DispatchQueue::main().exec_async(move || {
        let result = build(port, paths, shutdown, update).map(|item| {
            STATUS_ITEM.with(|slot| *slot.borrow_mut() = Some(item));
            refresh_update_item(&initial);
        });
        let _ = ready_tx.send(result);
    });
    match ready_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(())) => {
            // an update the daily check finds shows up without opening the menu
            tokio::spawn(crate::desktop::follow_status(
                changes,
                move || {
                    let update = follow.clone();
                    DispatchQueue::main().exec_async(move || refresh_update_item(&update));
                },
                || DispatchQueue::main().exec_async(advance_spinner),
            ));
            Some(Tray)
        }
        Ok(Err(e)) => {
            tracing::warn!("menu bar item disabled: {e}");
            None
        }
        Err(_) => {
            tracing::warn!("menu bar item disabled: initialization timed out");
            None
        }
    }
}

impl Tray {
    /// Removes the menu bar item as soon as serving ends, before the task drain.
    pub fn stop(self) {
        let (done_tx, done_rx) = mpsc::channel();
        DispatchQueue::main().exec_async(move || {
            TRAY_ICON.with(|slot| slot.borrow_mut().take());
            STATUS_ITEM.with(|slot| slot.borrow_mut().take());
            let _ = done_tx.send(());
        });
        let _ = done_rx.recv_timeout(Duration::from_secs(5));
    }
}

fn build(
    port: u16,
    paths: TrayPaths,
    shutdown: tokio::sync::watch::Sender<bool>,
    update: crate::service::desktop_update::DesktopUpdate,
) -> Result<TrayIcon, String> {
    let open_item = MenuItem::with_id("open", "Open KMServer", true, None);
    let config_item = MenuItem::with_id("config", "Open configuration directory", true, None);
    let logs_item = MenuItem::with_id("logs", "Open logs folder", true, None);
    let autostart_item = crate::desktop::app_bundle_path().map(|_| {
        CheckMenuItem::with_id(
            "autostart",
            "Open at Login",
            true,
            autostart_enabled(),
            None,
        )
    });
    let about_item = MenuItem::with_id("about", "About KMServer", true, None);
    let quit_item = MenuItem::with_id("quit", "Quit KMServer", true, None);
    let update_item = crate::desktop::app_bundle_path().map(|_| {
        MenuItem::with_id(
            "update",
            crate::desktop::update_line(&update.status(), None).title,
            true,
            None,
        )
    });
    let menu = Menu::new();
    let _ = menu.append(&open_item);
    let _ = menu.append(&config_item);
    // the log files are where startup and scan errors end up when no terminal is
    // attached, so the menu only offers the folder when file logging came up
    let logs_dir = paths.logs_dir.is_dir().then_some(paths.logs_dir);
    if logs_dir.is_some() {
        let _ = menu.append(&logs_item);
    }
    if let Some(item) = &autostart_item {
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(item);
    }
    let _ = menu.append(&PredefinedMenuItem::separator());
    if let Some(item) = &update_item {
        let _ = menu.append(item);
    }
    let _ = menu.append(&about_item);
    let _ = menu.append(&quit_item);

    let icon = load_icon().map_err(|e| format!("menu bar icon decode: {e}"))?;
    let item = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("KMServer")
        .with_icon_templated(icon)
        .build()
        .map_err(|e| e.to_string())?;

    if let Some(item) = autostart_item {
        AUTOSTART_ITEM.with(|slot| *slot.borrow_mut() = Some(item));
    }
    if let Some(item) = update_item {
        UPDATE_ITEM.with(|slot| *slot.borrow_mut() = Some(item));
    }
    TRAY_ICON.with(|slot| *slot.borrow_mut() = Some(item.clone()));
    let config_dir = paths.config_dir;
    let refresh_update = update.clone();
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        if matches!(event, TrayIconEvent::Click { .. }) {
            refresh_autostart_item();
            refresh_update_item(&refresh_update);
        }
    }));
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| match event.id.0.as_str() {
        "open" => open_in_browser(port),
        "about" => show_about(port),
        "config" => open(config_dir.as_os_str()),
        "logs" => {
            if let Some(dir) = &logs_dir {
                open(dir.as_os_str());
            }
        }
        "autostart" => {
            let service = login_service();
            match unsafe { service.status() } {
                status if status == SMAppServiceStatus::RequiresApproval => {
                    open_login_settings();
                }
                status => {
                    let enable = status != SMAppServiceStatus::Enabled;
                    let result = if enable {
                        save_login_config(&config_dir)
                            .map_err(|e| e.to_string())
                            .and_then(|()| unsafe {
                                service.registerAndReturnError().map_err(|e| e.to_string())
                            })
                    } else {
                        let result = unsafe {
                            service
                                .unregisterAndReturnError()
                                .map_err(|e| e.to_string())
                        };
                        remove_login_config();
                        // launchd stops the login item's server along with the
                        // login item, so that server starts again on its own
                        if result.is_ok() && launched_by_login_agent() {
                            relaunch_after_exit(&config_dir);
                        }
                        result
                    };
                    if let Err(e) = result {
                        tracing::warn!("login item unchanged: {e}");
                    }
                }
            }
            refresh_autostart_item();
        }
        "quit" => {
            let _ = shutdown.send(true);
        }
        "update" => {
            if !crate::desktop::busy(&update.status()) {
                let update = update.clone();
                let shutdown = shutdown.clone();
                let app = crate::desktop::app_bundle_path().unwrap();
                let config_dir = config_dir.clone();
                let login_service = login_item_service(&config_dir);
                std::thread::spawn(move || {
                    if crate::desktop::click_installs(&update) {
                        crate::desktop::start_update(
                            update,
                            app,
                            config_dir,
                            port,
                            login_service,
                            shutdown,
                        )
                    }
                });
            }
            refresh_update_item(&update);
        }
        _ => {}
    }));
    Ok(item)
}

fn load_icon() -> Result<Icon, String> {
    load_icon_image(crate::desktop::Badge::None)
}

fn load_icon_image(badge: crate::desktop::Badge) -> Result<Icon, String> {
    let image = image::load_from_memory_with_format(MENU_BAR_ICON_PNG, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?
        .into_rgba8();
    let image = crate::desktop::badged_icon(&image, badge, true);
    let (width, height) = image.dimensions();
    Icon::from_rgba(image.into_raw(), width, height).map_err(|e| e.to_string())
}

/// Hands a URL or folder to Launch Services. Runs off the main thread so a slow
/// `open` never stalls the menu, and waits so the child is reaped.
fn open(target: &std::ffi::OsStr) {
    let target = Path::new(target).to_path_buf();
    std::thread::spawn(move || {
        match std::process::Command::new("/usr/bin/open")
            .arg(&target)
            .status()
        {
            Ok(status) if status.success() => {}
            Ok(status) => tracing::warn!("open {} failed: {status}", target.display()),
            Err(e) => tracing::warn!("open {} failed: {e}", target.display()),
        }
    });
}

fn login_config_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library/Application Support/KMServer/login-config-dir"))
}

fn save_login_config(config_dir: &Path) -> std::io::Result<()> {
    let path = login_config_path().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "app bundle is unavailable")
    })?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, config_dir.as_os_str().as_encoded_bytes())?;
    std::fs::rename(temporary, path)
}

fn remove_login_config() {
    if let Some(path) = login_config_path() {
        let _ = std::fs::remove_file(path);
    }
}

/// Opens the app again with the same data directory once this process has
/// exited, giving up if it is still running after a minute. The waiting script
/// runs in its own process group, which launchd's cleanup of the login item's
/// process group does not reach.
fn relaunch_after_exit(config_dir: &Path) {
    use std::os::unix::process::CommandExt;
    let Some(app) = crate::desktop::app_bundle_path() else {
        return;
    };
    let script = format!(
        "nohup /bin/sh -c 'waited=0; \
         while kill -0 {pid} 2>/dev/null && [ $waited -lt 60 ]; do \
         sleep 1; waited=$((waited + 1)); done; \
         kill -0 {pid} 2>/dev/null || exec /usr/bin/open -n \"$0\" --args --config-dir \"$1\"' \
         {app} {dir} </dev/null >/dev/null 2>&1 &",
        pid = std::process::id(),
        app = crate::desktop::shell_arg(&app),
        dir = crate::desktop::shell_arg(config_dir),
    );
    let started = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .process_group(0)
        .status();
    if !started.is_ok_and(|status| status.success()) {
        tracing::warn!("could not schedule the restart after leaving the login item");
    }
}

fn login_service() -> objc2::rc::Retained<SMAppService> {
    LOGIN_SERVICE.with(|slot| {
        slot.borrow_mut()
            .get_or_insert_with(|| unsafe {
                SMAppService::agentServiceWithPlistName(ns_string!(
                    "date.kmworks.server.login.plist"
                ))
            })
            .clone()
    })
}

fn autostart_enabled() -> bool {
    (unsafe { login_service().status() }) == SMAppServiceStatus::Enabled
}

fn refresh_autostart_item() {
    AUTOSTART_ITEM.with(|slot| {
        if let Some(item) = slot.borrow().as_ref() {
            item.set_checked(autostart_enabled());
        }
    });
}

fn refresh_update_item(update: &crate::service::desktop_update::DesktopUpdate) {
    UPDATE_ITEM.with(|slot| {
        if let Some(item) = slot.borrow().as_ref() {
            let status = update.status();
            let line = crate::desktop::update_line(&status, update.known_version().as_deref());
            set_update_line(item, &line);
            item.set_enabled(line.enabled);
            TRAY_ICON.with(|slot| {
                if let Some(icon) = slot.borrow().as_ref() {
                    if let Some(text) = crate::desktop::tooltip(&status) {
                        let _ = icon.set_tooltip(Some(text));
                    }
                }
            });
            let wanted = crate::desktop::update_badge(&status, update.pending());
            if let Some(badge) = crate::desktop::badge_change(BADGE.get(), wanted) {
                show_badge(badge);
            }
        }
    });
}

/// Shows the update entry, with its hint on a second line in smaller grey type.
fn set_update_line(item: &MenuItem, line: &crate::desktop::UpdateLine) {
    use objc2::AllocAnyThread;
    let Some(hint) = line.hint else {
        item.set_text(&line.title);
        return;
    };
    let text = format!("{}\n{hint}", line.title);
    let title = NSMutableAttributedString::initWithString(
        NSMutableAttributedString::alloc(),
        &NSString::from_str(&text),
    );
    // NSRange counts UTF-16 units
    let units = |part: &str| part.encode_utf16().count();
    let hint_range = NSRange::new(units(&text) - units(hint), units(hint));
    unsafe {
        title.addAttribute_value_range(
            NSFontAttributeName,
            &NSFont::menuFontOfSize(0.0),
            NSRange::new(0, units(&text)),
        );
        title.addAttribute_value_range(
            NSFontAttributeName,
            &NSFont::menuFontOfSize(NSFont::smallSystemFontSize()),
            hint_range,
        );
        title.addAttribute_value_range(
            NSForegroundColorAttributeName,
            &NSColor::secondaryLabelColor(),
            hint_range,
        );
    }
    item.set_attributed_title(Some(&title));
}

fn advance_spinner() {
    if let Some(badge) = crate::desktop::next_frame(BADGE.get()) {
        show_badge(badge);
    }
}

fn show_badge(badge: crate::desktop::Badge) {
    TRAY_ICON.with(|slot| {
        if let Some(icon) = slot.borrow().as_ref() {
            match load_icon_image(badge) {
                Ok(image) => {
                    let _ = icon.set_icon_templated(Some(image));
                    BADGE.set(badge);
                }
                Err(e) => tracing::warn!("menu bar icon decode: {e}"),
            }
        }
    });
}

fn open_login_settings() {
    let url = NSURL::URLWithString(ns_string!(
        "x-apple.systempreferences:com.apple.LoginItems-Settings.extension"
    ))
    .unwrap();
    objc2_app_kit::NSWorkspace::sharedWorkspace().openURL(&url);
}

/// The address another device on the network uses to reach this server, read
/// each time it is shown so it follows network changes. None without a usable
/// network address: localhost would only point another device at itself.
fn server_url(port: u16) -> Option<String> {
    lan_ipv4().map(|ip| format!("http://{ip}:{port}"))
}

/// The IPv4 address of an interface that is up. Ethernet and Wi-Fi (`en*`)
/// outrank VPN tunnels and virtual bridges even when those hold private
/// addresses: a tunnel address only reaches the server through the tunnel,
/// which another LAN device does not have. Among `en*` candidates the default
/// route's interface wins, then private over public, then enumeration order.
fn lan_ipv4() -> Option<std::net::Ipv4Addr> {
    let default_interface = default_route_interface();
    let mut best: Option<(u8, std::net::Ipv4Addr)> = None;
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return None;
    }
    let mut cursor = list;
    while !cursor.is_null() {
        let entry = unsafe { &*cursor };
        cursor = entry.ifa_next;
        let up = entry.ifa_flags & libc::IFF_UP as u32 != 0;
        let loopback = entry.ifa_flags & libc::IFF_LOOPBACK as u32 != 0;
        if !up || loopback || entry.ifa_addr.is_null() {
            continue;
        }
        if i32::from(unsafe { (*entry.ifa_addr).sa_family }) != libc::AF_INET {
            continue;
        }
        let address = unsafe { &*entry.ifa_addr.cast::<libc::sockaddr_in>() };
        let ip = std::net::Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr));
        if ip.is_link_local() {
            continue;
        }
        let name = unsafe { std::ffi::CStr::from_ptr(entry.ifa_name) }.to_bytes();
        let ethernet = name.starts_with(b"en");
        let rank = match (
            ethernet,
            default_interface.as_deref() == Some(name),
            ip.is_private(),
        ) {
            (true, true, true) => 5,
            (true, true, false) => 4,
            (true, false, true) => 3,
            (true, false, false) => 2,
            (false, _, true) => 1,
            (false, _, false) => 0,
        };
        if best.is_none_or(|(best_rank, _)| rank > best_rank) {
            best = Some((rank, ip));
        }
    }
    unsafe { libc::freeifaddrs(list) };
    best.map(|(_, ip)| ip)
}

/// The interface the default IPv4 route points at, from the route command's
/// stable output rather than the undocumented route-socket wire format. Runs
/// only when the About window opens, so a child process is cheap; None when
/// there is no default route, and VPN-scoped defaults lose to the real one
/// because the command reports the primary default route.
fn default_route_interface() -> Option<Vec<u8>> {
    let output = std::process::Command::new("/sbin/route")
        .args(["-n", "get", "-inet", "default"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .find_map(|line| line.trim().strip_prefix("interface: "))
        .map(|name| name.trim().as_bytes().to_vec())
}

const DOCUMENTATION_URL: &str = "https://kmworks.date/server/";

/// The standard About window, with a documentation link and the address other
/// devices use to reach this server in the credits. Name and version are passed
/// explicitly so a binary outside the app bundle shows them too.
fn show_about(port: u16) {
    use objc2::AllocAnyThread;
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    // a menu bar app is never frontmost by itself; activate() needs macOS 14 and
    // the app supports macOS 13
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);

    let address_line = match server_url(port) {
        Some(url) => format!("Server address: {url}"),
        None => "Server address: no network connection".to_string(),
    };
    let text = format!("Documentation\n{address_line}");
    let credits = NSMutableAttributedString::initWithString(
        NSMutableAttributedString::alloc(),
        &NSString::from_str(&text),
    );
    // NSRange counts UTF-16 units; the parts are slices of text, so find cannot miss
    let range_of = |part: &str| {
        let start = text
            .find(part)
            .expect("attribute range outside the credits text");
        NSRange::new(
            text[..start].encode_utf16().count(),
            part.encode_utf16().count(),
        )
    };
    let paragraph = NSMutableParagraphStyle::new();
    paragraph.setAlignment(NSTextAlignment::Center);
    let font = NSFont::systemFontOfSize(NSFont::smallSystemFontSize());
    let grey = NSColor::secondaryLabelColor();
    unsafe {
        credits.addAttribute_value_range(
            NSParagraphStyleAttributeName,
            &paragraph,
            range_of(&text),
        );
        credits.addAttribute_value_range(NSFontAttributeName, &font, range_of(&text));
        credits.addAttribute_value_range(
            NSForegroundColorAttributeName,
            &grey,
            range_of(&address_line),
        );
        if let Some(url) = NSURL::URLWithString(&NSString::from_str(DOCUMENTATION_URL)) {
            credits.addAttribute_value_range(NSLinkAttributeName, &url, range_of("Documentation"));
        }
        // paragraph spacing only takes effect through a paragraph style, so the
        // first paragraph gets its own style to set the address apart from the link
        let spaced = NSMutableParagraphStyle::new();
        spaced.setAlignment(NSTextAlignment::Center);
        spaced.setParagraphSpacing(6.0);
        credits.addAttribute_value_range(
            NSParagraphStyleAttributeName,
            &spaced,
            range_of("Documentation\n"),
        );
    }

    let version = NSString::from_str(env!("CARGO_PKG_VERSION"));
    // an empty build version keeps the window from repeating the version in
    // parentheses
    let build = ns_string!("");
    let keys = unsafe {
        [
            NSAboutPanelOptionApplicationName,
            NSAboutPanelOptionApplicationVersion,
            NSAboutPanelOptionVersion,
            NSAboutPanelOptionCredits,
        ]
    };
    let objects: [&AnyObject; 4] = [ns_string!("KMServer"), &version, build, &credits];
    let options = NSDictionary::from_slices(&keys, &objects);
    unsafe { app.orderFrontStandardAboutPanelWithOptions(&options) };
}

/// Key equivalents like Cmd+C are dispatched through the main menu. An accessory
/// app never shows one, but without it text in its windows (the About credits)
/// can only be copied through the context menu.
fn install_edit_menu(app: &NSApplication, mtm: MainThreadMarker) {
    use objc2::MainThreadOnly;
    let edit = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("Edit"));
    for (title, action, key) in [
        ("Copy", objc2::sel!(copy:), "c"),
        ("Select All", objc2::sel!(selectAll:), "a"),
    ] {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                Some(action),
                &NSString::from_str(key),
            )
        };
        edit.addItem(&item);
    }
    let edit_item = NSMenuItem::new(mtm);
    edit_item.setSubmenu(Some(&edit));
    let main = NSMenu::new(mtm);
    main.addItem(&edit_item);
    app.setMainMenu(Some(&main));
}
