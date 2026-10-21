//! The floating recording indicator.
//!
//! This is the window you see while dictating from another app: a small borderless
//! panel showing the level meter and live transcript. The hard requirement is that it
//! must never take keyboard focus — the whole point is that the app you were typing in
//! stays frontmost, so the paste keystroke lands there and not here.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tauri::{AppHandle, Manager, Monitor, PhysicalPosition, PhysicalSize, WebviewWindow};

pub const LABEL: &str = "overlay";

/// Gap between the visible *panel* and the bottom edge of the screen, in logical
/// pixels — scaled by the monitor's DPI factor below.
const PANEL_BOTTOM_GAP_LOGICAL: f64 = 97.0;

/// Transparent margin the window carries below the panel so the panel's shadow has
/// somewhere to fade out instead of being sliced off at the window edge. Mirrors
/// `--panel-gutter-bottom` in `styles.css`; subtracted from the gap above so growing
/// the window for the shadow's sake does not move the panel up the screen.
const SHADOW_GUTTER_BOTTOM_LOGICAL: f64 = 36.0;

/// Configured overlay dimensions in logical pixels — must match the `overlay` window in
/// `tauri.conf.json`, shadow gutters included. Reapply these before every show:
/// Windows can retain a DPI-scaled physical size after displays are connected or moved.
const WIDTH_LOGICAL: f64 = 518.0;
const HEIGHT_LOGICAL: f64 = 250.0;

/// Set while onboarding's "try it" step is on screen. That step drives a real take
/// from our own window on purpose, to show recognized text without sending anyone
/// to another app — and it already renders its own level meter and transcript from
/// the same event stream this window listens to. Showing the overlay on top of it
/// would just be the same take on screen twice.
static SUPPRESSED: AtomicBool = AtomicBool::new(false);

pub fn set_suppressed(suppressed: bool) {
    SUPPRESSED.store(suppressed, Ordering::Relaxed);
}

/// Bumped every time the panel is shown.
///
/// Whoever schedules a delayed `hide` captures this first and hides only if it still
/// matches, so a timer left over from a failure cannot blank the panel of the take the
/// user started in the meantime — the likeliest thing for them to do after being told
/// their last take was cut short.
static GENERATION: AtomicU64 = AtomicU64::new(0);

fn window(app: &AppHandle) -> Option<WebviewWindow> {
    app.get_webview_window(LABEL)
}

/// Hides the panel only if it is still the same appearance `generation` referred to.
pub fn hide_if_unchanged(app: &AppHandle, generation: u64) {
    if GENERATION.load(Ordering::Relaxed) == generation {
        hide(app);
    }
}

/// Shows the overlay without focusing it. Deliberately never calls `set_focus`.
///
/// Returns this appearance's [`GENERATION`], for callers that want to hide it later
/// without stepping on a panel that has since been shown again.
pub fn show(app: &AppHandle) -> u64 {
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    if SUPPRESSED.load(Ordering::Relaxed) {
        return generation;
    }
    let Some(window) = window(app) else {
        log::warn!("overlay window is missing");
        return generation;
    };
    position_bottom_center(&window);
    if let Err(e) = window.show() {
        log::warn!("could not show the overlay: {e}");
    }
    // Showing an already-created window does not restore its place in the native
    // stacking order. In particular, Windows can demote a hidden overlay while
    // displays or other topmost windows change.
    raise_without_focus(&window);
    generation
}

pub fn hide(app: &AppHandle) {
    if let Some(window) = window(app) {
        if let Err(e) = window.hide() {
            log::warn!("could not hide the overlay: {e}");
        }
    }
}

#[cfg(target_os = "windows")]
fn raise_without_focus(window: &WebviewWindow) {
    let overlay = window.clone();
    if let Err(e) = window.run_on_main_thread(move || {
        use std::ffi::c_void;

        #[link(name = "user32")]
        extern "system" {
            fn SetWindowPos(
                hwnd: *mut c_void,
                insert_after: *mut c_void,
                x: i32,
                y: i32,
                width: i32,
                height: i32,
                flags: u32,
            ) -> i32;
        }

        let Ok(hwnd) = overlay.hwnd() else {
            return;
        };
        // Tao caches the always-on-top flag, so setting it to true again can be
        // a no-op even when the OS stacking order has changed. Reassert HWND_TOPMOST
        // directly, preserving geometry and the user's focused paste target.
        const HWND_TOPMOST: *mut c_void = -1isize as *mut c_void;
        // SWP_NOSIZE | SWP_NOMOVE | SWP_NOACTIVATE.
        const FLAGS: u32 = 0x0001 | 0x0002 | 0x0010;
        // SAFETY: the live Tauri window supplies the HWND; this runs on its UI thread.
        if unsafe { SetWindowPos(hwnd.0, HWND_TOPMOST, 0, 0, 0, 0, FLAGS) } == 0 {
            log::warn!(
                "could not raise the overlay: {}",
                std::io::Error::last_os_error()
            );
        }
    }) {
        log::warn!("could not schedule overlay stacking repair: {e}");
    }
}

#[cfg(not(target_os = "windows"))]
fn raise_without_focus(window: &WebviewWindow) {
    if let Err(e) = window.set_always_on_top(true) {
        log::warn!("could not raise the overlay: {e}");
    }
}

/// Places the overlay near the bottom of the monitor containing the pointer on
/// Windows and macOS. Other platforms keep the existing current-monitor behavior.
fn position_bottom_center(window: &WebviewWindow) {
    let monitor = monitor_for_pointer(window).or_else(|| window.current_monitor().ok().flatten());
    let Some(monitor) = monitor else {
        return;
    };
    let scale = monitor.scale_factor();
    let size = overlay_size_on_monitor(scale);
    let area = monitor.size();
    let origin = monitor.position();

    let x = origin.x + ((area.width as i32 - size.width as i32) / 2);
    let window_margin = PANEL_BOTTOM_GAP_LOGICAL - SHADOW_GUTTER_BOTTOM_LOGICAL;
    let y = origin.y + area.height as i32 - size.height as i32 - (window_margin * scale) as i32;

    let position = PhysicalPosition::new(x, y);
    if let Err(e) = window.set_position(position) {
        log::debug!("could not position the overlay: {e}");
    }
    if let Err(e) = window.set_size(size) {
        log::warn!("could not restore the overlay size: {e}");
    }
    // Setting the size can alter the suggested window rectangle during a DPI change.
    // Reapply the target position so the final geometry stays bottom-centered.
    if let Err(e) = window.set_position(position) {
        log::debug!("could not reposition the resized overlay: {e}");
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn monitor_for_pointer(window: &WebviewWindow) -> Option<Monitor> {
    window.cursor_position().ok().and_then(|cursor| {
        window.available_monitors().ok().and_then(|monitors| {
            monitors
                .into_iter()
                .find(|monitor| monitor_contains_point(monitor.position(), monitor.size(), &cursor))
        })
    })
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn monitor_for_pointer(_window: &WebviewWindow) -> Option<Monitor> {
    None
}

fn overlay_size_on_monitor(scale: f64) -> PhysicalSize<u32> {
    tauri::LogicalSize::new(WIDTH_LOGICAL, HEIGHT_LOGICAL).to_physical(scale)
}

fn monitor_contains_point(
    origin: &PhysicalPosition<i32>,
    size: &PhysicalSize<u32>,
    point: &PhysicalPosition<f64>,
) -> bool {
    point.x >= origin.x as f64
        && point.x < origin.x as f64 + size.width as f64
        && point.y >= origin.y as f64
        && point.y < origin.y as f64 + size.height as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_monitor_above_the_primary_display() {
        let primary = PhysicalPosition::new(0, 0);
        let upper = PhysicalPosition::new(0, -1620);
        let primary_size = PhysicalSize::new(2560, 1600);
        let upper_size = PhysicalSize::new(2880, 1620);
        let pointer = PhysicalPosition::new(1554.0, -342.0);

        assert!(!monitor_contains_point(&primary, &primary_size, &pointer));
        assert!(monitor_contains_point(&upper, &upper_size, &pointer));
    }

    #[test]
    fn restores_configured_size_at_target_scale() {
        assert_eq!(overlay_size_on_monitor(1.5), PhysicalSize::new(777, 375));
    }
}
