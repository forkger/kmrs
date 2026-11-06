//! Desktop installation boundaries: only an installed app or per-user Windows
//! installation can hand a verified update to its platform helper.

#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::path::Path;
use std::path::PathBuf;

use crate::service::desktop_update::DesktopUpdate;

/// How often a failed update is checked again while a newer release is known, so a
/// failure from a dropped connection clears once the network is back.
const RETRY_AFTER_FAILURE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Frames in one turn of the spinner badge and how long each shows: about a turn a second.
const SPINNER_FRAMES: u8 = 12;
const SPINNER_STEP: std::time::Duration = std::time::Duration::from_millis(80);

pub fn update_handle() -> DesktopUpdate {
    DesktopUpdate::new(tokio::runtime::Handle::current())
}

/// The Windows menu's update entry. A failure offers a retry only while a newer release
/// is known (`pending`); a check that failed with nothing to install offers a new check.
#[cfg(target_os = "windows")]
pub fn status_label(status: &crate::service::desktop_update::Status, pending: bool) -> String {
    use crate::service::desktop_update::Status;
    match status {
        Status::Checking => "Checking for Updates…".into(),
        Status::Available(version) => format!("Update to {version} and Restart"),
        Status::Downloading(version) => format!("Downloading {version}…"),
        Status::Installing(version) => format!("Installing {version}…"),
        Status::Failed(_) if pending => "Retry Update".into(),
        Status::Idle | Status::Failed(_) => "Check for Updates".into(),
    }
}

/// The tray icon's hover text, or `None` while a check runs so that a background retry
/// keeps the failure reason next to the "!" badge.
pub fn tooltip(status: &crate::service::desktop_update::Status) -> Option<String> {
    use crate::service::desktop_update::Status;
    match status {
        Status::Checking => None,
        Status::Failed(error) => Some(format!("Update failed: {error}")),
        _ => Some("KMServer".into()),
    }
}

/// Handles a click on the update entry while nothing is under way, and returns whether to
/// go on and install. A checked update installs; a failed one for a known release is
/// checked again and installs if the check finds it, so a retry takes one click. Anything
/// else starts a check. Blocks during the retry's check, so call it off the tray's thread.
pub fn click_installs(update: &DesktopUpdate) -> bool {
    use crate::service::desktop_update::Status;
    match update.status() {
        Status::Available(_) => true,
        Status::Failed(_) if update.pending() => {
            update.check_blocking().is_ok() && matches!(update.status(), Status::Available(_))
        }
        _ => {
            let _ = update.start_check();
            false
        }
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
            let found_now = found.borrow_and_update().clone();
            let newer = found_now.is_some();
            update.announce(found_now);
            if newer {
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

/// The update entry in the macOS menu: a main line, an optional hint under it in smaller
/// grey type, and whether clicking it does anything.
#[cfg(target_os = "macos")]
#[derive(Debug, PartialEq, Eq)]
pub struct UpdateLine {
    pub title: String,
    pub hint: Option<&'static str>,
    pub enabled: bool,
}

/// The macOS menu's update entry for this status. Unless an update is under way or known,
/// it stays the on-demand "Check for Updates".
#[cfg(target_os = "macos")]
pub fn update_line(
    status: &crate::service::desktop_update::Status,
    known_version: Option<&str>,
) -> UpdateLine {
    use crate::service::desktop_update::Status;
    let line = |title: String, hint, enabled| UpdateLine {
        title,
        hint,
        enabled,
    };
    match (status, known_version) {
        (Status::Checking, _) => line("Checking for Updates…".into(), None, false),
        (Status::Available(version), _) => line(
            format!("Version {version} available"),
            Some("Click to update and restart"),
            true,
        ),
        (Status::Downloading(version) | Status::Installing(version), _) => {
            line(format!("Updating to {version}…"), None, false)
        }
        (Status::Failed(_), Some(version)) => line(
            format!("Version {version} available"),
            Some("Update failed. Click to try again"),
            true,
        ),
        (Status::Idle | Status::Failed(_), _) => line("Check for Updates".into(), None, true),
    }
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
    /// the up arrow inside a turning ring, at this frame: an update is downloading or
    /// installing
    Updating(u8),
}

/// The badge to switch to, if any: none while a check keeps the current one, and none
/// between spinner frames, which advance on their own clock.
pub fn badge_change(shown: Badge, wanted: Option<Badge>) -> Option<Badge> {
    match (shown, wanted?) {
        (Badge::Updating(_), Badge::Updating(_)) => None,
        (shown, wanted) if shown == wanted => None,
        (_, wanted) => Some(wanted),
    }
}

/// The spinner's next frame while it shows.
pub fn next_frame(shown: Badge) -> Option<Badge> {
    match shown {
        Badge::Updating(frame) => Some(Badge::Updating((frame + 1) % SPINNER_FRAMES)),
        _ => None,
    }
}

/// Runs `refresh` after every status change and, while an update downloads or installs,
/// `advance` once per spinner frame. Both are expected to hand off to the tray's thread.
pub async fn follow_status(
    mut changes: tokio::sync::watch::Receiver<crate::service::desktop_update::Status>,
    refresh: impl Fn(),
    advance: impl Fn(),
) {
    use crate::service::desktop_update::Status;

    let mut frames = tokio::time::interval(SPINNER_STEP);
    frames.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let spinning = matches!(
            *changes.borrow(),
            Status::Downloading(_) | Status::Installing(_)
        );
        if spinning {
            tokio::select! {
                changed = changes.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    refresh();
                }
                _ = frames.tick() => advance(),
            }
        } else if changes.changed().await.is_ok() {
            refresh();
        } else {
            return;
        }
    }
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
        Status::Available(_) => Badge::Update,
        Status::Downloading(_) | Status::Installing(_) => Badge::Updating(0),
        Status::Failed(_) if pending => Badge::Failed,
        Status::Idle | Status::Failed(_) => Badge::None,
    })
}

/// The tray icon with a badge: a filled circle holding the mark in the bottom-right
/// corner, set off from the logo by a transparent ring, with the logo shrunk toward
/// the top-left to make room. A macOS template image only keeps its alpha channel, so
/// there (`template`) the mark is cut out of an opaque circle and the logo is faded to
/// let the badge stand out; otherwise the badge is drawn in colour. While updating, the
/// circle gives way to a ring with a gap that turns a step per frame.
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
    // in units of the circle's diameter around its centre, y growing downward:
    // a stem below a triangular head whose tip points up
    let arrow = |u: f32, v: f32| {
        let stem = u.abs() <= 0.09 && (-0.04..=0.27).contains(&v);
        let head = (-0.30..=0.0).contains(&v) && u.abs() <= 0.28 * (v + 0.30) / 0.30;
        stem || head
    };
    let in_mark = |x: f32, y: f32| {
        let (u, v) = ((x - cx) / diameter, (y - cy) / diameter);
        match badge {
            // a bar above a dot
            Badge::Failed => {
                let bar = u.abs() <= 0.09 && (-0.30..=0.04).contains(&v);
                let dot = u * u + (v - 0.19).powi(2) <= 0.09 * 0.09;
                bar || dot
            }
            _ => arrow(u, v),
        }
    };
    // three quarters of a ring with round ends, starting further clockwise each frame
    let in_ring = |x: f32, y: f32| {
        use std::f32::consts::TAU;
        let Badge::Updating(frame) = badge else {
            return false;
        };
        let stroke = diameter * 0.08;
        let middle = radius - stroke / 2.0;
        let start = TAU * f32::from(frame) / f32::from(SPINNER_FRAMES);
        let end = start + TAU * 0.75;
        let (dx, dy) = (x - cx, y - cy);
        let along = (dy.atan2(dx) - start).rem_euclid(TAU) <= TAU * 0.75;
        let on_band = (dx.hypot(dy) - middle).abs() <= stroke / 2.0;
        let cap = |angle: f32| {
            (dx - middle * angle.cos()).hypot(dy - middle * angle.sin()) <= stroke / 2.0
        };
        (on_band && along) || cap(start) || cap(end)
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
        let logo_alpha = f32::from(pixel.0[3]) / 255.0 * (1.0 - ring);
        let (badge_alpha, badge_rgb) = if matches!(badge, Badge::Updating(_)) {
            // ring and arrow are drawn solid: there is no circle to cut them out of
            let shape = coverage(x, y, &|sx, sy| in_ring(sx, sy) || in_mark(sx, sy));
            (shape, fill)
        } else {
            let circle = coverage(x, y, &|sx, sy| in_circle(sx, sy, radius));
            let mark = coverage(x, y, &|sx, sy| in_circle(sx, sy, radius) && in_mark(sx, sy));
            if template {
                ((circle - mark).max(0.0), fill)
            } else if circle > 0.0 {
                let white = mark / circle;
                let rgb = [0, 1, 2].map(|c| fill[c] * (1.0 - white) + 255.0 * white);
                (circle, rgb)
            } else {
                (0.0, fill)
            }
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
            (Badge::Updating(0), true),
            (Badge::Updating(7), false),
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
    fn spinner_turns_and_wraps_around() {
        let icon = image::RgbaImage::new(64, 64);
        for template in [true, false] {
            assert_ne!(
                badged_icon(&icon, Badge::Updating(0), template),
                badged_icon(&icon, Badge::Updating(1), template)
            );
            assert_ne!(
                badged_icon(&icon, Badge::Update, template),
                badged_icon(&icon, Badge::Updating(0), template)
            );
        }
        assert_eq!(next_frame(Badge::Updating(0)), Some(Badge::Updating(1)));
        assert_eq!(
            next_frame(Badge::Updating(SPINNER_FRAMES - 1)),
            Some(Badge::Updating(0))
        );
        assert_eq!(next_frame(Badge::Update), None);
    }

    #[test]
    fn a_turning_spinner_is_not_restarted() {
        let spinning = Some(Badge::Updating(0));
        // a status change from downloading to installing keeps the current frame
        assert_eq!(badge_change(Badge::Updating(5), spinning), None);
        assert_eq!(badge_change(Badge::Update, spinning), spinning);
        assert_eq!(
            badge_change(Badge::Updating(5), Some(Badge::Failed)),
            Some(Badge::Failed)
        );
        assert_eq!(badge_change(Badge::Update, Some(Badge::Update)), None);
        assert_eq!(badge_change(Badge::Failed, None), None);
    }

    #[test]
    fn badge_follows_the_update_status() {
        use crate::service::desktop_update::Status;
        assert_eq!(
            update_badge(&Status::Available("1.2.3".into()), true),
            Some(Badge::Update)
        );
        for status in [
            Status::Downloading("1.2.3".into()),
            Status::Installing("1.2.3".into()),
        ] {
            assert_eq!(
                update_badge(&status, true),
                Some(Badge::Updating(0)),
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
        #[cfg(target_os = "windows")]
        {
            use crate::service::desktop_update::Status;
            assert_eq!(status_label(&Status::Idle, false), "Check for Updates");
            assert!(status_label(&Status::Available("1.2.3".into()), true).contains("1.2.3"));
            assert_eq!(
                status_label(&Status::Failed("x".into()), true),
                "Retry Update"
            );
            // a failed check with no known release has nothing to retry
            assert_eq!(
                status_label(&Status::Failed("x".into()), false),
                "Check for Updates"
            );
        }
        assert!(busy(&crate::service::desktop_update::Status::Installing(
            "1.2.3".into()
        )));
    }

    #[test]
    fn hover_text_keeps_the_failure_reason_while_checking_again() {
        use crate::service::desktop_update::Status;
        assert_eq!(
            tooltip(&Status::Failed("offline".into())).as_deref(),
            Some("Update failed: offline")
        );
        assert_eq!(tooltip(&Status::Checking), None);
        assert_eq!(tooltip(&Status::Idle).as_deref(), Some("KMServer"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn menu_keeps_the_on_demand_check_unless_an_update_is_known() {
        use crate::service::desktop_update::Status;
        let plain = |title: &str, enabled| UpdateLine {
            title: title.into(),
            hint: None,
            enabled,
        };
        assert_eq!(
            update_line(&Status::Idle, None),
            plain("Check for Updates", true)
        );
        assert_eq!(
            update_line(&Status::Checking, None),
            plain("Checking for Updates…", false)
        );
        assert_eq!(
            update_line(&Status::Available("1.2.3".into()), Some("1.2.3")),
            UpdateLine {
                title: "Version 1.2.3 available".into(),
                hint: Some("Click to update and restart"),
                enabled: true,
            }
        );
        for status in [
            Status::Downloading("1.2.3".into()),
            Status::Installing("1.2.3".into()),
        ] {
            assert_eq!(
                update_line(&status, Some("1.2.3")),
                plain("Updating to 1.2.3…", false)
            );
        }
        assert_eq!(
            update_line(&Status::Failed("x".into()), Some("1.2.3")),
            UpdateLine {
                title: "Version 1.2.3 available".into(),
                hint: Some("Update failed. Click to try again"),
                enabled: true,
            }
        );
        // a failed check with no known release does not claim one
        assert_eq!(
            update_line(&Status::Failed("x".into()), None),
            plain("Check for Updates", true)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn shell_arguments_preserve_apostrophes() {
        assert_eq!(shell_arg(Path::new("/tmp/KM's app")), "'/tmp/KM'\\''s app'");
    }
}
