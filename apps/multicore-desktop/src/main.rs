#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app_icon;
mod bootstrap;
mod daemon;
mod preferences;
mod single_instance;
mod tray;
mod updater;
mod view_model;
mod windows_settings;
mod windows_shell;

use std::cell::Cell;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use daemon::{DaemonClient, DaemonError, UnavailableDaemonClient};
use preferences::{AppPreferences, PreferenceStore, VisiblePage, WindowBounds};
use slint::winit_030::WinitWindowAccessor;
use slint::{
    CloseRequestResponse, ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel,
};
use tray::{TrayCommand, TrayGroup, TrayInput, TrayMenuModel, build_menu_model};
use updater::{UpdateAction, UpdateState};
use view_model::{
    CatalogDisplayKind, CatalogPresentation, DesktopViewModel, DiagnosticsPresentation,
    LatencyPresentation, LatencyRequest, MutationToken, NodeSelectionRequest, Panel, PrimaryAction,
    QueuedNodeSelection, SemanticTone, StatePresentation, SubscriptionPresentation,
    derive_catalog_display,
};
use windows_settings::LaunchAtSignInState;
use windows_shell::{
    CloseDisposition, MinimizeDisposition, MonitorRect, MonitorSignature, close_disposition,
    minimize_disposition, monitor_cap_changed, next_maximized, parse_resize_edge,
    restore_placement,
};

slint::include_modules!();

#[derive(Clone)]
struct UiSnapshot {
    presentation: StatePresentation,
    panel: Panel,
    import_pending: bool,
    import_error: String,
    subscription: SubscriptionPresentation,
    catalog: CatalogPresentation,
    diagnostics: DiagnosticsPresentation,
    latency: LatencyPresentation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WindowObservation {
    restored_bounds: Option<WindowBounds>,
    maximized: bool,
    visible_page: VisiblePage,
}

struct PersistenceTracker {
    persisted: AppPreferences,
    desired: AppPreferences,
    candidate: Option<WindowObservation>,
    queued: Option<AppPreferences>,
}

impl PersistenceTracker {
    fn new(persisted: AppPreferences) -> Self {
        Self {
            desired: persisted.clone(),
            persisted,
            candidate: None,
            queued: None,
        }
    }

    fn incorporate_observation(&mut self, observation: &WindowObservation) {
        let mut next = self.desired.clone();
        if let Some(bounds) = &observation.restored_bounds {
            next.restored_bounds = Some(bounds.clone());
        }
        next.maximized = observation.maximized;
        next.visible_page = observation.visible_page.clone();
        self.desired = next;
    }

    fn set_ambient_background(&mut self, enabled: bool) {
        self.desired.ambient_background = enabled;
    }

    fn observe(&mut self, observation: Option<WindowObservation>) -> Option<AppPreferences> {
        let Some(observation) = observation else {
            self.candidate = None;
            return None;
        };
        let stable = self.candidate.as_ref() == Some(&observation);
        self.candidate = Some(observation.clone());
        self.incorporate_observation(&observation);
        if !stable || self.desired == self.persisted || self.queued.as_ref() == Some(&self.desired)
        {
            return None;
        }
        self.queued = Some(self.desired.clone());
        self.queued.clone()
    }

    fn mark_persisted(&mut self, preferences: AppPreferences) {
        if self.queued.as_ref() == Some(&preferences) {
            self.queued = None;
        }
        self.persisted = preferences;
    }

    fn snapshot_for_flush(&mut self, observation: Option<&WindowObservation>) -> AppPreferences {
        if let Some(observation) = observation {
            self.incorporate_observation(observation);
        }
        self.desired.clone()
    }

    #[cfg(test)]
    fn latest_desired(&self) -> &AppPreferences {
        &self.desired
    }
}

fn visible_page_name(page: &VisiblePage) -> &'static str {
    match page {
        VisiblePage::Home => "home",
        VisiblePage::Status => "status",
        VisiblePage::Settings => "settings",
    }
}

fn visible_page_from_name(page: &str) -> Option<VisiblePage> {
    match page {
        "home" => Some(VisiblePage::Home),
        "status" => Some(VisiblePage::Status),
        "settings" => Some(VisiblePage::Settings),
        _ => None,
    }
}

fn reduced_motion_from_client_area_animation(enabled: Option<bool>) -> bool {
    matches!(enabled, Some(false))
}

#[cfg(windows)]
fn client_area_animation_enabled() -> Option<bool> {
    const SPI_GETCLIENTAREAANIMATION: u32 = 0x1042;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn SystemParametersInfoW(
            action: u32,
            parameter: u32,
            value: *mut core::ffi::c_void,
            update: u32,
        ) -> i32;
    }

    let mut enabled = 1_i32;
    // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL into the valid `enabled` pointer.
    let succeeded = unsafe {
        SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, (&raw mut enabled).cast(), 0)
    } != 0;
    succeeded.then_some(enabled != 0)
}

#[cfg(not(windows))]
fn client_area_animation_enabled() -> Option<bool> {
    None
}

const PREFERENCE_SAVE_ERROR_RU: &str = "Не удалось сохранить настройки.";
const PREFERENCE_RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(2)];

fn retry_delay_after_failure(attempt: usize) -> Option<Duration> {
    attempt
        .checked_sub(1)
        .and_then(|index| PREFERENCE_RETRY_DELAYS.get(index).copied())
}

enum WriterCommand {
    Write(AppPreferences),
    Stop,
}

enum WriterResult {
    Saved(AppPreferences),
    Failed,
}

struct PreferenceWriter {
    command_tx: Sender<WriterCommand>,
    result_rx: Receiver<WriterResult>,
    handle: Option<thread::JoinHandle<()>>,
    store: Option<PreferenceStore>,
}

impl PreferenceWriter {
    fn start(store: Option<PreferenceStore>) -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let worker_store = store.clone();
        let handle =
            thread::spawn(move || preference_writer_loop(worker_store, command_rx, result_tx));
        Self {
            command_tx,
            result_rx,
            handle: Some(handle),
            store,
        }
    }

    fn enqueue(&self, preferences: AppPreferences) {
        let _ = self.command_tx.send(WriterCommand::Write(preferences));
    }

    fn poll(&self) -> Vec<WriterResult> {
        self.result_rx.try_iter().collect()
    }

    fn stop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = self.command_tx.send(WriterCommand::Stop);
            let _ = handle.join();
        }
    }

    fn stop_and_flush(&mut self, preferences: &AppPreferences) -> bool {
        self.stop();
        self.store
            .as_ref()
            .is_some_and(|store| store.save(preferences).is_ok())
    }
}

impl Drop for PreferenceWriter {
    fn drop(&mut self) {
        self.stop();
    }
}

fn preference_writer_loop(
    store: Option<PreferenceStore>,
    commands: Receiver<WriterCommand>,
    results: Sender<WriterResult>,
) {
    while let Ok(command) = commands.recv() {
        let WriterCommand::Write(mut preferences) = command else {
            break;
        };
        let mut stop_after_write = drain_latest_write(&commands, &mut preferences);
        let mut attempt = 0_usize;
        loop {
            attempt += 1;
            let saved = store
                .as_ref()
                .is_some_and(|store| store.save(&preferences).is_ok());
            if saved {
                let _ = results.send(WriterResult::Saved(preferences));
                if stop_after_write {
                    return;
                }
                break;
            }
            let _ = results.send(WriterResult::Failed);
            if stop_after_write {
                return;
            }
            let Some(retry_delay) = retry_delay_after_failure(attempt) else {
                break;
            };
            match commands.recv_timeout(retry_delay) {
                Ok(WriterCommand::Write(replacement)) => {
                    preferences = replacement;
                    attempt = 0;
                    stop_after_write = drain_latest_write(&commands, &mut preferences);
                }
                Ok(WriterCommand::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }
}

fn drain_latest_write(commands: &Receiver<WriterCommand>, latest: &mut AppPreferences) -> bool {
    loop {
        match commands.try_recv() {
            Ok(WriterCommand::Write(replacement)) => *latest = replacement,
            Ok(WriterCommand::Stop) | Err(TryRecvError::Disconnected) => return true,
            Err(TryRecvError::Empty) => return false,
        }
    }
}

fn main() -> Result<(), slint::PlatformError> {
    let launch_arguments =
        parse_launch_arguments(std::env::args_os().skip(1)).unwrap_or_else(|error| {
            eprintln!("launch arguments rejected: {error}");
            LaunchArguments::foreground()
        });
    let activation = activation_for_launch_arguments(&launch_arguments);
    let (_instance_guard, activations) = match single_instance::claim_or_forward(&activation) {
        Ok(single_instance::Claim::Primary {
            _guard,
            activations,
        }) => (_guard, activations),
        Ok(single_instance::Claim::Forwarded) => return Ok(()),
        Err(error) => {
            eprintln!("single-instance coordination failed: {error}");
            return Ok(());
        }
    };
    let launch_mode = launch_arguments.mode;
    let (client, _bootstrap_guard): (Arc<dyn DaemonClient>, Option<bootstrap::Bootstrap>) =
        match bootstrap::start() {
            Ok(bootstrap) => (bootstrap.client(), Some(bootstrap)),
            Err(error) => {
                eprintln!("bootstrap failed: {error:?}");
                let error = DaemonError::new(error.user_message());
                (Arc::new(UnavailableDaemonClient::new(error)), None)
            }
        };

    let (preference_store, loaded_preferences) = load_preferences();
    let preference_tracker = Arc::new(Mutex::new(PersistenceTracker::new(
        loaded_preferences.clone(),
    )));
    let preference_writer = Arc::new(Mutex::new(PreferenceWriter::start(preference_store)));

    let model = Arc::new(Mutex::new(DesktopViewModel::new(client)));
    let update_state = Arc::new(Mutex::new(UpdateState::initial()));

    let ui = AppWindow::new()?;
    if preference_writer
        .lock()
        .expect("preference writer lock")
        .store
        .is_none()
    {
        ui.set_preference_error(PREFERENCE_SAVE_ERROR_RU.into());
    }
    apply_snapshot(&ui, &snapshot(&model.lock().expect("view model lock")));
    ui.set_local_page(visible_page_name(&loaded_preferences.visible_page).into());
    ui.set_ambient_background_enabled(loaded_preferences.ambient_background);
    ui.set_reduced_motion(reduced_motion_from_client_area_animation(
        client_area_animation_enabled(),
    ));
    if let Some(subscription_url) = launch_arguments.subscription_url {
        ui.set_local_page("settings".into());
        ui.set_subscription_draft(subscription_url.into());
        ui.set_replace_editor_open(true);
    }
    apply_update_state(
        &ui,
        &update_state.lock().expect("update state lock"),
        runtime_allows_update(&model.lock().expect("view model lock").presentation()),
    );
    apply_launch_at_sign_in(&ui, windows_settings::query_launch_at_sign_in());
    apply_saved_placement(&ui, &loaded_preferences);
    wire_import(&ui, model.clone());
    wire_subscription_refresh(&ui, model.clone());
    wire_primary_action(&ui, model.clone());
    wire_panels(&ui, model.clone());
    wire_event_filter(&ui, model.clone());
    wire_catalog_group(&ui, model.clone());
    wire_catalog_node(&ui, model.clone());
    let _latency_timer = wire_automatic_latency(&ui, model.clone());
    wire_windows_settings(&ui);
    wire_external_activations(&ui, activations);
    let tray_model = tray_menu_model(&model.lock().expect("view model lock"));
    let tray_model_source = model.clone();
    let tray_action_ui = ui.as_weak();
    let tray_action_model = model.clone();
    let tray_preference_writer = preference_writer.clone();
    let tray_preference_tracker = preference_tracker.clone();
    let tray_runtime = tray::start(
        tray_model,
        move || tray_menu_model(&tray_model_source.lock().expect("view model lock")),
        move |command| {
            handle_tray_command(
                &tray_action_ui,
                &tray_action_model,
                command,
                &tray_preference_writer,
                &tray_preference_tracker,
            )
        },
    )
    .map_err(|error| eprintln!("tray unavailable: {error}"))
    .ok();
    wire_window_controls(&ui, tray_runtime.is_some());
    let _preference_timer =
        wire_preference_persistence(&ui, preference_writer.clone(), preference_tracker.clone());
    wire_updater(&ui, model.clone(), update_state.clone());
    if updater::configured_repository().ok().flatten().is_some() {
        run_update_check(ui.as_weak(), model.clone(), update_state);
    }
    run_refresh(ui.as_weak(), model);
    let starts_visible = initial_window_visible(launch_mode, tray_runtime.is_some());
    ui.set_shell_active(starts_visible);
    if starts_visible {
        ui.show()?;
    }
    let result = slint::run_event_loop();
    preference_writer
        .lock()
        .expect("preference writer lock")
        .stop();
    let _ = ui.hide();
    drop(tray_runtime);
    result
}

fn activation_for_launch_arguments(arguments: &LaunchArguments) -> single_instance::Activation {
    match (&arguments.subscription_url, arguments.mode) {
        (Some(url), _) => single_instance::Activation::InstallSubscription(url.clone()),
        (None, LaunchMode::Foreground) => single_instance::Activation::Show,
        (None, LaunchMode::Background) => single_instance::Activation::Background,
    }
}

fn prefill_subscription(ui: &AppWindow, subscription_url: String) {
    ui.set_local_page("settings".into());
    ui.set_subscription_draft(subscription_url.into());
    ui.set_replace_editor_open(true);
}

fn wire_external_activations(
    ui: &AppWindow,
    activations: std::sync::mpsc::Receiver<single_instance::Activation>,
) {
    let weak = ui.as_weak();
    thread::spawn(move || {
        while let Ok(activation) = activations.recv() {
            let weak = weak.clone();
            if slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else {
                    return;
                };
                match activation {
                    single_instance::Activation::Background => {}
                    single_instance::Activation::Show => restore_and_focus(&ui),
                    single_instance::Activation::InstallSubscription(url) => {
                        prefill_subscription(&ui, url);
                        restore_and_focus(&ui);
                    }
                }
            })
            .is_err()
            {
                break;
            }
        }
    });
}

fn restore_and_focus(ui: &AppWindow) {
    ui.set_shell_active(true);
    ui.window().set_minimized(false);
    let _ = ui.show();
    let _ = ui.window().with_winit_window(|window| {
        window.set_minimized(false);
        window.set_visible(true);
        window.focus_window();
        window.request_redraw();
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LaunchMode {
    Foreground,
    Background,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LaunchArguments {
    mode: LaunchMode,
    subscription_url: Option<String>,
}

impl LaunchArguments {
    fn foreground() -> Self {
        Self {
            mode: LaunchMode::Foreground,
            subscription_url: None,
        }
    }
}

const INSTALL_SUBSCRIPTION_PREFIX: &str = "multicore://install-sub?url=";

fn parse_launch_arguments(
    args: impl IntoIterator<Item = OsString>,
) -> Result<LaunchArguments, &'static str> {
    let args = args.into_iter().collect::<Vec<_>>();
    match args.as_slice() {
        [] => Ok(LaunchArguments::foreground()),
        [argument] if argument == OsStr::new("--quiet-signal-preview") => {
            Ok(LaunchArguments::foreground())
        }
        [argument] if argument == OsStr::new("--background") => Ok(LaunchArguments {
            mode: LaunchMode::Background,
            subscription_url: None,
        }),
        [argument] => {
            let argument = argument
                .to_str()
                .ok_or("launch argument must be valid Unicode")?;
            let encoded = argument
                .strip_prefix(INSTALL_SUBSCRIPTION_PREFIX)
                .ok_or("unsupported launch argument")?;
            let subscription_url = decode_percent_encoded_url(encoded)?;
            let parsed = reqwest::Url::parse(&subscription_url)
                .map_err(|_| "subscription URL is malformed")?;
            if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
                return Err("subscription URL must use HTTP or HTTPS");
            }
            Ok(LaunchArguments {
                mode: LaunchMode::Foreground,
                subscription_url: Some(subscription_url),
            })
        }
        _ => Err("background mode and installer URI cannot be combined"),
    }
}

fn decode_percent_encoded_url(encoded: &str) -> Result<String, &'static str> {
    if encoded.is_empty() || !encoded.as_bytes().contains(&b'%') {
        return Err("subscription URL must be percent-encoded");
    }

    let mut decoded = Vec::with_capacity(encoded.len());
    let bytes = encoded.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let high = *bytes.get(index + 1).ok_or("incomplete percent escape")?;
                let low = *bytes.get(index + 2).ok_or("incomplete percent escape")?;
                decoded.push(
                    hex_value(high)
                        .and_then(|high| hex_value(low).map(|low| high * 16 + low))
                        .ok_or("invalid percent escape")?,
                );
                index += 3;
            }
            byte if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') => {
                decoded.push(byte);
                index += 1;
            }
            _ => return Err("subscription URL contains an unescaped character"),
        }
    }

    String::from_utf8(decoded).map_err(|_| "subscription URL is not valid UTF-8")
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn initial_window_visible(launch_mode: LaunchMode, tray_available: bool) -> bool {
    launch_mode == LaunchMode::Foreground || !tray_available
}

fn smoke_close_requested() -> bool {
    std::env::var_os("MULTICORE_SMOKE_EXIT_ON_CLOSE").as_deref() == Some(std::ffi::OsStr::new("1"))
}

fn runtime_allows_update(presentation: &StatePresentation) -> bool {
    !presentation.is_connected
        && !presentation.is_busy
        && !presentation.is_error
        && !presentation.is_degraded
}

fn apply_update_state(ui: &AppWindow, state: &UpdateState, runtime_idle: bool) {
    let presentation = state.presentation(runtime_idle);
    ui.set_update_status(presentation.status.into());
    ui.set_update_detail(presentation.detail.into());
    ui.set_update_action_label(presentation.action_label.into());
    ui.set_update_action_enabled(presentation.action_enabled);
    ui.set_update_available(presentation.available);
    ui.set_update_busy(presentation.busy);
}

fn load_preferences() -> (Option<PreferenceStore>, AppPreferences) {
    let store = match PreferenceStore::from_local_app_data() {
        Ok(store) => store,
        Err(_) => {
            eprintln!("preferences unavailable");
            return (None, AppPreferences::default());
        }
    };
    let preferences = match store.load() {
        Ok(preferences) => preferences,
        Err(_) => {
            eprintln!("preferences load failed");
            AppPreferences::default()
        }
    };
    (Some(store), preferences)
}

#[cfg(windows)]
fn monitor_work_area(
    monitor: &slint::winit_030::winit::monitor::MonitorHandle,
) -> Option<(
    slint::winit_030::winit::dpi::PhysicalPosition<i32>,
    slint::winit_030::winit::dpi::PhysicalSize<u32>,
)> {
    use slint::winit_030::winit::dpi::{PhysicalPosition, PhysicalSize};
    use slint::winit_030::winit::platform::windows::MonitorHandleExtWindows;

    #[repr(C)]
    #[derive(Default)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct MonitorInfo {
        size: u32,
        monitor: Rect,
        work: Rect,
        flags: u32,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetMonitorInfoW(monitor: isize, info: *mut MonitorInfo) -> i32;
    }

    let mut info = MonitorInfo {
        size: std::mem::size_of::<MonitorInfo>() as u32,
        ..MonitorInfo::default()
    };
    // SAFETY: `info` has the Win32 MONITORINFO layout and remains writable for the call.
    if unsafe { GetMonitorInfoW(monitor.hmonitor(), &raw mut info) } == 0 {
        return None;
    }
    let width = u32::try_from(info.work.right.checked_sub(info.work.left)?).ok()?;
    let height = u32::try_from(info.work.bottom.checked_sub(info.work.top)?).ok()?;
    (width > 0 && height > 0).then_some((
        PhysicalPosition::new(info.work.left, info.work.top),
        PhysicalSize::new(width, height),
    ))
}

#[cfg(not(windows))]
fn monitor_work_area(
    monitor: &slint::winit_030::winit::monitor::MonitorHandle,
) -> Option<(
    slint::winit_030::winit::dpi::PhysicalPosition<i32>,
    slint::winit_030::winit::dpi::PhysicalSize<u32>,
)> {
    Some((monitor.position(), monitor.size()))
}

fn monitor_rects(window: &slint::winit_030::winit::window::Window) -> Vec<MonitorRect> {
    let primary = window.primary_monitor();
    window
        .available_monitors()
        .filter_map(|monitor| {
            let scale = monitor.scale_factor();
            let scale_milli = scale_factor_milli(scale)?;
            let (position, size) = monitor_work_area(&monitor)?;
            Some(MonitorRect {
                x: position.x,
                y: position.y,
                width: size.width,
                height: size.height,
                scale_milli,
                primary: primary.as_ref() == Some(&monitor),
            })
        })
        .collect()
}

fn scale_factor_milli(scale: f64) -> Option<u32> {
    if !scale.is_finite() || !(0.5..=8.0).contains(&scale) {
        return None;
    }
    Some((scale * 1_000.0).round() as u32)
}

fn current_monitor_signature(
    window: &slint::winit_030::winit::window::Window,
) -> Option<MonitorSignature> {
    let monitor = window.current_monitor()?;
    let (position, size) = monitor_work_area(&monitor)?;
    Some(MonitorSignature {
        x: position.x,
        y: position.y,
        width: size.width,
        height: size.height,
        scale_milli: scale_factor_milli(monitor.scale_factor())?,
    })
}

fn set_current_monitor_max_size(
    window: &slint::winit_030::winit::window::Window,
) -> Option<MonitorSignature> {
    use slint::winit_030::winit::dpi::LogicalSize;

    let signature = current_monitor_signature(window);
    let maximum = signature.map(|signature| {
        let width = u64::from(signature.width) * 1_000 / u64::from(signature.scale_milli);
        let height = u64::from(signature.height) * 1_000 / u64::from(signature.scale_milli);
        LogicalSize::new(
            width.min(u64::from(u32::MAX)) as u32,
            height.min(u64::from(u32::MAX)) as u32,
        )
    });
    window.set_max_inner_size(maximum);
    signature
}

fn apply_saved_placement(ui: &AppWindow, preferences: &AppPreferences) {
    use slint::winit_030::winit::dpi::{LogicalSize, PhysicalPosition};

    let saved = preferences.restored_bounds.clone();
    let maximized = preferences.maximized;
    let _ = ui.window().with_winit_window(|window| {
        let monitors = monitor_rects(window);
        let mut restored = false;
        if let Some(placement) = saved.and_then(|saved| restore_placement(saved, &monitors)) {
            let _ = window.request_inner_size(LogicalSize::new(
                placement.inner_size.width,
                placement.inner_size.height,
            ));
            window.set_outer_position(PhysicalPosition::new(
                placement.position.x,
                placement.position.y,
            ));
            window.set_max_inner_size(Some(LogicalSize::new(
                placement.max_inner_size.width,
                placement.max_inner_size.height,
            )));
            restored = true;
        }
        if !restored {
            let _ = set_current_monitor_max_size(window);
        }
        window.set_maximized(maximized);
    });
}

fn observe_window(ui: &AppWindow) -> Option<WindowObservation> {
    let visible_page = visible_page_from_name(ui.get_local_page().as_str())?;
    ui.window().with_winit_window(|window| {
        if window.is_minimized() == Some(true) {
            return None;
        }
        let maximized = window.is_maximized();
        let restored_bounds = if maximized {
            None
        } else {
            let scale = window.scale_factor();
            let position = window.outer_position().ok()?;
            let size = window.inner_size().to_logical::<u32>(scale);
            Some(WindowBounds {
                x: position.x,
                y: position.y,
                width: size.width,
                height: size.height,
            })
        };
        Some(WindowObservation {
            restored_bounds,
            maximized,
            visible_page,
        })
    })?
}

fn process_writer_results(
    ui: &AppWindow,
    writer: &PreferenceWriter,
    tracker: &Arc<Mutex<PersistenceTracker>>,
) {
    for result in writer.poll() {
        ui.set_preference_error(preference_error_for_result(&result).into());
        match result {
            WriterResult::Saved(preferences) => {
                tracker
                    .lock()
                    .expect("preference tracker lock")
                    .mark_persisted(preferences);
            }
            WriterResult::Failed => {
                eprintln!("preferences save failed");
            }
        }
    }
}

fn preference_error_for_result(result: &WriterResult) -> &'static str {
    match result {
        WriterResult::Saved(_) => "",
        WriterResult::Failed => PREFERENCE_SAVE_ERROR_RU,
    }
}

fn flush_preferences(
    ui: &AppWindow,
    writer: &Arc<Mutex<PreferenceWriter>>,
    tracker: &Arc<Mutex<PersistenceTracker>>,
) {
    let observation = observe_window(ui);
    let preferences = {
        let mut tracker = tracker.lock().expect("preference tracker lock");
        tracker.set_ambient_background(ui.get_ambient_background_enabled());
        tracker.snapshot_for_flush(observation.as_ref())
    };
    let saved = writer
        .lock()
        .expect("preference writer lock")
        .stop_and_flush(&preferences);
    if saved {
        tracker
            .lock()
            .expect("preference tracker lock")
            .mark_persisted(preferences);
        ui.set_preference_error("".into());
    } else {
        ui.set_preference_error(PREFERENCE_SAVE_ERROR_RU.into());
        eprintln!("preferences save failed");
    }
}

fn wire_preference_persistence(
    ui: &AppWindow,
    writer: Arc<Mutex<PreferenceWriter>>,
    tracker: Arc<Mutex<PersistenceTracker>>,
) -> Timer {
    let timer = Timer::default();
    let weak = ui.as_weak();
    let last_monitor_signature = Cell::new(None);
    timer.start(TimerMode::Repeated, Duration::from_millis(500), move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if let Some(current) = ui.window().with_winit_window(current_monitor_signature) {
            let previous = last_monitor_signature.get();
            if monitor_cap_changed(previous, current)
                && let Some(applied) = ui.window().with_winit_window(set_current_monitor_max_size)
            {
                last_monitor_signature.set(applied);
            }
        }
        process_writer_results(
            &ui,
            &writer.lock().expect("preference writer lock"),
            &tracker,
        );
        let observation = observe_window(&ui);
        let preferences = {
            let mut tracker = tracker.lock().expect("preference tracker lock");
            tracker.set_ambient_background(ui.get_ambient_background_enabled());
            tracker.observe(observation)
        };
        if let Some(preferences) = preferences {
            writer
                .lock()
                .expect("preference writer lock")
                .enqueue(preferences);
        }
    });
    timer
}

fn wire_updater(
    ui: &AppWindow,
    model: Arc<Mutex<DesktopViewModel>>,
    state: Arc<Mutex<UpdateState>>,
) {
    let weak = ui.as_weak();
    ui.on_update_action(move || {
        let runtime_idle =
            runtime_allows_update(&model.lock().expect("view model lock").presentation());
        let action = state
            .lock()
            .expect("update state lock")
            .presentation(runtime_idle)
            .action;
        match action {
            UpdateAction::Check => run_update_check(weak.clone(), model.clone(), state.clone()),
            UpdateAction::Install if runtime_idle => {
                let release = state.lock().expect("update state lock").available_release();
                let Some(release) = release else {
                    return;
                };
                {
                    let mut current = state.lock().expect("update state lock");
                    *current = UpdateState::Downloading(release.clone());
                    if let Some(ui) = weak.upgrade() {
                        apply_update_state(&ui, &current, true);
                    }
                }
                let weak = weak.clone();
                let state = state.clone();
                thread::spawn(move || match updater::download_and_start_apply(&release) {
                    Ok(()) => {
                        let _ = slint::invoke_from_event_loop(move || {
                            let _keep_state_alive = state;
                            let _keep_window_alive = weak;
                            let _ = slint::quit_event_loop();
                        });
                    }
                    Err(error) => {
                        let _ = slint::invoke_from_event_loop(move || {
                            let mut current = state.lock().expect("update state lock");
                            *current = UpdateState::Error(error);
                            if let Some(ui) = weak.upgrade() {
                                apply_update_state(&ui, &current, true);
                            }
                        });
                    }
                });
            }
            UpdateAction::None | UpdateAction::Install => {}
        }
    });
}

fn run_update_check(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    state: Arc<Mutex<UpdateState>>,
) {
    {
        let mut current = state.lock().expect("update state lock");
        *current = UpdateState::Checking;
        if let Some(ui) = weak.upgrade() {
            let runtime_idle =
                runtime_allows_update(&model.lock().expect("view model lock").presentation());
            apply_update_state(&ui, &current, runtime_idle);
        }
    }
    thread::spawn(move || {
        let result = updater::check_latest().unwrap_or_else(UpdateState::Error);
        let _ = slint::invoke_from_event_loop(move || {
            let runtime_idle =
                runtime_allows_update(&model.lock().expect("view model lock").presentation());
            let mut current = state.lock().expect("update state lock");
            *current = result;
            if let Some(ui) = weak.upgrade() {
                apply_update_state(&ui, &current, runtime_idle);
            }
        });
    });
}

fn wire_window_controls(ui: &AppWindow, tray_available: bool) {
    let weak = ui.as_weak();
    ui.on_window_drag(move || {
        if let Some(ui) = weak.upgrade() {
            let _ = ui.window().with_winit_window(|window| window.drag_window());
        }
    });

    let weak = ui.as_weak();
    ui.on_window_minimize(move || match minimize_disposition(tray_available) {
        MinimizeDisposition::HideToTray => {
            if let Some(ui) = weak.upgrade() {
                ui.set_shell_active(false);
                let _ = ui.hide();
            }
        }
        MinimizeDisposition::MinimizeToTaskbar => {
            if let Some(ui) = weak.upgrade() {
                ui.set_shell_active(false);
                ui.window().set_minimized(true);
            }
        }
    });

    let weak = ui.as_weak();
    ui.on_window_resize(move |edge| {
        let Some(edge) = parse_resize_edge(edge.as_str()) else {
            return;
        };
        if let Some(ui) = weak.upgrade() {
            let _ = ui.window().with_winit_window(|window| {
                let _ = set_current_monitor_max_size(window);
                #[cfg(windows)]
                let _ = window.drag_resize_window(edge.into());
                #[cfg(not(windows))]
                let _ = edge;
            });
        }
    });

    let weak = ui.as_weak();
    ui.on_window_toggle_maximize(move || {
        if let Some(ui) = weak.upgrade() {
            let window = ui.window();
            let _ = window.with_winit_window(set_current_monitor_max_size);
            window.set_maximized(next_maximized(window.is_maximized()));
        }
    });

    let weak = ui.as_weak();
    ui.on_window_close(move || {
        close_window(&weak, tray_available);
    });

    let weak = ui.as_weak();
    ui.window().on_close_requested(move || {
        close_window(&weak, tray_available);
        CloseRequestResponse::KeepWindowShown
    });
}

fn close_window(weak: &slint::Weak<AppWindow>, tray_available: bool) {
    match close_disposition(tray_available, smoke_close_requested()) {
        CloseDisposition::HideToTray => {
            if let Some(ui) = weak.upgrade() {
                ui.set_shell_active(false);
                let _ = ui.hide();
            }
        }
        CloseDisposition::Exit => {
            let _ = slint::quit_event_loop();
        }
    }
}

fn tray_menu_model(model: &DesktopViewModel) -> TrayMenuModel {
    let presentation = model.presentation();
    let catalog = model.catalog_presentation();
    let status = if presentation.is_connected {
        format!("В сети · {}", presentation.node)
    } else {
        presentation.headline.clone()
    };
    build_menu_model(TrayInput {
        status,
        connection_label: presentation.primary_label.into(),
        connection_enabled: presentation.primary_enabled
            && presentation.primary_action != PrimaryAction::None,
        groups: catalog
            .groups
            .into_iter()
            .map(|group| TrayGroup {
                id: group.id,
                label: group.label,
                selected: group.selected,
            })
            .collect(),
        selected_group_id: catalog.selected_group_id,
        nodes: catalog
            .nodes
            .into_iter()
            .map(|node| (node.id, node.label, node.selected))
            .collect(),
        route_selection_enabled: catalog.selection_enabled,
    })
}

fn handle_tray_command(
    weak: &slint::Weak<AppWindow>,
    model: &Arc<Mutex<DesktopViewModel>>,
    command: TrayCommand,
    preference_writer: &Arc<Mutex<PreferenceWriter>>,
    preference_tracker: &Arc<Mutex<PersistenceTracker>>,
) {
    let Some(ui) = weak.upgrade() else {
        return;
    };
    match command {
        TrayCommand::ShowWindow => {
            ui.set_shell_active(true);
            ui.window().set_minimized(false);
            let _ = ui.show();
            let _ = ui.window().with_winit_window(|window| {
                window.set_minimized(false);
                window.set_visible(true);
                window.focus_window();
                window.request_redraw();
            });
        }
        TrayCommand::ToggleConnection => ui.invoke_primary_action(),
        TrayCommand::SelectGroup { group_id } => {
            ui.invoke_select_catalog_group(group_id.into());
        }
        TrayCommand::SelectNode { group_id, node_id } => {
            let selected_group = model
                .lock()
                .expect("view model lock")
                .catalog_presentation()
                .selected_group_id;
            if selected_group.as_deref() != Some(group_id.as_str()) {
                ui.invoke_select_catalog_group(group_id.into());
            }
            ui.invoke_select_catalog_node(node_id.into());
        }
        TrayCommand::Exit => {
            flush_preferences(&ui, preference_writer, preference_tracker);
            let _ = slint::quit_event_loop();
        }
    }
}

fn wire_windows_settings(ui: &AppWindow) {
    let weak = ui.as_weak();
    ui.on_set_launch_at_sign_in(move |enabled| {
        if let Some(ui) = weak.upgrade() {
            apply_launch_at_sign_in(&ui, windows_settings::set_launch_at_sign_in(enabled));
        }
    });
}

fn apply_launch_at_sign_in(ui: &AppWindow, state: std::io::Result<LaunchAtSignInState>) {
    match state {
        Ok(LaunchAtSignInState::Enabled) => {
            ui.set_launch_at_sign_in_enabled(true);
            ui.set_launch_at_sign_in_stale(false);
            ui.set_launch_at_sign_in_error("".into());
        }
        Ok(LaunchAtSignInState::Disabled) => {
            ui.set_launch_at_sign_in_enabled(false);
            ui.set_launch_at_sign_in_stale(false);
            ui.set_launch_at_sign_in_error("".into());
        }
        Ok(LaunchAtSignInState::Stale { .. }) => {
            ui.set_launch_at_sign_in_enabled(false);
            ui.set_launch_at_sign_in_stale(true);
            ui.set_launch_at_sign_in_error("".into());
        }
        Err(_) => {
            ui.set_launch_at_sign_in_enabled(false);
            ui.set_launch_at_sign_in_stale(false);
            ui.set_launch_at_sign_in_error(
                "Не удалось изменить автозапуск. Проверьте права пользователя.".into(),
            );
        }
    }
}

fn wire_import(ui: &AppWindow, model: Arc<Mutex<DesktopViewModel>>) {
    let weak = ui.as_weak();
    ui.on_import_url(move |url| {
        let url = url.trim().to_owned();
        let (mutation, client, current) = {
            let mut model = model.lock().expect("view model lock");
            let mutation = model.begin_import(&url);
            let client = model.daemon_client();
            (mutation, client, snapshot(&model))
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
        }
        let Some(mutation) = mutation else {
            return;
        };

        run_import(weak.clone(), model.clone(), client, mutation, url);
    });
}

fn wire_subscription_refresh(ui: &AppWindow, model: Arc<Mutex<DesktopViewModel>>) {
    let weak = ui.as_weak();
    ui.on_refresh_subscription(move || {
        let (mutation, client, current, needs_url) = {
            let mut model = model.lock().expect("view model lock");
            let available = model.subscription_presentation().refresh_available;
            let mutation = model.begin_subscription_refresh();
            (
                mutation,
                model.daemon_client(),
                snapshot(&model),
                !available,
            )
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
            if needs_url {
                ui.set_replace_editor_open(true);
            }
        }
        let Some(mutation) = mutation else {
            return;
        };
        run_subscription_refresh(weak.clone(), model.clone(), client, mutation);
    });
}

fn wire_primary_action(ui: &AppWindow, model: Arc<Mutex<DesktopViewModel>>) {
    let weak = ui.as_weak();
    ui.on_primary_action(move || {
        let (action, client, queued, current) = {
            let mut model = model.lock().expect("view model lock");
            let action = model.begin_primary_action();
            let client = model.daemon_client();
            let queued = model.queued_node_selections();
            (action, client, queued, snapshot(&model))
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
        }

        match action {
            Some((PrimaryAction::Connect, mutation)) => {
                run_connect(weak.clone(), model.clone(), client, mutation, queued);
            }
            Some((PrimaryAction::Disconnect | PrimaryAction::Cleanup, mutation)) => {
                run_disconnect(weak.clone(), model.clone(), client, mutation);
            }
            Some((PrimaryAction::Retry, mutation)) => {
                run_status(weak.clone(), model.clone(), client, mutation);
            }
            Some((PrimaryAction::None, _)) | None => {}
        }
    });
}

fn wire_panels(ui: &AppWindow, model: Arc<Mutex<DesktopViewModel>>) {
    let weak = ui.as_weak();
    let open_model = model.clone();
    ui.on_open_panel(move |name| {
        let panel = match name.as_str() {
            "subscription" => Panel::Subscription,
            "routes" => Panel::Routes,
            "events" => Panel::Events,
            _ => Panel::None,
        };
        let current = {
            let mut model = open_model.lock().expect("view model lock");
            model.open_panel(panel);
            snapshot(&model)
        };
        if let Some(ui) = weak.upgrade() {
            if panel == Panel::Events {
                ui.set_event_filter("Все".into());
            } else if panel == Panel::Routes {
                ui.set_local_page("home".into());
            }
            apply_snapshot(&ui, &current);
        }
        if panel == Panel::Events {
            let cached = open_model
                .lock()
                .expect("view model lock")
                .set_event_filter(None);
            if let Some(ui) = weak.upgrade() {
                apply_events(&ui, cached);
            }
            load_events(weak.clone(), open_model.clone());
            load_diagnostics(weak.clone(), open_model.clone());
        } else if panel == Panel::Routes {
            load_catalog(weak.clone(), open_model.clone());
        }
    });

    let weak = ui.as_weak();
    let close_model = model.clone();
    ui.on_close_panel(move || {
        close_panel(&weak, &close_model, false);
    });

    let weak = ui.as_weak();
    ui.on_escape_pressed(move || {
        close_panel(&weak, &model, true);
    });
}

fn wire_catalog_group(ui: &AppWindow, model: Arc<Mutex<DesktopViewModel>>) {
    let weak = ui.as_weak();
    ui.on_select_catalog_group(move |id| {
        let current = {
            let mut model = model.lock().expect("view model lock");
            model
                .select_catalog_group(id.as_str())
                .then(|| snapshot(&model))
        };
        if let (Some(ui), Some(current)) = (weak.upgrade(), current) {
            apply_snapshot(&ui, &current);
            begin_latency_check(weak.clone(), model.clone());
        }
    });
}

fn wire_catalog_node(ui: &AppWindow, model: Arc<Mutex<DesktopViewModel>>) {
    let weak = ui.as_weak();
    ui.on_select_catalog_node(move |id| {
        let (request, current) = {
            let mut model = model.lock().expect("view model lock");
            let request = model.begin_node_selection(id.as_str());
            (request, snapshot(&model))
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
        }
        let Some(request) = request else {
            return;
        };
        run_node_selection(weak.clone(), model.clone(), request);
    });
}

fn wire_automatic_latency(ui: &AppWindow, model: Arc<Mutex<DesktopViewModel>>) -> Timer {
    let timer = Timer::default();
    let weak = ui.as_weak();
    timer.start(TimerMode::Repeated, Duration::from_secs(60), move || {
        if weak.upgrade().is_some_and(|ui| ui.window().is_visible()) {
            begin_latency_check(weak.clone(), model.clone());
        }
    });
    timer
}

fn begin_latency_check(weak: slint::Weak<AppWindow>, model: Arc<Mutex<DesktopViewModel>>) {
    let (request, current) = {
        let mut model = model.lock().expect("view model lock");
        let request = model.begin_latency_request();
        (request, snapshot(&model))
    };
    if let Some(ui) = weak.upgrade() {
        apply_snapshot(&ui, &current);
    }
    if let Some(request) = request {
        run_latency_check(weak, model, request);
    }
}

fn wire_event_filter(ui: &AppWindow, model: Arc<Mutex<DesktopViewModel>>) {
    let weak = ui.as_weak();
    ui.on_filter_events(move |filter| {
        let filter = filter.to_string();
        if let Some(ui) = weak.upgrade() {
            ui.set_event_filter(filter.clone().into());
        }
        let level = match filter.as_str() {
            "Ошибки" => Some("error".to_owned()),
            "Система" => Some("system".to_owned()),
            _ => None,
        };
        let events = model
            .lock()
            .expect("view model lock")
            .set_event_filter(level.as_deref());
        if let Some(ui) = weak.upgrade() {
            apply_events(&ui, events);
        }
    });
}

fn close_panel(
    weak: &slint::Weak<AppWindow>,
    model: &Arc<Mutex<DesktopViewModel>>,
    from_escape: bool,
) {
    let current = {
        let mut model = model.lock().expect("view model lock");
        if from_escape {
            model.close_panel_on_escape();
        } else {
            model.open_panel(Panel::None);
        }
        snapshot(&model)
    };
    if let Some(ui) = weak.upgrade() {
        apply_snapshot(&ui, &current);
    }
}

fn run_refresh(weak: slint::Weak<AppWindow>, model: Arc<Mutex<DesktopViewModel>>) {
    let (client, mutation, current) = {
        let mut model = model.lock().expect("view model lock");
        let mutation = model.begin_refresh();
        (model.daemon_client(), mutation, snapshot(&model))
    };
    if let Some(ui) = weak.upgrade() {
        apply_snapshot(&ui, &current);
    }
    run_status(weak, model, client, mutation);
}

fn run_status(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    client: Arc<dyn DaemonClient>,
    mutation: MutationToken,
) {
    thread::spawn(move || {
        let result = client.status();
        let refresh_catalog = result.is_ok();
        let current = {
            let mut model = model.lock().expect("view model lock");
            model.finish_refresh(mutation, result);
            snapshot(&model)
        };
        apply_snapshot_from_worker(weak, current, model, refresh_catalog, false);
    });
}

fn run_import(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    client: Arc<dyn DaemonClient>,
    mutation: MutationToken,
    url: String,
) {
    thread::spawn(move || {
        let result = client.import_subscription(&url);
        let import_succeeded = result.is_ok();
        let refresh_catalog = import_succeeded;
        let current = {
            let mut model = model.lock().expect("view model lock");
            model.finish_import(mutation, result);
            snapshot(&model)
        };
        apply_snapshot_from_worker(weak, current, model, refresh_catalog, import_succeeded);
    });
}

fn run_connect(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    client: Arc<dyn DaemonClient>,
    mutation: MutationToken,
    queued: Vec<QueuedNodeSelection>,
) {
    thread::spawn(move || {
        let result = client.connect();
        let connect_succeeded = result.is_ok();
        let queued_results = if connect_succeeded {
            queued
                .iter()
                .map(|queued| {
                    client.select_node(&queued.group_id, queued.revision, &queued.node_id)
                })
                .collect()
        } else {
            Vec::new()
        };
        let refresh_catalog = result.is_ok();
        let current = {
            let mut model = model.lock().expect("view model lock");
            model.finish_connect(mutation, result);
            if connect_succeeded && !queued.is_empty() {
                model.finish_queued_node_selections(&queued, queued_results);
            }
            snapshot(&model)
        };
        apply_snapshot_from_worker(weak, current, model, refresh_catalog, false);
    });
}

fn run_subscription_refresh(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    client: Arc<dyn DaemonClient>,
    mutation: MutationToken,
) {
    thread::spawn(move || {
        let result = client.refresh_subscription();
        let current = {
            let mut model = model.lock().expect("view model lock");
            let succeeded = model.finish_subscription_refresh(mutation, result);
            (snapshot(&model), succeeded)
        };
        apply_snapshot_from_worker(weak, current.0, model, current.1, false);
    });
}

fn run_disconnect(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    client: Arc<dyn DaemonClient>,
    mutation: MutationToken,
) {
    thread::spawn(move || {
        let result = client.disconnect();
        let (reconciliation, current) = {
            let mut model = model.lock().expect("view model lock");
            let reconcile_status = model.finish_disconnect(mutation, result);
            let reconciliation = reconcile_status.then(|| model.begin_disconnect_reconciliation());
            (reconciliation, snapshot(&model))
        };
        apply_snapshot_from_worker(weak.clone(), current, model.clone(), false, false);
        if let Some(reconciliation) = reconciliation {
            run_disconnect_reconciliation(weak, model, client, reconciliation);
        }
    });
}

fn run_disconnect_reconciliation(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    client: Arc<dyn DaemonClient>,
    mutation: MutationToken,
) {
    thread::spawn(move || {
        let result = client.status();
        let refresh_catalog = result.is_ok();
        let current = {
            let mut model = model.lock().expect("view model lock");
            model
                .finish_disconnect_reconciliation(mutation, result)
                .then(|| snapshot(&model))
        };
        let Some(current) = current else {
            return;
        };
        apply_snapshot_from_worker(weak, current, model, refresh_catalog, false);
    });
}

fn apply_snapshot_from_worker(
    weak: slint::Weak<AppWindow>,
    current: UiSnapshot,
    model: Arc<Mutex<DesktopViewModel>>,
    refresh_catalog: bool,
    import_succeeded: bool,
) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
            if import_succeeded {
                ui.set_import_success_serial(ui.get_import_success_serial().wrapping_add(1));
            }
        }
        if refresh_catalog && current.presentation.has_profile {
            load_catalog(weak, model);
        }
    });
}

fn load_events(weak: slint::Weak<AppWindow>, model: Arc<Mutex<DesktopViewModel>>) {
    let request = model
        .lock()
        .expect("view model lock")
        .begin_events_request();
    let Some(request) = request else {
        return;
    };
    thread::spawn(move || {
        let result = request
            .client
            .events(request.after, request.known_epoch.as_deref());
        let events = model
            .lock()
            .expect("view model lock")
            .finish_events(request.token, result);
        let Some(events) = events else {
            return;
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_events(&ui, events);
            }
        });
    });
}

fn load_diagnostics(weak: slint::Weak<AppWindow>, model: Arc<Mutex<DesktopViewModel>>) {
    let (request, current) = {
        let mut model = model.lock().expect("view model lock");
        let request = model.begin_diagnostics_request();
        (request, snapshot(&model))
    };
    if let Some(ui) = weak.upgrade() {
        apply_snapshot(&ui, &current);
    }
    let Some(request) = request else {
        return;
    };
    thread::spawn(move || {
        let result = request.client.diagnostics();
        let completion = {
            let mut model = model.lock().expect("view model lock");
            model
                .finish_diagnostics(request.token, result)
                .map(|events| (snapshot(&model), events))
        };
        let Some((current, events)) = completion else {
            return;
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_snapshot(&ui, &current);
                apply_events(&ui, events);
            }
        });
    });
}

fn load_catalog(weak: slint::Weak<AppWindow>, model: Arc<Mutex<DesktopViewModel>>) {
    let (request, current) = {
        let mut model = model.lock().expect("view model lock");
        let request = model.begin_catalog_request();
        (request, snapshot(&model))
    };
    if let Some(ui) = weak.upgrade() {
        apply_snapshot(&ui, &current);
    }
    let Some(request) = request else {
        return;
    };
    thread::spawn(move || {
        let result = request.client.catalog();
        let current = {
            let mut model = model.lock().expect("view model lock");
            model
                .finish_catalog(request.token, result)
                .map(|_| snapshot(&model))
        };
        let Some(current) = current else {
            return;
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_snapshot(&ui, &current);
            }
            if current.latency.queued {
                begin_latency_check(weak, model);
            }
        });
    });
}

fn run_latency_check(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    request: LatencyRequest,
) {
    thread::spawn(move || {
        let result = request
            .client
            .latencies(request.catalog_revision, &request.group_ids);
        let completion = {
            let mut model = model.lock().expect("view model lock");
            model
                .finish_latencies(request.token, result)
                .map(|outcome| (snapshot(&model), outcome))
        };
        let Some((current, outcome)) = completion else {
            return;
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_snapshot(&ui, &current);
            }
            if outcome.refresh_catalog {
                load_catalog(weak, model);
            } else if outcome.rerun {
                begin_latency_check(weak, model);
            }
        });
    });
}

fn run_node_selection(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    request: NodeSelectionRequest,
) {
    thread::spawn(move || {
        let result =
            request
                .client
                .select_node(&request.group_id, request.revision, &request.node_id);
        let completion = {
            let mut model = model.lock().expect("view model lock");
            model
                .finish_node_selection(request.token, result)
                .map(|outcome| (snapshot(&model), outcome.refresh_catalog))
        };
        let Some((current, refresh_catalog)) = completion else {
            return;
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_snapshot(&ui, &current);
            }
            if refresh_catalog {
                load_catalog(weak, model);
            }
        });
    });
}

fn apply_events(ui: &AppWindow, events: Vec<view_model::PresentedEvent>) {
    let events = events
        .into_iter()
        .map(|event| UiEvent {
            timestamp: event.timestamp.into(),
            component: event.component.into(),
            level: event.level.into(),
            message: event.message.into(),
        })
        .collect::<Vec<_>>();
    ui.set_events(ModelRc::new(VecModel::from(events)));
}

fn snapshot(model: &DesktopViewModel) -> UiSnapshot {
    UiSnapshot {
        presentation: model.presentation(),
        panel: model.panel(),
        import_pending: model.import_pending(),
        import_error: model.import_error().unwrap_or_default().to_owned(),
        subscription: model.subscription_presentation(),
        catalog: model.catalog_presentation(),
        diagnostics: model.diagnostics_presentation(),
        latency: model.latency_presentation(),
    }
}

fn apply_snapshot(ui: &AppWindow, snapshot: &UiSnapshot) {
    let presentation = &snapshot.presentation;
    ui.set_status_eyebrow(SharedString::from(presentation.eyebrow));
    ui.set_headline(presentation.headline.clone().into());
    ui.set_supporting(presentation.supporting.clone().into());
    ui.set_node_name(presentation.node.clone().into());
    ui.set_has_profile(presentation.has_profile);
    ui.set_state_busy(presentation.is_busy);
    ui.set_update_runtime_safe(runtime_allows_update(presentation));
    ui.set_primary_label(SharedString::from(presentation.primary_label));
    ui.set_primary_enabled(presentation.primary_enabled);
    ui.set_status_label(SharedString::from(presentation.pill_label));
    ui.set_status_tone(
        match presentation.semantic_tone {
            SemanticTone::Neutral => "neutral",
            SemanticTone::Accent => "accent",
            SemanticTone::Success => "success",
            SemanticTone::Danger => "danger",
            SemanticTone::Warning => "warning",
        }
        .into(),
    );
    ui.set_import_pending(snapshot.import_pending);
    ui.set_import_error(snapshot.import_error.clone().into());
    ui.set_subscription_title(snapshot.subscription.display_name.clone().into());
    ui.set_subscription_usage(snapshot.subscription.usage.clone().into());
    ui.set_subscription_expiry(snapshot.subscription.expiry.clone().into());
    ui.set_subscription_announcement_text(snapshot.subscription.announcement_text.clone().into());
    ui.set_subscription_announcement_tone(snapshot.subscription.announcement_tone.clone().into());
    let service_logo = snapshot
        .subscription
        .service_logo_path
        .as_deref()
        .and_then(|path| slint::Image::load_from_path(Path::new(path)).ok());
    ui.set_has_service_logo(service_logo.is_some());
    ui.set_service_logo(service_logo.unwrap_or_default());
    ui.set_subscription_refresh_available(snapshot.subscription.refresh_available);
    ui.set_subscription_refresh_enabled(
        presentation.has_profile && !presentation.is_connected && !presentation.is_busy,
    );
    ui.set_subscription_refresh_pending(snapshot.subscription.refresh_pending);
    ui.set_subscription_refresh_error(
        snapshot
            .subscription
            .error
            .clone()
            .unwrap_or_default()
            .into(),
    );
    ui.set_diagnostics_xray_value(snapshot.diagnostics.xray.value.clone().into());
    ui.set_diagnostics_xray_tone(snapshot.diagnostics.xray.tone.clone().into());
    ui.set_diagnostics_mihomo_value(snapshot.diagnostics.mihomo.value.clone().into());
    ui.set_diagnostics_mihomo_tone(snapshot.diagnostics.mihomo.tone.clone().into());
    ui.set_diagnostics_tun_value(snapshot.diagnostics.tun.value.clone().into());
    ui.set_diagnostics_tun_tone(snapshot.diagnostics.tun.tone.clone().into());
    ui.set_diagnostics_loading(snapshot.diagnostics.loading);
    ui.set_latency_check_loading(snapshot.latency.loading);
    ui.set_latency_check_queued(snapshot.latency.queued);
    ui.set_latency_check_error(snapshot.latency.error.clone().unwrap_or_default().into());
    ui.set_diagnostic_mappings(ModelRc::new(VecModel::from(
        snapshot
            .diagnostics
            .mappings
            .iter()
            .map(|mapping| UiProxyMapping {
                proxy_name: mapping.proxy_name.clone().into(),
                address: mapping.address.clone().into(),
                xray_label: mapping.xray_label.clone().into(),
            })
            .collect::<Vec<_>>(),
    )));
    apply_catalog(ui, &snapshot.catalog);
    ui.set_active_panel(
        match snapshot.panel {
            Panel::None => "none",
            Panel::Subscription => "subscription",
            Panel::Routes => "routes",
            Panel::Events => "events",
        }
        .into(),
    );
}

fn apply_catalog(ui: &AppWindow, catalog: &CatalogPresentation) {
    let groups = catalog
        .groups
        .iter()
        .map(|group| {
            let display = derive_catalog_display(&group.label, CatalogDisplayKind::Group);
            UiCatalogGroup {
                id: group.id.clone().into(),
                icon: display.icon.into(),
                label: display.label.into(),
                selected: group.selected,
            }
        })
        .collect::<Vec<_>>();
    let nodes = catalog
        .nodes
        .iter()
        .map(|node| {
            let display = derive_catalog_display(&node.label, CatalogDisplayKind::Node);
            UiCatalogNode {
                id: node.id.clone().into(),
                icon: display.icon.into(),
                label: display.label.into(),
                selected: node.selected,
                latency_text: node.latency_text.clone().into(),
                latency_tone: node.latency_tone.clone().into(),
            }
        })
        .collect::<Vec<_>>();
    ui.set_catalog_groups(ModelRc::new(VecModel::from(groups)));
    ui.set_catalog_nodes(ModelRc::new(VecModel::from(nodes)));
    ui.set_catalog_loading(catalog.loading);
    ui.set_catalog_selection_enabled(catalog.selection_enabled);
    ui.set_catalog_selection_pending(catalog.selection_pending);
    ui.set_catalog_selection_queued(catalog.selection_queued);
    ui.set_catalog_error(catalog.error.clone().unwrap_or_default().into());
}

#[cfg(test)]
mod window_tests {
    use super::{
        LaunchArguments, LaunchMode, PREFERENCE_RETRY_DELAYS, PREFERENCE_SAVE_ERROR_RU,
        PersistenceTracker, PreferenceWriter, WindowObservation, WriterCommand, WriterResult,
        activation_for_launch_arguments, drain_latest_write, initial_window_visible,
        next_maximized, parse_launch_arguments, preference_error_for_result,
        reduced_motion_from_client_area_animation, retry_delay_after_failure,
        runtime_allows_update, scale_factor_milli, visible_page_from_name, visible_page_name,
    };
    use crate::preferences::{AppPreferences, PreferenceStore, VisiblePage, WindowBounds};
    use crate::single_instance::Activation;
    use crate::view_model::UiState;
    use crate::windows_shell::{
        CloseDisposition, MinimizeDisposition, close_disposition, minimize_disposition,
        parse_resize_edge,
    };

    #[test]
    fn maximize_toggle_inverts_current_window_state() {
        assert!(next_maximized(false));
        assert!(!next_maximized(true));
    }

    #[test]
    fn windows_client_animation_preference_has_a_safe_testable_mapping() {
        assert!(!reduced_motion_from_client_area_animation(Some(true)));
        assert!(reduced_motion_from_client_area_animation(Some(false)));
        assert!(!reduced_motion_from_client_area_animation(None));

        let source = include_str!("main.rs");
        assert!(source.contains("SPI_GETCLIENTAREAANIMATION"));
        assert!(source.contains("client_area_animation_enabled()"));
        assert!(source.contains("ui.set_reduced_motion("));
    }

    #[test]
    fn ambient_preference_and_shell_visibility_are_wired_to_native_lifecycle() {
        let mut tracker = PersistenceTracker::new(AppPreferences::default());
        tracker.set_ambient_background(false);
        assert!(!tracker.latest_desired().ambient_background);

        let source = include_str!("main.rs");
        assert!(
            source.contains(
                "ui.set_ambient_background_enabled(loaded_preferences.ambient_background)"
            )
        );
        assert!(
            source.contains("tracker.set_ambient_background(ui.get_ambient_background_enabled())")
        );
        assert!(source.contains("ui.set_shell_active(starts_visible)"));
        assert!(source.matches("ui.set_shell_active(false)").count() >= 2);
        assert!(source.matches("ui.set_shell_active(true)").count() >= 2);
    }

    #[test]
    fn winit_scale_factor_is_bounded_before_entering_placement_model() {
        assert_eq!(scale_factor_milli(1.0), Some(1_000));
        assert_eq!(scale_factor_milli(2.0), Some(2_000));
        assert_eq!(scale_factor_milli(f64::NAN), None);
        assert_eq!(scale_factor_milli(f64::INFINITY), None);
        assert_eq!(scale_factor_milli(0.49), None);
        assert_eq!(scale_factor_milli(8.01), None);
    }

    #[test]
    fn close_hides_only_when_a_tray_is_available_outside_smoke_mode() {
        assert_eq!(close_disposition(true, false), CloseDisposition::HideToTray);
        assert_eq!(close_disposition(false, false), CloseDisposition::Exit);
        assert_eq!(close_disposition(true, true), CloseDisposition::Exit);
        assert_eq!(minimize_disposition(true), MinimizeDisposition::HideToTray);
        assert_eq!(
            minimize_disposition(false),
            MinimizeDisposition::MinimizeToTaskbar
        );
        let close_handler = include_str!("main.rs")
            .split("fn close_window(")
            .nth(1)
            .and_then(|source| source.split("fn tray_menu_model").next())
            .expect("close handler");
        assert!(!close_handler.contains("MinimizeToTaskbar"));
    }

    #[test]
    fn page_names_round_trip_and_unknown_names_are_rejected() {
        for page in [
            VisiblePage::Home,
            VisiblePage::Status,
            VisiblePage::Settings,
        ] {
            assert_eq!(visible_page_from_name(visible_page_name(&page)), Some(page));
        }
        assert_eq!(visible_page_from_name("routes"), None);
    }

    #[test]
    fn persistence_waits_for_two_stable_ticks_and_skips_unchanged_state() {
        let initial = AppPreferences::default();
        let mut tracker = PersistenceTracker::new(initial.clone());
        let changed = WindowObservation {
            restored_bounds: Some(WindowBounds {
                x: 10,
                y: 20,
                width: 900,
                height: 700,
            }),
            maximized: false,
            visible_page: VisiblePage::Settings,
        };

        assert_eq!(tracker.observe(Some(changed.clone())), None);
        let persisted = tracker
            .observe(Some(changed.clone()))
            .expect("stable change");
        assert_eq!(persisted.restored_bounds, changed.restored_bounds);
        assert_eq!(persisted.visible_page, VisiblePage::Settings);
        tracker.mark_persisted(persisted);
        assert_eq!(tracker.observe(Some(changed.clone())), None);
        assert_eq!(tracker.observe(None), None);
        assert_eq!(tracker.observe(Some(changed)), None);
    }

    #[test]
    fn persistence_queue_coalesces_latest_and_suppresses_failed_snapshot_repeats() {
        let mut first = AppPreferences {
            visible_page: VisiblePage::Home,
            ..AppPreferences::default()
        };
        let second = AppPreferences {
            visible_page: VisiblePage::Status,
            ..first.clone()
        };
        let third = AppPreferences {
            visible_page: VisiblePage::Settings,
            ..first.clone()
        };
        let (sender, receiver) = std::sync::mpsc::channel();
        sender.send(WriterCommand::Write(second.clone())).unwrap();
        sender.send(WriterCommand::Write(third.clone())).unwrap();
        assert!(!drain_latest_write(&receiver, &mut first));
        assert_eq!(first, third);

        let (sender, receiver) = std::sync::mpsc::channel();
        sender.send(WriterCommand::Write(second.clone())).unwrap();
        sender.send(WriterCommand::Stop).unwrap();
        assert!(drain_latest_write(&receiver, &mut first));
        assert_eq!(first, second);

        let mut tracker = PersistenceTracker::new(AppPreferences::default());
        let changed = WindowObservation {
            restored_bounds: None,
            maximized: false,
            visible_page: VisiblePage::Settings,
        };
        assert_eq!(tracker.observe(Some(changed.clone())), None);
        assert!(tracker.observe(Some(changed.clone())).is_some());
        for _ in 0..4 {
            assert_eq!(tracker.observe(Some(changed.clone())), None);
        }
        assert_eq!(PREFERENCE_RETRY_DELAYS.len() + 1, 3);
        assert_eq!(
            retry_delay_after_failure(1),
            Some(std::time::Duration::from_secs(1))
        );
        assert_eq!(
            retry_delay_after_failure(2),
            Some(std::time::Duration::from_secs(2))
        );
        assert_eq!(retry_delay_after_failure(3), None);
        assert_eq!(retry_delay_after_failure(4), None);
    }

    #[test]
    fn queued_restored_bounds_survive_maximize_before_ack_and_exit_flush() {
        let bounds_a = WindowBounds {
            x: 10,
            y: 20,
            width: 800,
            height: 650,
        };
        let bounds_b = WindowBounds {
            x: 300,
            y: 240,
            width: 980,
            height: 760,
        };
        let mut tracker = PersistenceTracker::new(AppPreferences {
            restored_bounds: Some(bounds_a),
            ..AppPreferences::default()
        });
        let restored_b = WindowObservation {
            restored_bounds: Some(bounds_b.clone()),
            maximized: false,
            visible_page: VisiblePage::Home,
        };
        assert_eq!(tracker.observe(Some(restored_b.clone())), None);
        let queued_b = tracker.observe(Some(restored_b)).expect("B queued");
        assert_eq!(queued_b.restored_bounds, Some(bounds_b.clone()));

        let maximized = WindowObservation {
            restored_bounds: None,
            maximized: true,
            visible_page: VisiblePage::Settings,
        };
        assert_eq!(tracker.observe(Some(maximized.clone())), None);
        let exit_snapshot = tracker.snapshot_for_flush(Some(&maximized));
        assert_eq!(exit_snapshot.restored_bounds, Some(bounds_b));
        assert!(exit_snapshot.maximized);
        assert_eq!(exit_snapshot.visible_page, VisiblePage::Settings);
    }

    #[test]
    fn older_saved_ack_does_not_roll_latest_desired_snapshot_back() {
        let bounds_a = WindowBounds {
            x: 10,
            y: 20,
            width: 800,
            height: 650,
        };
        let bounds_b = WindowBounds {
            x: 300,
            y: 240,
            width: 980,
            height: 760,
        };
        let initial = AppPreferences {
            restored_bounds: Some(bounds_a),
            ..AppPreferences::default()
        };
        let mut tracker = PersistenceTracker::new(initial);
        let restored_b = WindowObservation {
            restored_bounds: Some(bounds_b.clone()),
            maximized: false,
            visible_page: VisiblePage::Home,
        };
        tracker.observe(Some(restored_b.clone()));
        let queued_b = tracker.observe(Some(restored_b)).unwrap();
        let maximized = WindowObservation {
            restored_bounds: None,
            maximized: true,
            visible_page: VisiblePage::Status,
        };
        tracker.observe(Some(maximized));
        tracker.mark_persisted(queued_b);

        assert_eq!(tracker.latest_desired().restored_bounds, Some(bounds_b));
        assert!(tracker.latest_desired().maximized);
        assert_eq!(tracker.latest_desired().visible_page, VisiblePage::Status);
    }

    #[test]
    fn failed_in_flight_snapshot_remains_the_exit_restore_source() {
        let bounds_b = WindowBounds {
            x: -400,
            y: 80,
            width: 920,
            height: 700,
        };
        let mut tracker = PersistenceTracker::new(AppPreferences::default());
        let restored_b = WindowObservation {
            restored_bounds: Some(bounds_b.clone()),
            maximized: false,
            visible_page: VisiblePage::Home,
        };
        tracker.observe(Some(restored_b.clone()));
        assert!(tracker.observe(Some(restored_b)).is_some());
        for _failed_attempt in 0..3 {
            assert_eq!(
                tracker.latest_desired().restored_bounds,
                Some(bounds_b.clone())
            );
        }

        let exit_snapshot = tracker.snapshot_for_flush(Some(&WindowObservation {
            restored_bounds: None,
            maximized: true,
            visible_page: VisiblePage::Settings,
        }));
        assert_eq!(exit_snapshot.restored_bounds, Some(bounds_b));
        assert!(exit_snapshot.maximized);
    }

    #[test]
    fn writer_results_expose_only_bounded_safe_russian_error_and_success_clears_it() {
        assert_eq!(
            preference_error_for_result(&WriterResult::Failed),
            PREFERENCE_SAVE_ERROR_RU
        );
        assert_eq!(
            preference_error_for_result(&WriterResult::Saved(AppPreferences::default())),
            ""
        );
        assert!(!PREFERENCE_SAVE_ERROR_RU.is_ascii());
        assert!(PREFERENCE_SAVE_ERROR_RU.chars().count() < 64);
        assert!(!PREFERENCE_SAVE_ERROR_RU.contains("http"));
        assert!(!PREFERENCE_SAVE_ERROR_RU.contains(":\\"));
    }

    #[test]
    fn writer_shutdown_drains_pending_snapshots_in_order() {
        let root = tempfile::tempdir().unwrap();
        let store = PreferenceStore::at(root.path().join("preferences.json"));
        let mut writer = PreferenceWriter::start(Some(store.clone()));
        writer.enqueue(AppPreferences {
            visible_page: VisiblePage::Status,
            ..AppPreferences::default()
        });
        writer.enqueue(AppPreferences {
            visible_page: VisiblePage::Settings,
            ..AppPreferences::default()
        });
        writer.stop();
        assert_eq!(store.load().unwrap().visible_page, VisiblePage::Settings);
    }

    #[test]
    #[ignore = "manual event-loop enqueue benchmark"]
    fn preference_enqueue_latency_baseline() {
        let mut writer = PreferenceWriter::start(None);
        let preferences = AppPreferences::default();
        let started = std::time::Instant::now();
        for _ in 0..1_000 {
            writer.enqueue(preferences.clone());
        }
        let elapsed = started.elapsed();
        println!(
            "preference enqueue: {:?} total, {:?} average",
            elapsed,
            elapsed / 1_000
        );
        writer.stop();
    }

    #[test]
    fn maximized_observation_preserves_last_restored_bounds() {
        let initial = AppPreferences {
            restored_bounds: Some(WindowBounds {
                x: 1,
                y: 2,
                width: 800,
                height: 650,
            }),
            ..AppPreferences::default()
        };
        let mut tracker = PersistenceTracker::new(initial.clone());
        let maximized = WindowObservation {
            restored_bounds: None,
            maximized: true,
            visible_page: VisiblePage::Status,
        };
        assert_eq!(tracker.observe(Some(maximized.clone())), None);
        let saved = tracker
            .observe(Some(maximized))
            .expect("stable maximized change");
        assert_eq!(saved.restored_bounds, initial.restored_bounds);
        assert!(saved.maximized);
        assert_eq!(saved.visible_page, VisiblePage::Status);
    }

    #[test]
    fn native_resize_and_persistence_source_contract() {
        let rust = include_str!("main.rs");
        let slint = include_str!("../ui/app.slint");
        assert!(rust.contains("ui.on_window_resize"));
        assert!(rust.contains("parse_resize_edge"));
        assert!(rust.contains("drag_resize_window"));
        assert!(rust.contains("PreferenceStore::from_local_app_data()"));
        assert!(rust.contains("TimerMode::Repeated, Duration::from_millis(500)"));
        assert!(rust.contains("stop_and_flush"));
        let exit_branch = rust
            .split("TrayCommand::Exit => {")
            .nth(1)
            .and_then(|source| source.split("}").next())
            .expect("tray Exit branch");
        assert!(
            exit_branch.find("flush_preferences").unwrap()
                < exit_branch.find("quit_event_loop").unwrap()
        );
        let timer = rust
            .split("fn wire_preference_persistence(")
            .nth(1)
            .and_then(|source| source.split("fn wire_updater").next())
            .expect("preference timer");
        assert!(timer.contains(".enqueue(preferences)"));
        assert!(!timer.contains(".save("));
        assert!(timer.contains("monitor_cap_changed"));
        let maximize = rust
            .split("ui.on_window_toggle_maximize")
            .nth(1)
            .and_then(|source| source.split("ui.on_window_close").next())
            .expect("maximize callback");
        assert!(
            maximize.find("set_current_monitor_max_size").unwrap()
                < maximize.find("set_maximized").unwrap()
        );
        assert!(!slint.contains("max-width: 1120px"));
        assert!(!slint.contains("max-height: 1000px"));
        assert!(slint.contains("callback window-resize(string)"));
        let visual_frame = slint
            .split("visual-frame := FocusScope {")
            .nth(1)
            .and_then(|source| source.split("key-pressed(event)").next())
            .expect("inset visual frame");
        assert!(slint.contains("private property <length> resize-border: 6px;"));
        assert!(visual_frame.contains("x: root.resize-border;"));
        assert!(visual_frame.contains("y: root.resize-border;"));
        assert!(visual_frame.contains("width: parent.width - 2 * root.resize-border;"));
        assert!(visual_frame.contains("height: parent.height - 2 * root.resize-border;"));
        assert!(slint.contains("in property <string> preference-error;"));
        assert!(slint.contains("text: root.preference-error"));
        for edge in ["n", "ne", "e", "se", "s", "sw", "w", "nw"] {
            assert!(parse_resize_edge(edge).is_some(), "missing {edge}");
            assert!(
                slint.contains(&format!("root.window-resize(\"{edge}\")")),
                "missing Slint hit region for {edge}"
            );
        }
    }

    #[test]
    fn update_install_gate_accepts_only_idle_runtime_states() {
        assert!(runtime_allows_update(&UiState::Empty.presentation()));
        assert!(runtime_allows_update(
            &UiState::Ready {
                profile: "profile".into(),
                node: "auto".into(),
            }
            .presentation()
        ));
        assert!(!runtime_allows_update(
            &UiState::Connected {
                profile: "profile".into(),
                node: "node".into(),
            }
            .presentation()
        ));
        assert!(!runtime_allows_update(
            &UiState::Error {
                message: "x".into()
            }
            .presentation()
        ));
    }

    #[test]
    fn background_launch_and_restore_source_contract() {
        let source = include_str!("main.rs");
        assert!(source.contains("--background"));
        assert!(source.contains("LaunchMode::Background"));
        assert!(source.contains("slint::run_event_loop()"));
        assert!(source.contains("initial_window_visible(launch_mode, tray_runtime.is_some())"));

        let show_branch = source
            .split("TrayCommand::ShowWindow => {")
            .nth(1)
            .and_then(|source| source.split("TrayCommand::ToggleConnection").next())
            .expect("tray ShowWindow branch");
        let unminimize = show_branch
            .find("set_minimized(false)")
            .expect("restore must unminimize");
        let show = show_branch.find("ui.show()").expect("restore must show");
        assert!(unminimize < show, "unminimize must happen before show");
        assert!(show_branch.contains("window.set_visible(true)"));
        assert!(show_branch.contains("window.focus_window()"));
    }

    #[test]
    fn background_mode_stays_hidden_only_when_tray_is_available() {
        assert_eq!(
            parse_launch_arguments(["--background".into()])
                .unwrap()
                .mode,
            LaunchMode::Background
        );
        assert_eq!(
            parse_launch_arguments(Vec::new()).unwrap().mode,
            LaunchMode::Foreground
        );
        assert!(!initial_window_visible(LaunchMode::Background, true));
        assert!(initial_window_visible(LaunchMode::Background, false));
        assert!(initial_window_visible(LaunchMode::Foreground, true));
    }

    #[test]
    fn preview_marker_is_a_foreground_only_capture_argument() {
        assert_eq!(
            parse_launch_arguments(["--quiet-signal-preview".into()]),
            Ok(LaunchArguments::foreground())
        );
    }

    #[test]
    fn installer_uri_decodes_one_strict_http_subscription_url() {
        let parsed = parse_launch_arguments([
            "multicore://install-sub?url=https%3A%2F%2Fexample.com%2Fsub%3Ftoken%3Da%252Fb".into(),
        ])
        .unwrap();

        assert_eq!(
            parsed,
            LaunchArguments {
                mode: LaunchMode::Foreground,
                subscription_url: Some("https://example.com/sub?token=a%2Fb".into()),
            }
        );
    }

    #[test]
    fn installer_uri_rejects_malformed_duplicate_and_non_http_inputs() {
        for argument in [
            "multicore://install-sub?url=https%3A%2F%2Fexample.com%2F%ZZ",
            "multicore://install-sub?url=https%3A%2F%2Fexample.com%2F%2",
            "multicore://install-sub?url=https%3A%2F%2Fa.example&url=https%3A%2F%2Fb.example",
            "multicore://install-sub?url=ftp%3A%2F%2Fexample.com%2Fsub",
            "multicore://install-sub?url=https://example.com/sub",
            "multicore://install-sub?other=https%3A%2F%2Fexample.com",
        ] {
            assert!(
                parse_launch_arguments([argument.into()]).is_err(),
                "{argument}"
            );
        }
    }

    #[test]
    fn background_and_installer_uri_are_mutually_exclusive() {
        let uri = std::ffi::OsString::from(
            "multicore://install-sub?url=https%3A%2F%2Fexample.com%2Fsubscription",
        );
        assert!(parse_launch_arguments(["--background".into(), uri.clone()]).is_err());
        assert!(parse_launch_arguments([uri, "--background".into()]).is_err());
    }

    #[test]
    fn secondary_launch_preserves_foreground_background_and_deep_link_intent() {
        assert_eq!(
            activation_for_launch_arguments(&LaunchArguments::foreground()),
            Activation::Show
        );
        assert_eq!(
            activation_for_launch_arguments(&LaunchArguments {
                mode: LaunchMode::Background,
                subscription_url: None,
            }),
            Activation::Background
        );
        assert_eq!(
            activation_for_launch_arguments(&LaunchArguments {
                mode: LaunchMode::Foreground,
                subscription_url: Some("https://example.com/sub".into()),
            }),
            Activation::InstallSubscription("https://example.com/sub".into())
        );
    }

    #[test]
    fn single_instance_gate_precedes_daemon_bootstrap() {
        let source = include_str!("main.rs");
        let claim = source
            .find("single_instance::claim_or_forward")
            .expect("single-instance gate");
        let bootstrap = source.find("bootstrap::start()").expect("daemon bootstrap");
        assert!(claim < bootstrap);
    }

    #[test]
    fn installer_launch_only_prefills_and_reveals_the_editor() {
        let source = include_str!("main.rs");
        let setup = source
            .split("if let Some(subscription_url) = launch_arguments.subscription_url {")
            .nth(1)
            .and_then(|source| source.split("apply_update_state(").next())
            .expect("installer launch UI setup");
        assert!(setup.contains("ui.set_local_page(\"settings\".into())"));
        assert!(setup.contains("ui.set_subscription_draft(subscription_url.into())"));
        assert!(setup.contains("ui.set_replace_editor_open(true)"));
        assert!(!setup.contains("invoke_import_url"));
        assert!(!setup.contains("run_import"));
    }
}
