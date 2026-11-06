//! Windows shell integration: the binary targets the windows subsystem (no
//! console), so a running server surfaces through a notification-area icon with
//! an Open/Exit menu.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
#[cfg(not(debug_assertions))]
use windows_sys::Win32::System::Console::FreeConsole;
use windows_sys::Win32::System::Console::{AttachConsole, GetConsoleWindow, ATTACH_PARENT_PROCESS};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ, RRF_RT_REG_SZ,
};
#[cfg(test)]
use windows_sys::Win32::System::Registry::{RegCreateKeyExW, RegDeleteKeyW};
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, OpenInputDesktop, DESKTOP_READOBJECTS,
};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MessageBoxW, PeekMessageW, PostThreadMessageW, TranslateMessage,
    MB_ICONERROR, MB_OK, MSG, PM_NOREMOVE, SW_SHOWNORMAL, WM_APP, WM_QUIT, WM_USER,
};

const TRAY_ICON_PNG: &[u8] = include_bytes!("../resources/windows/tray-icon.png");

/// Posted to the tray thread when the update status changes: the icon and the
/// menu can only be changed from that thread.
const WM_UPDATE_STATUS: u32 = WM_APP + 1;
/// Posted to the tray thread for each frame of the spinner badge.
const WM_SPINNER_STEP: u32 = WM_APP + 2;

const AUTOSTART_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(test)]
thread_local! {
    static AUTOSTART_TEST_SUBKEY: RefCell<Option<String>> = const { RefCell::new(None) };
}
const LEGACY_AUTOSTART_VALUE: &str = "KMReader";
const AUTOSTART_VALUE: &str = "KMServer";

thread_local! {
    // the menu event handler must be Send+Sync, so the check item (Rc-based)
    // cannot move into it; muda dispatches menu events synchronously on this
    // thread, where the item is reachable again
    static AUTOSTART_ITEM: RefCell<Option<CheckMenuItem>> = const { RefCell::new(None) };
    static UPDATE_ITEM: RefCell<Option<MenuItem>> = const { RefCell::new(None) };
    static TRAY_ICON: RefCell<Option<TrayIcon>> = const { RefCell::new(None) };
    // the badge the icon currently shows, so it is only swapped on a change
    static BADGE: std::cell::Cell<crate::desktop::Badge> =
        const { std::cell::Cell::new(crate::desktop::Badge::None) };
}

pub struct Tray {
    thread_id: u32,
    handle: JoinHandle<()>,
}

/// Folders the tray menu opens in Explorer. Grouped because both are PathBuf —
/// positional parameters of the same type would invite a swapped call site.
pub struct TrayPaths {
    pub config_dir: PathBuf,
    pub logs_dir: PathBuf,
}

/// Reattaches to the parent terminal when launched from one, so --help and
/// startup errors have somewhere to print. The shell never waits on a
/// windows-subsystem binary and closing the terminal would kill an attached
/// process, so detach_console() cuts over before serving starts.
pub fn attach_parent_console() {
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

pub fn detach_console() {
    // debug builds stay on the console subsystem (crate attribute in main.rs):
    // there the console is the log output and detaching would strand it
    #[cfg(not(debug_assertions))]
    unsafe {
        FreeConsole();
    }
}

/// Last-resort error surface for a fatal startup error: without a console the
/// process would otherwise vanish without a word. No-op for terminal launches,
/// which already see the error printed.
pub fn show_startup_error(error: &anyhow::Error) {
    if unsafe { GetConsoleWindow() }.is_null() {
        let text = wide(&format!(
            "{error:#}\n\nLog files are in the \"logs\" folder of the data directory."
        ));
        let title = wide("KMServer failed to start");
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                text.as_ptr(),
                title.as_ptr(),
                MB_ICONERROR | MB_OK,
            );
        }
    }
}

/// Opens the web UI in the default browser.
pub fn open_in_browser(port: u16) {
    shell_open(&wide(&format!("http://localhost:{port}/")));
}

/// Returns None in non-interactive sessions (Windows service, headless task):
/// there is no shell to show the icon in, and the server runs unaffected.
pub fn start(
    port: u16,
    paths: TrayPaths,
    shutdown: tokio::sync::watch::Sender<bool>,
    update: crate::service::desktop_update::DesktopUpdate,
) -> Option<Tray> {
    let interactive = unsafe {
        let desktop = OpenInputDesktop(0, 0, DESKTOP_READOBJECTS);
        if desktop.is_null() {
            false
        } else {
            CloseDesktop(desktop);
            true
        }
    };
    if !interactive {
        return None;
    }
    let (ready_tx, ready_rx) = mpsc::channel();
    let changes = update.subscribe();
    let handle = std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || run(port, paths, shutdown, update, ready_tx))
        .ok()?;
    match ready_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(thread_id)) => {
            // an update the daily check finds shows up without opening the menu
            tokio::spawn(crate::desktop::follow_status(
                changes,
                move || unsafe {
                    PostThreadMessageW(thread_id, WM_UPDATE_STATUS, 0, 0);
                },
                move || unsafe {
                    PostThreadMessageW(thread_id, WM_SPINNER_STEP, 0, 0);
                },
            ));
            Some(Tray { thread_id, handle })
        }
        Ok(Err(e)) => {
            tracing::warn!("system tray disabled: {e}");
            let _ = handle.join();
            None
        }
        Err(_) => {
            tracing::warn!("system tray disabled: initialization timed out");
            None
        }
    }
}

impl Tray {
    /// Quits the tray thread's message loop and joins it, so the icon is removed
    /// from the notification area instead of lingering as a ghost until hovered.
    pub fn stop(self) {
        unsafe {
            PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0);
        }
        let _ = self.handle.join();
    }
}

fn run(
    port: u16,
    paths: TrayPaths,
    shutdown: tokio::sync::watch::Sender<bool>,
    update: crate::service::desktop_update::DesktopUpdate,
    ready: mpsc::Sender<Result<u32, String>>,
) {
    let open_item = MenuItem::with_id("open", "Open KMServer", true, None);
    let config_item = MenuItem::with_id("config", "Open configuration directory", true, None);
    let logs_item = MenuItem::with_id("logs", "Open logs folder", true, None);
    let autostart_dir = paths.config_dir.clone();
    let update_config_dir = paths.config_dir.clone();
    let autostart_item = CheckMenuItem::with_id(
        "autostart",
        "Start with Windows",
        true,
        autostart_enabled(),
        None,
    );
    let exit_item = MenuItem::with_id("exit", "Exit", true, None);
    let installed_exe = crate::desktop::installed_exe();
    let update_item = installed_exe.as_ref().map(|_| {
        MenuItem::with_id(
            "update",
            crate::desktop::status_label(&update.status(), update.pending()),
            true,
            None,
        )
    });
    let menu = Menu::new();
    let _ = menu.append(&open_item);
    let config_target = wide_os(paths.config_dir.as_os_str());
    let _ = menu.append(&config_item);
    // without a console the log files are the only place startup/scan errors show
    // up, so the menu only offers the folder when file logging came up
    let logs_target = paths
        .logs_dir
        .is_dir()
        .then(|| wide_os(paths.logs_dir.as_os_str()));
    if logs_target.is_some() {
        let _ = menu.append(&logs_item);
    }
    let _ = menu.append(&autostart_item);
    if let Some(item) = &update_item {
        let _ = menu.append(item);
    }
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&exit_item);
    AUTOSTART_ITEM.with(|slot| *slot.borrow_mut() = Some(autostart_item));
    if let Some(item) = update_item {
        UPDATE_ITEM.with(|slot| *slot.borrow_mut() = Some(item));
    }
    let tray = (|| {
        let icon = load_icon().map_err(|e| format!("tray icon decode: {e}"))?;
        TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            // default true would pop the menu on left-button-up, making a plain
            // left click (our open-UI gesture) unreachable
            .with_menu_on_left_click(false)
            .with_tooltip("KMServer")
            .with_icon(icon)
            .build()
            .map_err(|e| e.to_string())
    })();
    let tray = match tray {
        Ok(tray) => tray,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };

    TRAY_ICON.with(|slot| *slot.borrow_mut() = Some(tray.clone()));
    migrate_legacy_autostart();
    let status_update = update.clone();
    refresh_update_item(&status_update);

    let refresh_update = update.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| match event.id.0.as_str() {
        "open" => open_in_browser(port),
        "logs" => {
            if let Some(target) = &logs_target {
                shell_open(target);
            }
        }
        "config" => shell_open(&config_target),
        "autostart" => {
            let enable = !autostart_enabled();
            if let Err(e) = set_autostart(enable, &autostart_dir) {
                tracing::warn!("startup registration unchanged: {e}");
            }
            // display what the registry holds, not what the click flipped to: an
            // out-of-band edit would otherwise leave the mark inverted all session
            AUTOSTART_ITEM.with(|slot| {
                if let Some(item) = slot.borrow().as_ref() {
                    item.set_checked(autostart_enabled());
                }
            });
        }
        "exit" => {
            let _ = shutdown.send(true);
        }
        "update" => {
            if !crate::desktop::busy(&update.status()) {
                let update = update.clone();
                let shutdown = shutdown.clone();
                let app = installed_exe.clone().unwrap();
                let config_dir = update_config_dir.clone();
                std::thread::spawn(move || {
                    if crate::desktop::click_installs(&update) {
                        crate::desktop::start_update(update, app, config_dir, shutdown)
                    }
                });
            }
            refresh_update_item(&update);
        }
        _ => {}
    }));
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        refresh_update_item(&refresh_update);
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = event
        {
            open_in_browser(port);
        }
    }));

    unsafe {
        let mut msg = MSG::default();
        // force the message queue into existence so stop() can post WM_QUIT to it
        PeekMessageW(
            &mut msg,
            std::ptr::null_mut(),
            WM_USER,
            WM_USER,
            PM_NOREMOVE,
        );
        let _ = ready.send(Ok(GetCurrentThreadId()));

        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            // a thread message has no window to be dispatched to
            if msg.message == WM_UPDATE_STATUS {
                refresh_update_item(&status_update);
                continue;
            }
            if msg.message == WM_SPINNER_STEP {
                advance_spinner();
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    drop(tray);
}

fn load_icon() -> Result<Icon, String> {
    load_icon_image(crate::desktop::Badge::None)
}

fn load_icon_image(badge: crate::desktop::Badge) -> Result<Icon, String> {
    let image = image::load_from_memory_with_format(TRAY_ICON_PNG, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?
        .into_rgba8();
    let image = crate::desktop::badged_icon(&image, badge, false);
    let (width, height) = image.dimensions();
    Icon::from_rgba(image.into_raw(), width, height).map_err(|e| e.to_string())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_os(s: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().chain(std::iter::once(0)).collect()
}

fn shell_open(target: &[u16]) {
    let verb = wide("open");
    unsafe {
        let result = ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
        // the API's legacy return contract: values ≤ 32 are failure codes
        if result as isize <= 32 {
            tracing::warn!("ShellExecute open failed (code {})", result as isize);
        }
    }
}

fn refresh_update_item(update: &crate::service::desktop_update::DesktopUpdate) {
    UPDATE_ITEM.with(|slot| {
        if let Some(item) = slot.borrow().as_ref() {
            let status = update.status();
            item.set_text(crate::desktop::status_label(&status, update.pending()));
            item.set_enabled(!crate::desktop::busy(&status));
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
                    let _ = icon.set_icon(Some(image));
                    BADGE.set(badge);
                }
                Err(e) => tracing::warn!("tray icon decode: {e}"),
            }
        }
    });
}

// Compatibility with the old startup registration; remove in 0.27.0.
fn migrate_legacy_autostart() {
    unsafe {
        let Some(hkey) = open_autostart_key() else {
            return;
        };
        if read_autostart(hkey.0, AUTOSTART_VALUE)
            .ok()
            .flatten()
            .is_some()
        {
            RegDeleteValueW(hkey.0, wide(LEGACY_AUTOSTART_VALUE).as_ptr());
            return;
        }
        let Ok(Some(command)) = read_autostart(hkey.0, LEGACY_AUTOSTART_VALUE) else {
            return;
        };
        if write_autostart(hkey.0, AUTOSTART_VALUE, &command) == ERROR_SUCCESS {
            RegDeleteValueW(hkey.0, wide(LEGACY_AUTOSTART_VALUE).as_ptr());
        }
    }
}

pub fn installed_exe() -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;

    let exe = std::env::current_exe().ok()?;
    let key = unsafe { open_install_key()? };
    let installed = unsafe { read_autostart(key.0, "InstallDir").ok().flatten()? };
    let installed = std::ffi::OsString::from_wide(&installed[..installed.len().saturating_sub(1)]);
    (exe == PathBuf::from(installed).join("kmrs.exe")).then_some(exe)
}

unsafe fn open_install_key() -> Option<AutostartKey> {
    let mut hkey: HKEY = std::ptr::null_mut();
    let result = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            wide(r"Software\KMServer").as_ptr(),
            0,
            KEY_QUERY_VALUE,
            &mut hkey,
        )
    };
    (result == ERROR_SUCCESS).then_some(AutostartKey(hkey))
}

fn autostart_enabled() -> bool {
    unsafe {
        let Some(hkey) = open_autostart_key() else {
            return false;
        };
        read_autostart(hkey.0, AUTOSTART_VALUE)
            .ok()
            .flatten()
            .is_some()
    }
}

struct AutostartKey(HKEY);

impl Drop for AutostartKey {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

fn set_autostart(enable: bool, config_dir: &std::path::Path) -> Result<(), String> {
    // resolved before opening the key: a failure here must not leak the handle
    let command = enable.then(|| autostart_command(config_dir)).transpose()?;
    unsafe {
        let Some(hkey) = open_autostart_key() else {
            return Err("open Run key".into());
        };
        let result = match &command {
            Some(data) => write_autostart(hkey.0, AUTOSTART_VALUE, data),
            None => RegDeleteValueW(hkey.0, wide(AUTOSTART_VALUE).as_ptr()),
        };
        if result == ERROR_SUCCESS {
            RegDeleteValueW(hkey.0, wide(LEGACY_AUTOSTART_VALUE).as_ptr());
        }
        if result == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("write Run value: error {result}"))
        }
    }
}

unsafe fn open_autostart_key() -> Option<AutostartKey> {
    #[cfg(test)]
    let subkey = AUTOSTART_TEST_SUBKEY
        .with(|key| key.borrow().clone())
        .unwrap_or_else(|| AUTOSTART_SUBKEY.into());
    #[cfg(not(test))]
    let subkey: String = AUTOSTART_SUBKEY.into();
    let mut hkey: HKEY = std::ptr::null_mut();
    let result = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            wide(&subkey).as_ptr(),
            0,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            &mut hkey,
        )
    };
    (result == ERROR_SUCCESS).then_some(AutostartKey(hkey))
}

unsafe fn read_autostart(hkey: HKEY, name: &str) -> Result<Option<Vec<u16>>, u32> {
    let name = wide(name);
    let mut bytes = 0;
    let result = unsafe {
        RegGetValueW(
            hkey,
            std::ptr::null(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut bytes,
        )
    };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if result != ERROR_SUCCESS {
        return Err(result);
    }
    let mut data = vec![0u16; bytes as usize / size_of::<u16>()];
    let result = unsafe {
        RegGetValueW(
            hkey,
            std::ptr::null(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            data.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if result != ERROR_SUCCESS {
        return Err(result);
    }
    while data.last() == Some(&0) {
        data.pop();
    }
    data.push(0);
    Ok(Some(data))
}

unsafe fn write_autostart(hkey: HKEY, name: &str, data: &[u16]) -> u32 {
    unsafe {
        RegSetValueExW(
            hkey,
            wide(name).as_ptr(),
            0,
            REG_SZ,
            data.as_ptr().cast(),
            (data.len() * 2) as u32,
        )
    }
}

fn autostart_command(config_dir: &std::path::Path) -> Result<Vec<u16>, String> {
    use std::os::windows::ffi::OsStrExt;
    let exe = std::env::current_exe().map_err(|e| format!("current exe: {e}"))?;
    let config_dir = std::path::absolute(config_dir).map_err(|e| format!("config dir: {e}"))?;
    // a logon start gets an arbitrary working directory, and the default config
    // dir is CWD-relative: pin it. Run values are parsed as a command line, so
    // both paths are quoted (spaces); encode_wide keeps non-UTF-8 paths intact
    let mut command: Vec<u16> = Vec::new();
    command.push(b'"' as u16);
    command.extend(exe.as_os_str().encode_wide());
    command.extend("\" --config-dir \"".encode_utf16());
    command.extend(config_dir.as_os_str().encode_wide());
    command.push(b'"' as u16);
    command.push(0);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_autostart_is_migrated_and_disabled() {
        let key = TestAutostartKey::new();
        let old = wide(r#"C:\Program Files\KMReader\kmrs.exe" --config-dir "C:\data"#);
        let new = wide("new-command");
        unsafe {
            assert_eq!(
                write_autostart(key.0, LEGACY_AUTOSTART_VALUE, &old),
                ERROR_SUCCESS
            );
            assert_eq!(
                read_autostart(key.0, LEGACY_AUTOSTART_VALUE),
                Ok(Some(old.clone()))
            );
            migrate_legacy_autostart();
            assert_eq!(
                read_autostart(key.0, AUTOSTART_VALUE),
                Ok(Some(old.clone()))
            );
            assert_eq!(read_autostart(key.0, LEGACY_AUTOSTART_VALUE), Ok(None));
            assert!(autostart_enabled());

            assert_eq!(write_autostart(key.0, AUTOSTART_VALUE, &new), ERROR_SUCCESS);
            assert_eq!(
                write_autostart(key.0, LEGACY_AUTOSTART_VALUE, &old),
                ERROR_SUCCESS
            );
            migrate_legacy_autostart();
            assert_eq!(read_autostart(key.0, AUTOSTART_VALUE), Ok(Some(new)));
            assert_eq!(read_autostart(key.0, LEGACY_AUTOSTART_VALUE), Ok(None));

            set_autostart(false, std::path::Path::new(".")).unwrap();
            assert!(!autostart_enabled());
            assert_eq!(read_autostart(key.0, LEGACY_AUTOSTART_VALUE), Ok(None));
        }
    }

    struct TestAutostartKey(HKEY);

    impl TestAutostartKey {
        fn new() -> Self {
            let subkey = format!(r"Software\kmrs-tests\{}", std::process::id());
            AUTOSTART_TEST_SUBKEY.with(|key| *key.borrow_mut() = Some(subkey.clone()));
            unsafe {
                RegDeleteKeyW(HKEY_CURRENT_USER, wide(&subkey).as_ptr());
                let mut key = std::ptr::null_mut();
                let result = RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    wide(&subkey).as_ptr(),
                    0,
                    std::ptr::null(),
                    0,
                    KEY_QUERY_VALUE | KEY_SET_VALUE,
                    std::ptr::null(),
                    &mut key,
                    std::ptr::null_mut(),
                );
                assert_eq!(result, ERROR_SUCCESS);
                Self(key)
            }
        }
    }

    impl Drop for TestAutostartKey {
        fn drop(&mut self) {
            let subkey = AUTOSTART_TEST_SUBKEY.with(|key| key.borrow_mut().take());
            unsafe {
                RegCloseKey(self.0);
                RegDeleteKeyW(HKEY_CURRENT_USER, wide(&subkey.unwrap()).as_ptr());
            }
        }
    }
}
