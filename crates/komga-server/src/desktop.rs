//! Desktop installation boundaries: only an installed app or per-user Windows
//! installation can hand a verified update to its platform helper.

#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::path::Path;
use std::path::PathBuf;

use crate::service::desktop_update::DesktopUpdate;

/// How often a failed update is checked again while a newer release is known, so a
/// failure from a dropped connection clears once the network is back.
const RETRY_AFTER_FAILURE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

pub fn update_handle() -> DesktopUpdate {
    DesktopUpdate::new(tokio::runtime::Handle::current())
}

pub fn status_label(status: &crate::service::desktop_update::Status) -> String {
    use crate::service::desktop_update::Status;
    match status {
        Status::Idle => "Check for Updates".into(),
        Status::Checking => "Checking for Updates…".into(),
        Status::Available(version) => format!("Update to {version} and Restart"),
        Status::Downloading(version) => format!("Downloading {version}…"),
        Status::Installing(version) => format!("Installing {version}…"),
        Status::Failed(_) => "Retry Update".into(),
    }
}

pub fn tooltip(status: &crate::service::desktop_update::Status) -> String {
    match status {
        crate::service::desktop_update::Status::Failed(error) => format!("Update failed: {error}"),
        _ => "KMServer".into(),
    }
}

pub fn busy(status: &crate::service::desktop_update::Status) -> bool {
    use crate::service::desktop_update::Status;
    matches!(
        status,
        Status::Checking | Status::Downloading(_) | Status::Installing(_)
    )
}

/// Whether this process is an installation the menu can update.
fn installed() -> bool {
    #[cfg(target_os = "macos")]
    {
        app_bundle_path().is_some()
    }
    #[cfg(target_os = "windows")]
    {
        installed_exe().is_some()
    }
}

/// Lets a release found by the daily update check become an update the menu offers,
/// without waiting for a click on "Check for Updates". The desktop check runs on top
/// of it because only that one selects the release asset and its checksum.
pub fn follow_update_check(
    update: DesktopUpdate,
    mut found: tokio::sync::watch::Receiver<Option<String>>,
) {
    use crate::service::desktop_update::Status;

    if !installed() {
        return;
    }
    tokio::spawn(async move {
        let mut retry = tokio::time::interval(RETRY_AFTER_FAILURE);
        loop {
            let found_now = found.borrow_and_update().is_some();
            update.announce(found_now);
            if found_now {
                // refused while a check or an update is already running
                let _ = update.start_check();
            }
            loop {
                tokio::select! {
                    changed = found.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        break;
                    }
                    _ = retry.tick() => {
                        if matches!(update.status(), Status::Failed(_)) && update.pending() {
                            let _ = update.start_check();
                        }
                    }
                }
            }
        }
    });
}

/// The mark on the tray icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Badge {
    /// the plain icon
    None,
    /// an up arrow: an update is available or being installed
    Update,
    /// an exclamation mark: an update is known but could not be installed
    Failed,
}

/// The badge for this status, or `None` while a check runs so that a background retry
/// keeps the icon steady. A failure is flagged only while a newer release is known
/// (`pending`): a check that failed with nothing to install needs no lasting warning.
pub fn update_badge(
    status: &crate::service::desktop_update::Status,
    pending: bool,
) -> Option<Badge> {
    use crate::service::desktop_update::Status;
    Some(match status {
        Status::Checking => return None,
        Status::Available(_) | Status::Downloading(_) | Status::Installing(_) => Badge::Update,
        Status::Failed(_) if pending => Badge::Failed,
        Status::Idle | Status::Failed(_) => Badge::None,
    })
}

/// The tray icon with a badge: a filled circle holding the mark in the bottom-right
/// corner, set off from the logo by a transparent ring, with the logo shrunk toward
/// the top-left to make room. A macOS template image only keeps its alpha channel, so
/// there (`template`) the mark is cut out of an opaque circle and the logo is faded to
/// let the badge stand out; otherwise the badge is drawn in colour.
pub fn badged_icon(icon: &image::RgbaImage, badge: Badge, template: bool) -> image::RgbaImage {
    use image::imageops::{resize, FilterType};

    if badge == Badge::None {
        return icon.clone();
    }

    let (width, height) = icon.dimensions();
    let size = width.min(height) as f32;
    let logo_size = (size * 0.86).round() as u32;
    let fade = if template { 0.7 } else { 1.0 };
    let mut out = image::RgbaImage::new(width, height);
    for (x, y, pixel) in resize(icon, logo_size, logo_size, FilterType::Triangle).enumerate_pixels()
    {
        let mut pixel = *pixel;
        pixel.0[3] = (f32::from(pixel.0[3]) * fade).round() as u8;
        out.put_pixel(x, y, pixel);
    }

    let diameter = size * 34.0 / 64.0;
    let radius = diameter / 2.0;
    let gap = size * 4.0 / 64.0;
    let (cx, cy) = (width as f32 - radius, height as f32 - radius);
    let in_circle = |x: f32, y: f32, r: f32| (x - cx).powi(2) + (y - cy).powi(2) <= r * r;
    // in units of the circle's diameter around its centre, y growing downward
    let in_mark = |x: f32, y: f32| {
        let (u, v) = ((x - cx) / diameter, (y - cy) / diameter);
        if badge == Badge::Failed {
            // a bar above a dot
            let bar = u.abs() <= 0.09 && (-0.30..=0.04).contains(&v);
            let dot = u * u + (v - 0.19).powi(2) <= 0.09 * 0.09;
            return bar || dot;
        }
        // a stem below a triangular head whose tip points up
        let stem = u.abs() <= 0.09 && (-0.04..=0.27).contains(&v);
        let head = (-0.30..=0.0).contains(&v) && u.abs() <= 0.28 * (v + 0.30) / 0.30;
        stem || head
    };
    // share of a pixel inside a shape, sampled on a 4x4 grid for smooth edges
    let coverage = |px: u32, py: u32, inside: &dyn Fn(f32, f32) -> bool| {
        let hits = (0..16)
            .filter(|i| {
                let sx = px as f32 + (f32::from(*i as u8 % 4) + 0.5) / 4.0;
                let sy = py as f32 + (f32::from(*i as u8 / 4) + 0.5) / 4.0;
                inside(sx, sy)
            })
            .count();
        hits as f32 / 16.0
    };
    let fill = match (template, badge) {
        (true, _) => [0.0, 0.0, 0.0],
        (false, Badge::Failed) => [209.0, 52.0, 56.0],
        (false, _) => [31.0, 122.0, 224.0],
    };

    for (x, y, pixel) in out.enumerate_pixels_mut() {
        let ring = coverage(x, y, &|sx, sy| in_circle(sx, sy, radius + gap));
        if ring == 0.0 {
            continue;
        }
        let circle = coverage(x, y, &|sx, sy| in_circle(sx, sy, radius));
        let mark = coverage(x, y, &|sx, sy| in_circle(sx, sy, radius) && in_mark(sx, sy));
        let logo_alpha = f32::from(pixel.0[3]) / 255.0 * (1.0 - ring);
        let (badge_alpha, badge_rgb) = if template {
            ((circle - mark).max(0.0), fill)
        } else if circle > 0.0 {
            let white = mark / circle;
            let rgb = [0, 1, 2].map(|c| fill[c] * (1.0 - white) + 255.0 * white);
            (circle, rgb)
        } else {
            (0.0, fill)
        };
        let alpha = badge_alpha + logo_alpha * (1.0 - badge_alpha);
        if alpha > 0.0 {
            for (channel, badge) in pixel.0.iter_mut().zip(badge_rgb) {
                let logo = f32::from(*channel) * logo_alpha * (1.0 - badge_alpha);
                *channel = ((badge * badge_alpha + logo) / alpha).round() as u8;
            }
        }
        pixel.0[3] = (alpha * 255.0).round() as u8;
    }
    out
}

#[cfg(target_os = "macos")]
pub fn app_bundle_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let app = exe.parent()?.parent()?.parent()?;
    (app.extension()? == "app").then(|| app.to_path_buf())
}

#[cfg(target_os = "macos")]
pub fn start_update(
    update: DesktopUpdate,
    app: PathBuf,
    config_dir: PathBuf,
    port: u16,
    login_service: Option<String>,
    shutdown: tokio::sync::watch::Sender<bool>,
) {
    if let Err(e) = install_macos(
        &update,
        &app,
        &config_dir,
        port,
        login_service.as_deref(),
        &shutdown,
    ) {
        update.set_failed(e.clone());
        tracing::error!("macOS update failed: {e}");
    }
}

#[cfg(target_os = "macos")]
fn install_macos(
    update: &DesktopUpdate,
    app: &Path,
    config_dir: &Path,
    port: u16,
    login_service: Option<&str>,
    shutdown: &tokio::sync::watch::Sender<bool>,
) -> Result<(), String> {
    use crate::service::desktop_update::Status;

    if !matches!(update.status(), Status::Available(_)) {
        return Err("no checked update is ready".into());
    };
    let parent = app.parent().ok_or("app has no parent directory")?;
    let work = tempfile::Builder::new()
        .prefix(".kmserver-update-")
        .tempdir_in(parent)
        .map_err(|e| format!("create update directory: {e}"))?;
    let downloaded = update.download_verified(work.path())?;
    unpack_app(&downloaded.path, work.path())?;
    let staged = work.path().join("staged/KMServer.app");
    verify_app(&staged)?;
    let helper = work.path().join("update-helper.sh");
    std::fs::write(
        &helper,
        include_bytes!("../resources/macos/update-helper.sh"),
    )
    .map_err(|e| format!("write update helper: {e}"))?;
    let script = format!(
        "nohup /bin/sh {} {} {} {} {} {} {} </dev/null >/dev/null 2>&1 &",
        shell_arg(&helper),
        shell_arg(Path::new(&std::process::id().to_string())),
        shell_arg(app),
        shell_arg(work.path()),
        shell_arg(config_dir),
        port,
        shell_arg(Path::new(login_service.unwrap_or_default())),
    );
    // nohup only ignores SIGHUP; launchd kills the job's whole process group on exit
    use std::os::unix::process::CommandExt;
    let status = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .process_group(0)
        .status()
        .map_err(|e| format!("start update helper: {e}"))?;
    if !status.success() {
        return Err(format!("update helper failed to start: {status}"));
    }
    update.installing();
    // The detached helper owns the staging files from this point.
    std::mem::forget(work);
    let _ = shutdown.send(true);
    Ok(())
}

#[cfg(target_os = "macos")]
fn verify_app(app: &Path) -> Result<(), String> {
    for (program, args) in [
        ("/usr/bin/codesign", vec!["--verify", "--deep", "--strict"]),
        ("/usr/bin/spctl", vec!["--assess", "--type", "execute"]),
    ] {
        let status = std::process::Command::new(program)
            .args(args)
            .arg(app)
            .status()
            .map_err(|e| format!("verify update: {e}"))?;
        if !status.success() {
            return Err(format!("verify update: {status}"));
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn unpack_app(archive: &Path, directory: &Path) -> Result<(), String> {
    let status = std::process::Command::new("/usr/bin/ditto")
        .arg("-x")
        .arg("-k")
        .arg(archive)
        .arg(directory.join("staged"))
        .status()
        .map_err(|e| format!("unpack update: {e}"))?;
    if !status.success() {
        return Err(format!("unpack update: {status}"));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn shell_arg(path: &Path) -> String {
    let text = path.to_string_lossy();
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[cfg(target_os = "windows")]
pub fn installed_exe() -> Option<PathBuf> {
    crate::tray::installed_exe()
}

#[cfg(target_os = "windows")]
pub fn start_update(
    update: DesktopUpdate,
    _app: PathBuf,
    config_dir: PathBuf,
    shutdown: tokio::sync::watch::Sender<bool>,
) {
    if let Err(e) = install_windows(&update, &config_dir, &shutdown) {
        update.set_failed(e.clone());
        tracing::error!("Windows update failed: {e}");
    }
}

#[cfg(target_os = "windows")]
fn install_windows(
    update: &DesktopUpdate,
    config_dir: &Path,
    shutdown: &tokio::sync::watch::Sender<bool>,
) -> Result<(), String> {
    use crate::service::desktop_update::Status;

    if !matches!(update.status(), Status::Available(_)) {
        return Err("no checked update is ready".into());
    };
    let directory = std::env::temp_dir().join(format!("kmserver-update-{}", std::process::id()));
    std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
    let downloaded = update.download_verified(&directory)?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let install_dir = exe.parent().ok_or("installed executable has no parent")?;
    let result = std::process::Command::new(&downloaded.path)
        .arg("/S")
        .arg(format!("/UPDATEPID={}", std::process::id()))
        .arg(format!(r#"/INSTALLDIR="{}""#, install_dir.display()))
        .arg(format!(r#"/CONFIGDIR="{}""#, config_dir.display()))
        .spawn();
    match result {
        Ok(_) => {
            update.installing();
            let _ = shutdown.send(true);
            Ok(())
        }
        Err(e) => Err(format!("start installer: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badge_keeps_the_icon_size_and_stays_in_the_corner() {
        for (badge, template) in [
            (Badge::Update, true),
            (Badge::Update, false),
            (Badge::Failed, true),
            (Badge::Failed, false),
        ] {
            let badged = badged_icon(&image::RgbaImage::new(64, 64), badge, template);
            assert_eq!(badged.dimensions(), (64, 64));
            // on a transparent icon only the badge is drawn, in the bottom-right corner
            assert!(badged
                .enumerate_pixels()
                .all(|(x, y, pixel)| pixel.0[3] == 0 || (x >= 29 && y >= 29)));
            assert!(badged.pixels().any(|pixel| pixel.0[3] == 255));
        }
    }

    #[test]
    fn failed_and_update_badges_look_different() {
        let icon = image::RgbaImage::new(64, 64);
        for template in [true, false] {
            assert_ne!(
                badged_icon(&icon, Badge::Update, template),
                badged_icon(&icon, Badge::Failed, template)
            );
        }
        assert_eq!(badged_icon(&icon, Badge::None, true), icon);
    }

    #[test]
    fn badge_follows_the_update_status() {
        use crate::service::desktop_update::Status;
        for status in [
            Status::Available("1.2.3".into()),
            Status::Downloading("1.2.3".into()),
            Status::Installing("1.2.3".into()),
        ] {
            assert_eq!(
                update_badge(&status, true),
                Some(Badge::Update),
                "{status:?}"
            );
        }
        assert_eq!(
            update_badge(&Status::Failed("x".into()), true),
            Some(Badge::Failed)
        );
        // a failed check with no known release leaves the icon plain
        assert_eq!(
            update_badge(&Status::Failed("x".into()), false),
            Some(Badge::None)
        );
        assert_eq!(update_badge(&Status::Idle, false), Some(Badge::None));
        // a check in progress keeps whatever the icon shows
        assert_eq!(update_badge(&Status::Checking, true), None);
    }

    #[test]
    fn status_is_actionable_without_hiding_progress() {
        assert_eq!(
            status_label(&crate::service::desktop_update::Status::Idle),
            "Check for Updates"
        );
        assert!(
            status_label(&crate::service::desktop_update::Status::Available(
                "1.2.3".into()
            ))
            .contains("1.2.3")
        );
        assert!(busy(&crate::service::desktop_update::Status::Installing(
            "1.2.3".into()
        )));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn shell_arguments_preserve_apostrophes() {
        assert_eq!(shell_arg(Path::new("/tmp/KM's app")), "'/tmp/KM'\\''s app'");
    }
}
