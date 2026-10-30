//! Falls back to Slint's software renderer where the GPU renderer can't start.
//!
//! The GPU renderer is Skia: Direct3D 12 on Windows (a hardware adapter with
//! feature level 11, else Microsoft's WARP software adapter), OpenGL on Linux
//! (where Skia itself drops to its CPU raster surface if no GL context can be
//! created). Direct3D replaced FemtoVG on OpenGL 2.0, which a Windows machine
//! without a vendor display driver (a fresh install, a Hyper-V/VirtualBox guest
//! with the basic display adapter, some RDP sessions) lacks - the app used to
//! exit there before showing any window ("Failed to initialize OpenGL driver:
//! Could not locate glCreateShader symbol"). Direct3D can still fail on a broken
//! driver or a system without Direct3D 12. Slint's own renderer fallback only
//! covers a renderer that cannot be constructed, not a surface that fails when
//! the window is first shown, and the platform can only be selected once per
//! process - so the app records the failure and restarts itself with the
//! software renderer.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;
use std::{fs, io};

/// Presence of this file under the user data root selects the software
/// renderer on every later launch, so the failed GPU attempt isn't repeated.
const MARKER_FILE: &str = "software-renderer";
/// Set on the restarted process (and usable manually) to select the software
/// renderer even if the marker could not be written.
const SOFTWARE_RENDERER_ENV: &str = "MULTICORE_SOFTWARE_RENDERER";

pub fn software_renderer_requested() -> bool {
    std::env::var_os(SOFTWARE_RENDERER_ENV).is_some()
        || marker_path().is_some_and(|path| path.is_file())
}

/// Linux selects its backend in `select_linux_backend`, passing the renderer.
#[cfg(not(target_os = "linux"))]
pub fn select_software_renderer() -> Result<(), slint::PlatformError> {
    let selector = slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into());
    #[cfg(windows)]
    let selector = selector.with_winit_window_attributes_hook(crate::tray_popup_window_attributes);
    selector.select()
}

/// Skia on Direct3D 12. Without the explicit Direct3D request Slint's Skia
/// renderer draws on the CPU on Windows. `SLINT_BACKEND` (a manual override
/// for diagnostics) keeps Slint's own selection.
#[cfg(windows)]
pub fn select_gpu_renderer() -> Result<(), slint::PlatformError> {
    if std::env::var_os("SLINT_BACKEND").is_some() {
        return Ok(());
    }
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("skia".into())
        .require_d3d()
        .with_winit_window_attributes_hook(crate::tray_popup_window_attributes)
        .select()
}

/// Skia on Metal.
#[cfg(target_os = "macos")]
pub fn select_gpu_renderer() -> Result<(), slint::PlatformError> {
    if std::env::var_os("SLINT_BACKEND").is_some() {
        return Ok(());
    }
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("skia".into())
        .require_metal()
        .select()
}

/// Whether a platform error from creating, showing or drawing a window means the
/// GPU renderer itself failed, as opposed to any other failure. Also the errors
/// that end the event loop for good after `event_loop_guard` could not recover
/// the renderer: Skia spells those in lower case («Error presenting d3d swap
/// chain: 887a0005», «error setting up completion signal for d3d12 command
/// queue»), so the match ignores case.
pub fn is_gpu_renderer_failure(error: &slint::PlatformError) -> bool {
    is_gpu_renderer_failure_text(&error.to_string())
}

fn is_gpu_renderer_failure_text(text: &str) -> bool {
    let text = text.to_lowercase();
    [
        "opengl",
        "glutin",
        "egl",
        "wgl",
        "gl context",
        "suitable renderer",
        // Skia on Direct3D 12 (Windows) and Vulkan.
        "d3d",
        "dxgi",
        "direct3d",
        "swap chain",
        "skia",
        "vulkan",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

/// Upper bound on how long the launcher lingers for the restarted instance.
#[cfg(windows)]
const TAKEOVER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Records the fallback and starts a new instance with the same arguments.
/// The caller must already have released the single-instance guard.
///
/// On Windows the updater rolls an update back if the process it started
/// exits before the new version confirms its start, so this process stays
/// until the restarted instance has confirmed (or exited, or a minute has
/// passed). Its own window, left behind by the failed GPU renderer and no
/// longer serviced, is hidden first so no "not responding" frame shows.
pub fn remember_and_relaunch(arguments: Vec<OsString>) -> io::Result<()> {
    if let Some(path) = marker_path() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Err(error) = fs::write(&path, b"OpenGL 2.0 renderer failed to start\n") {
            eprintln!("software renderer preference not saved: {error}");
        }
    }
    #[cfg(windows)]
    let started = std::time::SystemTime::now();
    #[cfg_attr(not(windows), allow(unused_mut, unused_variables))]
    let mut child = Command::new(std::env::current_exe()?)
        .args(arguments)
        .env(SOFTWARE_RENDERER_ENV, "1")
        .spawn()?;
    #[cfg(windows)]
    {
        hide_own_windows();
        let deadline = std::time::Instant::now() + TAKEOVER_TIMEOUT;
        while std::time::Instant::now() < deadline
            && child.try_wait()?.is_none()
            && !crate::updater::start_confirmed_since(started)
        {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn hide_own_windows() {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, SW_HIDE, ShowWindow,
    };

    unsafe extern "system" fn hide_if_ours(window: HWND, _: LPARAM) -> i32 {
        let mut process_id = 0;
        // SAFETY: `window` comes from EnumWindows and the out pointer is valid.
        unsafe { GetWindowThreadProcessId(window, &mut process_id) };
        // SAFETY: GetCurrentProcessId has no preconditions.
        if process_id == unsafe { GetCurrentProcessId() } {
            // Synchronous: the window belongs to this thread, which no longer
            // pumps messages, so a posted (async) hide would never run.
            // SAFETY: `window` is a valid top-level window handle.
            unsafe { ShowWindow(window, SW_HIDE) };
        }
        1
    }
    // SAFETY: the callback matches WNDENUMPROC and ignores its LPARAM.
    unsafe { EnumWindows(Some(hide_if_ours), 0) };
}

fn marker_path() -> Option<PathBuf> {
    crate::data_root::user_data_root()
        .ok()
        .map(|root| root.join(MARKER_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_renderer_start_failures_trigger_the_fallback() {
        assert!(is_gpu_renderer_failure_text(
            "Failed to initialize OpenGL driver: Could not locate glCreateShader symbol"
        ));
        assert!(is_gpu_renderer_failure_text(
            "Winit backend failed to find a suitable renderer: no display"
        ));
        assert!(is_gpu_renderer_failure_text(
            "error calling D3D12CreateDevice failed. 887a0004"
        ));
        assert!(is_gpu_renderer_failure_text(
            "unable to create DXGIFactory4 failed. 80004005"
        ));
        assert!(is_gpu_renderer_failure_text(
            "unable to create Skia D3D DirectContext"
        ));
        assert!(is_gpu_renderer_failure_text(
            "Direct3D rendering requested but this is only supported on Windows"
        ));
        // Runtime failures of Skia's Direct3D surface, which end the event loop.
        assert!(is_gpu_renderer_failure_text(
            "Error presenting d3d swap chain: 887a0005"
        ));
        assert!(is_gpu_renderer_failure_text(
            "error setting up completion signal for d3d12 command queue failed. 887a0005"
        ));
        assert!(is_gpu_renderer_failure_text(
            "Error resizing swap chain buffers failed. 887a0001"
        ));
        assert!(is_gpu_renderer_failure_text(
            "unable to retrieve swap chain back buffer: 0x887A0005"
        ));
        assert!(!is_gpu_renderer_failure_text(
            "No default Slint platform was selected"
        ));
        assert!(!is_gpu_renderer_failure_text(
            "the event loop kept stopping without a quit request"
        ));
        assert!(!is_gpu_renderer_failure_text("event loop already running"));
    }
}
