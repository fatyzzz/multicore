#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app_icon;
mod appimage_handoff;
mod bootstrap;
mod clipboard_text;
mod conflicting_apps;
mod crash_log;
mod crisp_image;
mod daemon;
mod data_reset;
mod data_root;
mod emoji_images;
mod event_loop_guard;
mod language;
mod layout_shortcuts;
#[cfg(target_os = "linux")]
mod linux_display;
#[cfg(all(test, target_os = "linux"))]
mod linux_shell_tests;
mod log_bundle;
mod logo_cache;
#[cfg(windows)]
mod native_crash;
mod preferences;
#[cfg(test)]
mod preview_render;
mod renderer_fallback;
#[cfg(windows)]
mod renderer_release;
#[cfg(test)]
mod rust_catalog_tests;
mod save_dialog;
mod service_install;
mod service_logo;
mod session_logs;
mod single_instance;
#[cfg(test)]
mod slint_catalog_tests;
mod tray;
mod troubleshoot;
mod updater;
mod view_model;
mod windows_settings;
mod windows_shell;

use multicore_i18n::tr;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::io;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use daemon::{
    DaemonClient, DaemonError, DaemonUpdateProgress, DaemonUpdateStage, UnavailableDaemonClient,
};
use multicore_release::GitHubRelease;
use preferences::{
    AppPreferences, PreferenceStore, RailGrowthPreference, TrayPopupSizePreference, VisiblePage,
    WindowBounds,
};
use slint::winit_030::WinitWindowAccessor;
use slint::{
    CloseRequestResponse, ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel,
};
use tray::{
    TrayCommand, TrayGroup, TrayInput, TrayInteraction, TrayMenuModel, TrayNode,
    TrayPopupModelCache, build_menu_model,
};
use updater::{UpdateAction, UpdateStage, UpdateState};
use view_model::{
    CatalogDisplayKind, CatalogPresentation, ConflictingAppsPresentation, DesktopViewModel,
    DiagnosticsPresentation, LatencyPresentation, LatencyRequest, MutationToken,
    NodeLatencyRequest, NodeSelectionRequest, Panel, PrimaryAction, ProfileRequestToken,
    QueuedNodeSelection, SemanticTone, SessionLine, SessionTraffic, StatePresentation,
    SubscriptionPresentation, SubscriptionProfileOperation, SubscriptionProfilesPresentation,
    derive_catalog_display, exit_now_display, local_proxy_address, search_routes,
    split_leading_symbol,
};
use windows_settings::LaunchAtSignInState;
use windows_shell::{
    COLLAPSED_RAIL_WIDTH, CloseDisposition, EXPANDED_RAIL_WIDTH, MinimizeDisposition, MonitorRect,
    MonitorSignature, PhysicalPosition, PhysicalRect, RAIL_RESIZE_MS, RailGrowth, TRAY_POPUP_MAX,
    TRAY_POPUP_MIN, TrayPopupGrip, TrayResizeHandle, apply_tray_popup_native_rounding,
    close_disposition, global_cursor_position, minimize_disposition, monitor_cap_changed,
    next_maximized, parse_resize_edge, place_tray_popup, rail_grow_delta, rail_resize_frame,
    rail_shrink_width, resize_tray_popup, restore_placement, set_tray_popup_rect, tray_popup_grip,
    tray_popup_monitor, tray_popup_size,
};

slint::include_modules!();

#[derive(Clone)]
struct UiSnapshot {
    presentation: StatePresentation,
    panel: Panel,
    import_pending: bool,
    import_error: String,
    import_notice: String,
    subscription: SubscriptionPresentation,
    profiles: SubscriptionProfilesPresentation,
    catalog: CatalogPresentation,
    diagnostics: DiagnosticsPresentation,
    latency: LatencyPresentation,
    conflicting_apps: ConflictingAppsPresentation,
    cores_ready: bool,
    service_mode: bool,
}

const MAX_PENDING_SUBSCRIPTION_IMPORTS: usize = 16;

#[derive(Default)]
struct PendingSubscriptionConsents {
    proposals: VecDeque<SubscriptionProposal>,
}

struct SubscriptionProposal {
    url: String,
    host: String,
}

impl PendingSubscriptionConsents {
    fn enqueue(&mut self, url: String) -> bool {
        if self.proposals.len() >= MAX_PENDING_SUBSCRIPTION_IMPORTS
            || url.len() > view_model::MAX_SUBSCRIPTION_URL_BYTES
            || url.bytes().any(|byte| byte.is_ascii_control())
        {
            return false;
        }
        let Ok(parsed) = reqwest::Url::parse(&url) else {
            return false;
        };
        if !matches!(parsed.scheme(), "http" | "https") || parsed.fragment().is_some() {
            return false;
        }
        let Some(host) = parsed.host_str() else {
            return false;
        };
        self.proposals.push_back(SubscriptionProposal {
            url,
            host: host.to_owned(),
        });
        true
    }

    fn current_host(&self) -> Option<&str> {
        self.proposals
            .front()
            .map(|proposal| proposal.host.as_str())
    }

    fn approve_next(&mut self, imports: &mut PendingSubscriptionImports) -> bool {
        let Some(proposal) = self.proposals.front() else {
            return false;
        };
        if !imports.enqueue(proposal.url.clone()) {
            return false;
        }
        self.proposals.pop_front();
        true
    }

    fn reject_next(&mut self) -> bool {
        self.proposals.pop_front().is_some()
    }

    fn len(&self) -> usize {
        self.proposals.len()
    }
}

#[derive(Default)]
struct PendingSubscriptionImports {
    urls: VecDeque<String>,
    in_flight: bool,
    attempts: u8,
    retry_at: Option<Instant>,
}

#[derive(Debug, PartialEq, Eq)]
enum PendingImportOutcome {
    NotQueued,
    RetryScheduled,
    Succeeded,
    Failed,
}

impl PendingSubscriptionImports {
    fn enqueue(&mut self, url: String) -> bool {
        if self.urls.len() >= MAX_PENDING_SUBSCRIPTION_IMPORTS {
            return false;
        }
        self.urls.push_back(url);
        true
    }

    fn dispatch_next(&mut self, now: Instant, try_import: impl FnOnce(&str) -> bool) -> bool {
        if self.in_flight || self.retry_at.is_some_and(|retry_at| now < retry_at) {
            return false;
        }
        let Some(url) = self.urls.front() else {
            return false;
        };
        if !try_import(url) {
            return false;
        }
        self.in_flight = true;
        self.attempts += 1;
        true
    }

    fn finish_attempt(&mut self, url: &str, succeeded: bool, now: Instant) -> PendingImportOutcome {
        if self.urls.front().is_none_or(|queued| queued != url) {
            return PendingImportOutcome::NotQueued;
        }
        if succeeded || self.attempts >= 3 {
            self.urls.pop_front();
            self.in_flight = false;
            self.attempts = 0;
            self.retry_at = None;
            if succeeded {
                PendingImportOutcome::Succeeded
            } else {
                PendingImportOutcome::Failed
            }
        } else if self.in_flight {
            self.in_flight = false;
            self.retry_at = Some(now + Duration::from_secs(if self.attempts == 1 { 2 } else { 5 }));
            PendingImportOutcome::RetryScheduled
        } else {
            PendingImportOutcome::NotQueued
        }
    }

    fn is_empty(&self) -> bool {
        self.urls.is_empty()
    }

    fn current_host(&self) -> Option<String> {
        let url = self.urls.front()?;
        reqwest::Url::parse(url).ok()?.host_str().map(str::to_owned)
    }

    fn len(&self) -> usize {
        self.urls.len()
    }
}

struct ProfilePreferenceReconciler {
    hints: AppPreferences,
    current_profile: Option<String>,
    pending_ready_profile: Option<String>,
    tracker: Arc<Mutex<PersistenceTracker>>,
}

impl ProfilePreferenceReconciler {
    fn new(loaded: AppPreferences, tracker: Arc<Mutex<PersistenceTracker>>) -> Self {
        Self {
            hints: loaded,
            current_profile: None,
            pending_ready_profile: None,
            tracker,
        }
    }

    fn capture_current(&mut self, model: &DesktopViewModel) {
        let Some((profile, selected_group, selections)) = model.route_preferences() else {
            return;
        };
        self.record_hint(profile, selected_group, selections);
    }

    fn record_hint(
        &mut self,
        profile: String,
        selected_group: Option<String>,
        selections: BTreeMap<String, String>,
    ) {
        if let Some(group) = &selected_group {
            self.hints
                .last_group_by_profile
                .insert(profile.clone(), group.clone());
        } else {
            self.hints.last_group_by_profile.remove(&profile);
        }
        self.hints
            .selections_by_profile
            .insert(profile.clone(), selections.clone());
        self.tracker
            .lock()
            .expect("preference tracker lock")
            .set_route_preferences(profile, selected_group, selections);
    }

    fn reconcile_profile_list(&mut self, model: &DesktopViewModel) {
        self.set_authoritative_profile(model.profile_presentation().active_id);
    }

    fn set_authoritative_profile(&mut self, active_id: Option<String>) {
        self.tracker
            .lock()
            .expect("preference tracker lock")
            .set_active_profile_hint(active_id.clone());
        if self.current_profile == active_id {
            return;
        }
        self.current_profile.clone_from(&active_id);
        self.pending_ready_profile = active_id;
    }

    fn apply_ready_hints_after_catalog(
        &mut self,
        model: &mut DesktopViewModel,
        catalog_succeeded: bool,
    ) {
        if !catalog_succeeded {
            return;
        }
        let presentation = model.presentation();
        if presentation.is_connected {
            self.pending_ready_profile = None;
            return;
        }
        if !route_hints_apply_to(&presentation) {
            return;
        }
        let Some(profile) = self.pending_ready_profile.take() else {
            return;
        };
        let selected_group = self.hints.last_group_by_profile.get(&profile).cloned();
        let selections = self
            .hints
            .selections_by_profile
            .get(&profile)
            .cloned()
            .unwrap_or_default();
        let catalog_group = model.catalog_presentation().selected_group_id;
        let selected_group_id = selected_group
            .as_deref()
            .and_then(|key| model.resolve_group_preference_id(key));
        model.restore_route_preferences(selected_group_id.as_deref(), &BTreeMap::new());
        for (group_key, node_key) in &selections {
            if let Some((group_id, node_id)) =
                model.resolve_route_preference_ids(group_key, node_key)
                && model.select_catalog_group(&group_id)
            {
                let _ = model.begin_node_selection(&node_id);
            }
        }
        let restored_group = selected_group_id
            .as_deref()
            .is_some_and(|group_id| model.select_catalog_group(group_id));
        if !restored_group && let Some(group_id) = catalog_group {
            let _ = model.select_catalog_group(&group_id);
        }
    }
}

fn route_hints_apply_to(presentation: &StatePresentation) -> bool {
    presentation.primary_action == PrimaryAction::Connect
        && presentation.primary_enabled
        && !presentation.is_connected
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

    fn set_auto_connect_on_startup(&mut self, enabled: bool) {
        self.desired.auto_connect_on_startup = enabled;
    }

    fn set_navigation_collapsed(&mut self, collapsed: bool) {
        self.desired.navigation_collapsed = collapsed;
    }

    fn set_tun_enabled(&mut self, enabled: bool) {
        self.desired.tun_enabled = enabled;
    }

    fn set_rail_growth(&mut self, delta: i32, grown_width: i32) {
        self.desired.rail_growth = (delta > 0 && grown_width > 0).then(|| RailGrowthPreference {
            delta: delta.unsigned_abs(),
            grown_width: grown_width.unsigned_abs(),
        });
    }

    fn set_tray_popup_size(&mut self, size: (u32, u32)) {
        self.desired.tray_popup_size = Some(TrayPopupSizePreference {
            width: size.0,
            height: size.1,
        });
    }

    fn set_announcement_seen(&mut self, fingerprint: String) {
        self.desired.announcement_seen = (!fingerprint.is_empty()).then_some(fingerprint);
    }

    fn set_appimage_handoff_seen(&mut self, appimage: String) {
        self.desired.appimage_handoff_seen = (!appimage.is_empty()).then_some(appimage);
    }

    fn set_service_offer_dismissed(&mut self, dismissed: bool) {
        self.desired.service_offer_dismissed = dismissed;
    }

    fn set_language(&mut self, language: language::Lang, set_by_user: bool) {
        self.desired.language = language;
        self.desired.language_set_by_user = set_by_user;
    }

    fn set_route_preferences(
        &mut self,
        profile: String,
        selected_group: Option<String>,
        selections: BTreeMap<String, String>,
    ) {
        if let Some(group) = selected_group {
            self.desired
                .last_group_by_profile
                .insert(profile.clone(), group);
        } else {
            self.desired.last_group_by_profile.remove(&profile);
        }
        self.desired
            .selections_by_profile
            .insert(profile, selections);
    }

    fn set_active_profile_hint(&mut self, profile: Option<String>) {
        self.desired.active_profile_hint = profile;
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
        VisiblePage::Subscriptions => "subscriptions",
        VisiblePage::Settings => "settings",
    }
}

fn visible_page_from_name(page: &str) -> Option<VisiblePage> {
    match page {
        "home" => Some(VisiblePage::Home),
        "status" => Some(VisiblePage::Status),
        "subscriptions" => Some(VisiblePage::Subscriptions),
        "settings" => Some(VisiblePage::Settings),
        _ => None,
    }
}

fn reduced_motion_from_client_area_animation(enabled: Option<bool>) -> bool {
    matches!(enabled, Some(false))
}

#[cfg(test)]
fn ambient_motion_allowed(
    user_enabled: bool,
    shell_active: bool,
    reduced_motion: bool,
    minimized: bool,
    home_visible: bool,
) -> bool {
    user_enabled && shell_active && !reduced_motion && !minimized && home_visible
}

fn shell_active_after_native_minimize_change(minimized: bool) -> bool {
    !minimized
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

/// Shown in Settings when the preferences file cannot be written.
fn preference_save_error() -> &'static str {
    tr!("Не удалось сохранить настройки.")
}
const PREFERENCE_RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(2)];

fn retry_delay_after_failure(attempt: usize) -> Option<Duration> {
    attempt
        .checked_sub(1)
        .and_then(|index| PREFERENCE_RETRY_DELAYS.get(index).copied())
}

fn preference_failure_is_terminal(attempt: usize, stop_after_write: bool) -> bool {
    stop_after_write || retry_delay_after_failure(attempt).is_none()
}

enum WriterCommand {
    Write(AppPreferences),
    Stop,
}

enum WriterResult {
    Saved(AppPreferences),
    Failed(PreferenceWriteFailure),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PreferenceWriteFailure {
    kind: io::ErrorKind,
    raw_os_error: Option<i32>,
    attempts: usize,
    /// The failed step and the OS message (no paths), for the low-level log.
    detail: String,
}

impl PreferenceWriteFailure {
    fn from_error(error: &io::Error, attempts: usize) -> Self {
        Self {
            kind: error.kind(),
            raw_os_error: preferences::raw_os_error(error),
            attempts,
            detail: error.to_string(),
        }
    }

    fn log_line(&self) -> String {
        format!(
            "failed after {} attempt(s): {} (kind={:?}, os={:?})",
            self.attempts, self.detail, self.kind, self.raw_os_error
        )
    }
}

/// The exit flush gets no background retries, so it rides out a short sharing violation (a
/// core host reading the staged generation holds the data directory for a moment) itself.
const PREFERENCE_FLUSH_RETRY_DELAYS: [Duration; 2] =
    [Duration::from_millis(150), Duration::from_millis(350)];

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

    fn stop_and_flush(
        &mut self,
        preferences: &AppPreferences,
    ) -> Result<(), PreferenceWriteFailure> {
        self.stop();
        let Some(store) = self.store.as_ref() else {
            let error = io::Error::new(io::ErrorKind::NotFound, "preference store unavailable");
            return Err(PreferenceWriteFailure::from_error(&error, 1));
        };
        let mut attempt = 0;
        loop {
            attempt += 1;
            let Err(error) = store.save(preferences) else {
                return Ok(());
            };
            match PREFERENCE_FLUSH_RETRY_DELAYS.get(attempt - 1) {
                Some(delay) => thread::sleep(*delay),
                None => return Err(PreferenceWriteFailure::from_error(&error, attempt)),
            }
        }
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
            let save_result = store.as_ref().map_or_else(
                || {
                    Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "preference store unavailable",
                    ))
                },
                |store| store.save(&preferences),
            );
            if save_result.is_ok() {
                let _ = results.send(WriterResult::Saved(preferences));
                if stop_after_write {
                    return;
                }
                break;
            }
            if preference_failure_is_terminal(attempt, stop_after_write) {
                let error = save_result.expect_err("failed preference write has an error");
                let _ = results.send(WriterResult::Failed(PreferenceWriteFailure::from_error(
                    &error, attempt,
                )));
                if stop_after_write {
                    return;
                }
                break;
            }
            let Some(retry_delay) = retry_delay_after_failure(attempt) else {
                unreachable!("a non-terminal preference failure always has a retry delay");
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

#[cfg(any(target_os = "linux", windows))]
thread_local! {
    static CREATING_TRAY_POPUP: Cell<bool> = const { Cell::new(false) };
}

/// Windows: the tray popup gets its rounded corners when its window is created, so a popup
/// created on demand (see `TrayPopupSlot`) never shows a square-cornered first frame.
#[cfg(windows)]
pub(crate) fn tray_popup_window_attributes(
    attributes: slint::winit_030::winit::window::WindowAttributes,
) -> slint::winit_030::winit::window::WindowAttributes {
    use slint::winit_030::winit::platform::windows::{
        CornerPreference, WindowAttributesExtWindows,
    };
    if CREATING_TRAY_POPUP.with(Cell::get) {
        attributes.with_corner_preference(CornerPreference::Round)
    } else {
        attributes
    }
}

/// Winit with Skia on OpenGL (Slint's default renderer for this build; Skia
/// itself drops to its CPU raster surface if no GL context can be created), or
/// Slint's software renderer after a GPU failure.
#[cfg(target_os = "linux")]
fn select_linux_backend(software_renderer: bool) -> Result<(), slint::PlatformError> {
    let selector = slint::BackendSelector::new().backend_name("winit".into());
    let selector = if software_renderer {
        selector.renderer_name("software".into())
    } else {
        selector
    };
    selector
        .with_winit_window_attributes_hook(|attributes| {
            if CREATING_TRAY_POPUP.with(Cell::get) {
                use slint::winit_030::winit::platform::x11::{WindowAttributesExtX11, WindowType};
                attributes.with_x11_window_type(vec![WindowType::PopupMenu])
            } else {
                attributes
            }
        })
        .select()
}

fn create_tray_popup() -> Result<TrayPopup, slint::PlatformError> {
    #[cfg(any(target_os = "linux", windows))]
    CREATING_TRAY_POPUP.with(|creating| creating.set(true));
    let popup = TrayPopup::new().inspect(|popup| {
        let _ = popup.window();
    });
    #[cfg(any(target_os = "linux", windows))]
    CREATING_TRAY_POPUP.with(|creating| creating.set(false));
    popup
}

fn main() -> Result<(), slint::PlatformError> {
    // First of all: a panic anywhere below is written to logs/crash.log instead of only
    // closing the app.
    crash_log::install();
    // Before the single-instance guard: the resetting process waits for the old instance to
    // exit, deletes the data directory and starts a clean app itself.
    if data_reset::run_pending_reset(&std::env::args_os().skip(1).collect::<Vec<_>>()) {
        return Ok(());
    }
    // An AppImage next to an installed deb/rpm/pacman package becomes the installed app
    // (returns only when there is none, or the hand-off failed).
    #[cfg(target_os = "linux")]
    appimage_handoff::hand_off_to_installed_package();
    let software_renderer = renderer_fallback::software_renderer_requested();
    let result = match run_desktop(software_renderer) {
        // Everything run_desktop held (single-instance guard, owned daemon,
        // tray) is released by now, so the restarted instance starts clean.
        Err(error) if !software_renderer && renderer_fallback::is_gpu_renderer_failure(&error) => {
            eprintln!("GPU renderer unavailable ({error}); restarting with the software renderer");
            renderer_fallback::remember_and_relaunch(std::env::args_os().skip(1).collect())
                .map_err(|_| error)
        }
        result => result,
    };
    // The daemon and every file lock are gone now, so the service installer
    // can remove this per-user copy.
    service_install::launch_pending_installer();
    result
}

fn run_desktop(software_renderer: bool) -> Result<(), slint::PlatformError> {
    log_bundle::note_app_start();
    multicore_core::ensure_crypto_provider_installed();
    #[cfg(target_os = "linux")]
    linux_display::prefer_desktop_scale();
    let launch_arguments =
        parse_launch_arguments(std::env::args_os().skip(1)).unwrap_or_else(|error| {
            eprintln!("launch arguments rejected: {error}");
            LaunchArguments::foreground()
        });
    #[cfg(windows)]
    if let Err(error) = windows_settings::ensure_url_protocol_registration() {
        eprintln!("URL protocol registration could not be repaired: {error}");
    }
    let activation = activation_for_launch_arguments(&launch_arguments);
    let (_instance_guard, activations) = match single_instance::claim_or_forward(&activation) {
        Ok(single_instance::Claim::Primary {
            _guard,
            activations,
        }) => (_guard, activations),
        Ok(single_instance::Claim::Forwarded) => {
            // The running instance shows the hand-off notice (a frame of its own after the
            // launch's activation, which an older instance simply ignores).
            #[cfg(target_os = "linux")]
            if let Some(appimage) = appimage_handoff::handed_off_from() {
                let notice =
                    single_instance::Activation::HandedOff(appimage.to_string_lossy().into_owned());
                if let Err(error) = single_instance::forward_to_primary(&notice) {
                    eprintln!("hand-off notice could not be forwarded: {error}");
                }
            }
            return Ok(());
        }
        Err(error) => {
            eprintln!("single-instance coordination failed: {error}");
            return Ok(());
        }
    };
    // An in-app update from 1.1.1 kept the installer's "MultiCore" entry and shortcuts.
    #[cfg(windows)]
    windows_settings::migrate_legacy_brand();
    // The updater relaunches the new version from a `\\?\` path; an autostart value written
    // in that (or any older) spelling of this app is put back in the plain form, kept on.
    #[cfg(windows)]
    match windows_settings::repair_launch_at_sign_in() {
        Ok(true) => eprintln!("autostart command repaired for this executable"),
        Ok(false) => {}
        Err(error) => eprintln!("autostart command could not be repaired: {error}"),
    }
    #[cfg(target_os = "linux")]
    if let Some(appimage) = appimage_handoff::handed_off_from() {
        match windows_settings::adopt_launch_at_sign_in_from(&appimage) {
            Ok(true) => eprintln!("autostart moved from the AppImage to the installed app"),
            Ok(false) => {}
            Err(error) => eprintln!("autostart could not be moved from the AppImage: {error}"),
        }
    }
    let launch_mode = launch_arguments.mode;
    // Before anything builds user-facing text: the language comes from the preferences.
    let (preference_store, loaded_preferences) = load_preferences();
    language::initialize(
        loaded_preferences.language,
        loaded_preferences.language_set_by_user,
        language::detect_system_language(),
    );
    let (client, _bootstrap_guard): (Arc<dyn DaemonClient>, Option<bootstrap::Bootstrap>) =
        match bootstrap::start() {
            Ok(bootstrap) => (bootstrap.client(), Some(bootstrap)),
            Err(error) => {
                eprintln!("bootstrap failed: {error:?}");
                let error = DaemonError::new(error.user_message());
                (Arc::new(UnavailableDaemonClient::new(error)), None)
            }
        };

    let preference_tracker = Arc::new(Mutex::new(PersistenceTracker::new(
        loaded_preferences.clone(),
    )));
    let preference_writer = Arc::new(Mutex::new(PreferenceWriter::start(preference_store)));

    let model = Arc::new(Mutex::new(DesktopViewModel::new(client)));
    let profile_preferences = Arc::new(Mutex::new(ProfilePreferenceReconciler::new(
        loaded_preferences.clone(),
        preference_tracker.clone(),
    )));
    updater::load_install_result();
    let update_state = Arc::new(Mutex::new(UpdateState::initial()));
    let update_check_started_at = Rc::new(Cell::new(Instant::now()));

    #[cfg(target_os = "linux")]
    select_linux_backend(software_renderer)?;
    #[cfg(not(target_os = "linux"))]
    if software_renderer {
        renderer_fallback::select_software_renderer()?;
    } else {
        renderer_fallback::select_gpu_renderer()?;
    }
    let ui = AppWindow::new()?;
    // Slint picks a bundled translation from the system locale when the first component is
    // created; the app's own language (Russian unless chosen or set by the provider) wins.
    select_slint_language(language::current());
    sync_language(&ui);
    // Ambient motion only while the window has focus; winit reports it once shown.
    ui.set_window_active(false);
    let focus_ui = ui.as_weak();
    layout_shortcuts::install(ui.window(), move |focused| {
        if let Some(ui) = focus_ui.upgrade() {
            ui.set_window_active(focused);
        }
    });
    #[cfg(target_os = "linux")]
    ui.set_system_name("Linux".into());
    #[cfg(target_os = "macos")]
    ui.set_system_name("macOS".into());
    let _logo_cache_session = logo_cache::LogoCacheSession::install();
    if preference_writer
        .lock()
        .expect("preference writer lock")
        .store
        .is_none()
    {
        ui.set_preference_error(preference_save_error().into());
    }
    apply_snapshot(&ui, &snapshot(&model.lock().expect("view model lock")));
    ui.set_local_page(visible_page_name(&loaded_preferences.visible_page).into());
    ui.set_runtime_versions(env!("MULTICORE_RUNTIME_VERSIONS").into());
    ui.set_ambient_background_enabled(loaded_preferences.ambient_background);
    ui.set_navigation_collapsed(loaded_preferences.navigation_collapsed);
    ui.set_tun_enabled(loaded_preferences.tun_enabled);
    // The TUN-off (system proxy) mode exists on Windows only.
    ui.set_tun_toggle_available(cfg!(windows));
    // A connection that outlived the previous run was made in the saved mode.
    model
        .lock()
        .expect("view model lock")
        .set_connection_uses_tun(connect_uses_tun(&ui));
    if let Some(growth) = loaded_preferences.rail_growth {
        ui.set_rail_growth_delta(i32::try_from(growth.delta).unwrap_or(0));
        ui.set_rail_grown_width(i32::try_from(growth.grown_width).unwrap_or(0));
    }
    ui.set_announcement_seen(
        loaded_preferences
            .announcement_seen
            .clone()
            .unwrap_or_default()
            .into(),
    );
    ui.set_service_offer_dismissed(loaded_preferences.service_offer_dismissed);
    ui.set_handoff_notice_seen(
        loaded_preferences
            .appimage_handoff_seen
            .clone()
            .unwrap_or_default()
            .into(),
    );
    // Reached through an AppImage hand-off: a one-time toast says which copy runs. Set even
    // when starting hidden (autostart), so it shows when the window is next opened.
    #[cfg(target_os = "linux")]
    if let Some(appimage) = appimage_handoff::handed_off_from() {
        ui.set_handoff_notice_for(appimage.to_string_lossy().as_ref().into());
    }
    ui.set_auto_connect_on_startup(loaded_preferences.auto_connect_on_startup);
    ui.set_reduced_motion(reduced_motion_from_client_area_animation(
        client_area_animation_enabled(),
    ));
    apply_update_state(
        &ui,
        &update_state.lock().expect("update state lock"),
        runtime_allows_update(&model.lock().expect("view model lock").presentation()),
    );
    // Once after an update: «Обновлено до …» where the update toast sits. Set even when
    // starting hidden, so it shows when the window is next opened.
    if let Some(version) = updater::take_update_confirmation() {
        ui.set_update_done_version(version.into());
    }
    apply_launch_at_sign_in(&ui, windows_settings::query_launch_at_sign_in());
    apply_saved_placement(&ui, &loaded_preferences);
    let pending_subscription_imports = Arc::new(Mutex::new(PendingSubscriptionImports::default()));
    let pending_subscription_consents =
        Arc::new(Mutex::new(PendingSubscriptionConsents::default()));
    wire_import(
        &ui,
        model.clone(),
        profile_preferences.clone(),
        pending_subscription_imports.clone(),
    );
    wire_deep_link_consent(
        &ui,
        pending_subscription_consents.clone(),
        pending_subscription_imports.clone(),
    );
    let _pending_import_timer =
        wire_pending_subscription_imports(&ui, model.clone(), pending_subscription_imports.clone());
    wire_clipboard_import(&ui, model.clone());
    wire_local_proxy(&ui);
    wire_subscription_refresh(&ui, model.clone(), profile_preferences.clone());
    let _subscription_refresh_timer =
        wire_automatic_subscription_refresh(&ui, model.clone(), profile_preferences.clone());
    wire_profile_switcher(&ui, model.clone(), profile_preferences.clone());
    let startup_connect_pending = Rc::new(Cell::new(true));
    wire_primary_action(
        &ui,
        model.clone(),
        profile_preferences.clone(),
        startup_connect_pending.clone(),
    );
    wire_dashboard_action(&ui, model.clone());
    wire_service_install(&ui);
    let _troubleshoot_clock = wire_troubleshoot(&ui, model.clone());
    wire_data_reset(&ui);
    wire_panels(&ui, model.clone());
    wire_event_filter(&ui, model.clone());
    wire_log_range(&ui);
    wire_save_diagnostics(&ui, model.clone());
    wire_catalog_group(&ui, model.clone());
    wire_catalog_node(&ui, model.clone());
    wire_route_search(&ui, model.clone());
    let _latency_timer = wire_automatic_latency(&ui, model.clone());
    wire_manual_latency(&ui, model.clone());
    let _session_stats_timer = wire_session_stats(&ui, model.clone());
    let _cores_ready_timer = wire_cores_ready_watch(&ui, model.clone());
    let _connection_watch_timer = wire_connection_watch(&ui, model.clone());
    wire_conflicting_apps_banner(&ui, model.clone());
    let _conflicting_apps_timer = wire_conflicting_apps_scan(&ui, model.clone());
    wire_windows_settings(&ui);
    wire_external_activations(
        &ui,
        activations,
        model.clone(),
        pending_subscription_consents.clone(),
        pending_subscription_imports.clone(),
    );
    let tray_popup_open = Rc::new(Cell::new(false));
    let tray_popup_geometry = Rc::new(TrayPopupGeometry::new(loaded_preferences.tray_popup_size));
    let tray_popup_slot = {
        let geometry = tray_popup_geometry.clone();
        let tracker = preference_tracker.clone();
        let writer = preference_writer.clone();
        let tray_popup_open = tray_popup_open.clone();
        let model = model.clone();
        let ui = ui.as_weak();
        Rc::new(TrayPopupSlot::new(
            tray_popup_open.clone(),
            Box::new(move || {
                let tray_popup = create_tray_popup()?;
                wire_tray_popup_resize(&tray_popup, geometry.clone(), tracker.clone());
                wire_tray_popup_actions(
                    &tray_popup,
                    ui.clone(),
                    model.clone(),
                    writer.clone(),
                    tracker.clone(),
                    tray_popup_open.clone(),
                );
                let focus_timer = wire_tray_popup_focus_loss(
                    &tray_popup,
                    tray_popup_open.clone(),
                    model.clone(),
                    ui.clone(),
                );
                Ok((tray_popup, focus_timer))
            }),
        ))
    };
    // Linux keeps one popup for the whole run; Windows creates it on the first open.
    #[cfg(not(windows))]
    tray_popup_slot.get_or_create()?;
    #[cfg(windows)]
    let _tray_popup_release_timer = wire_tray_popup_release(tray_popup_slot.clone());
    let tray_model = tray_menu_model(&model.lock().expect("view model lock"));
    let tray_model_source = model.clone();
    let tray_action_ui = ui.as_weak();
    let tray_action_model = model.clone();
    let tray_preference_writer = preference_writer.clone();
    let tray_preference_tracker = preference_tracker.clone();
    let tray_interaction_popup = tray_popup_slot.clone();
    let tray_interaction_popup_open = tray_popup_open.clone();
    let tray_runtime = tray::start(
        tray_model,
        move || tray_menu_model(&tray_model_source.lock().expect("view model lock")),
        move |interaction| match interaction {
            TrayInteraction::Command(command) => {
                hide_tray_popup(&tray_interaction_popup.weak(), &tray_interaction_popup_open);
                handle_tray_command(
                    &tray_action_ui,
                    &tray_action_model,
                    command,
                    &tray_preference_writer,
                    &tray_preference_tracker,
                )
            }
            TrayInteraction::ShowMenu { x, y } => {
                toggle_tray_popup(
                    &tray_interaction_popup,
                    &tray_action_ui,
                    &tray_action_model,
                    &tray_popup_geometry,
                    PhysicalPosition { x, y },
                );
            }
        },
    )
    .map_err(|error| eprintln!("tray unavailable: {error}"))
    .ok();
    wire_window_controls(&ui, tray_runtime.is_some());
    wire_language(
        &ui,
        model.clone(),
        update_state.clone(),
        tray_popup_slot.clone(),
    );
    let _preference_timer = wire_preference_persistence(
        &ui,
        model.clone(),
        preference_writer.clone(),
        preference_tracker.clone(),
    );
    wire_updater(
        &ui,
        model.clone(),
        update_state.clone(),
        update_check_started_at.clone(),
    );
    let _update_check_timer = wire_periodic_update_check(
        &ui,
        model.clone(),
        update_state.clone(),
        update_check_started_at.clone(),
    );
    if updater::configured_repository().ok().flatten().is_some() {
        run_update_check(
            ui.as_weak(),
            model.clone(),
            update_state,
            update_check_started_at,
            false,
        );
    }
    run_refresh(ui.as_weak(), model.clone(), profile_preferences);
    if let Some(subscription_url) = launch_arguments.subscription_url {
        queue_subscription_import(
            &ui,
            &pending_subscription_consents,
            &pending_subscription_imports,
            subscription_url,
        );
    }
    let _startup_connect_timer = wire_startup_auto_connect(
        &ui,
        model.clone(),
        launch_mode,
        loaded_preferences.auto_connect_on_startup,
        startup_connect_pending,
    );
    let starts_visible = initial_window_visible(launch_mode, tray_runtime.is_some());
    ui.set_shell_active(starts_visible);
    if starts_visible {
        ui.show()?;
    }
    // Lets the Windows updater discard the previous version only once this
    // build has its daemon and event loop running.
    #[cfg(not(target_os = "linux"))]
    if _bootstrap_guard.is_some() {
        slint::Timer::single_shot(Duration::from_secs(2), updater::confirm_started);
    }
    let result = event_loop_guard::run_until_quit(|| {
        recreate_windows_after_renderer_failure(&ui, &tray_popup_slot)
    });
    flush_preferences(&ui, &model, &preference_writer, &preference_tracker);
    if let Some(tray_popup) = tray_popup_slot.weak().upgrade() {
        let _ = tray_popup.hide();
    }
    let _ = ui.hide();
    drop(tray_runtime);
    result
}

/// The event loop stopped on a renderer failure (`event_loop_guard`): every window gets a new
/// native window and renderer (on Windows a new Direct3D device). The main window comes back
/// if it was on screen and stays hidden otherwise, recreated on its next `show()`; the tray
/// popup closes and is made anew when next opened.
fn recreate_windows_after_renderer_failure(ui: &AppWindow, tray_popup_slot: &TrayPopupSlot) {
    if let Some(popup) = tray_popup_slot.weak().upgrade() {
        // Destroyed while still shown: Slint ignores a hide of a window it already hid.
        popup.set_resize_active("".into());
        if let Err(error) = event_loop_guard::destroy_native_window(popup.window()) {
            eprintln!("tray popup renderer not recreated: {error}");
        }
        tray_popup_slot.open.set(false);
    }
    #[cfg(windows)]
    {
        // Out of the slot first: dropping the component runs its destructors.
        let released = tray_popup_slot.popup.borrow_mut().take();
        drop(released);
    }
    let shown = ui.get_shell_active();
    if let Err(error) = event_loop_guard::destroy_native_window(ui.window()) {
        eprintln!("main window renderer not recreated: {error}");
    }
    if shown && let Err(error) = ui.show() {
        eprintln!("main window not shown again: {error}");
    }
}

fn activation_for_launch_arguments(arguments: &LaunchArguments) -> single_instance::Activation {
    match (&arguments.subscription_url, arguments.mode) {
        (Some(url), _) => single_instance::Activation::InstallSubscription(url.clone()),
        (None, LaunchMode::Foreground) => single_instance::Activation::Show,
        (None, LaunchMode::Background) => single_instance::Activation::Background,
    }
}

fn prefill_subscription(ui: &AppWindow, subscription_url: String) {
    ui.set_local_page("subscriptions".into());
    ui.set_subscription_draft(subscription_url.into());
    ui.set_replace_editor_open(true);
}

fn present_pending_consent(ui: &AppWindow, consent: &Arc<Mutex<PendingSubscriptionConsents>>) {
    let host = consent
        .lock()
        .expect("pending subscription consents lock")
        .current_host()
        .map(str::to_owned);
    if let Some(host) = host {
        ui.set_deep_link_confirm_host(host.into());
        ui.set_deep_link_confirm_open(true);
        ui.set_deep_link_focus_serial(ui.get_deep_link_focus_serial().saturating_add(1));
    } else {
        ui.set_deep_link_confirm_open(false);
        ui.set_deep_link_confirm_host("".into());
    }
}

fn queue_subscription_import(
    ui: &AppWindow,
    consent: &Arc<Mutex<PendingSubscriptionConsents>>,
    pending: &Arc<Mutex<PendingSubscriptionImports>>,
    subscription_url: String,
) {
    let approved_count = pending
        .lock()
        .expect("pending subscription imports lock")
        .len();
    let mut consent_guard = consent.lock().expect("pending subscription consents lock");
    let was_empty = consent_guard.current_host().is_none();
    if approved_count + consent_guard.len() < MAX_PENDING_SUBSCRIPTION_IMPORTS
        && consent_guard.enqueue(subscription_url)
    {
        drop(consent_guard);
        if was_empty {
            present_pending_consent(ui, consent);
        }
    } else {
        ui.set_local_page("subscriptions".into());
        ui.set_replace_editor_open(true);
        ui.set_import_error(
            tr!("Не удалось открыть ссылку подписки. Проверьте её и повторите попытку.").into(),
        );
    }
}

fn wire_deep_link_consent(
    ui: &AppWindow,
    consent: Arc<Mutex<PendingSubscriptionConsents>>,
    pending: Arc<Mutex<PendingSubscriptionImports>>,
) {
    let weak = ui.as_weak();
    let approved_consent = consent.clone();
    ui.on_confirm_deep_link(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let approved = {
            let mut consent = approved_consent
                .lock()
                .expect("pending subscription consents lock");
            let mut pending = pending.lock().expect("pending subscription imports lock");
            consent.approve_next(&mut pending)
        };
        if approved {
            ui.set_local_page("subscriptions".into());
            ui.set_replace_editor_open(false);
            let host = pending
                .lock()
                .expect("pending subscription imports lock")
                .current_host()
                .unwrap_or_default();
            ui.set_deep_link_import_host(host.into());
        }
        present_pending_consent(&ui, &approved_consent);
    });

    let weak = ui.as_weak();
    ui.on_cancel_deep_link(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        consent
            .lock()
            .expect("pending subscription consents lock")
            .reject_next();
        present_pending_consent(&ui, &consent);
    });
}

fn wire_pending_subscription_imports(
    ui: &AppWindow,
    model: Arc<Mutex<DesktopViewModel>>,
    pending: Arc<Mutex<PendingSubscriptionImports>>,
) -> Timer {
    let timer = Timer::default();
    let weak = ui.as_weak();
    timer.start(TimerMode::Repeated, Duration::from_millis(250), move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if pending
            .lock()
            .expect("pending subscription imports lock")
            .is_empty()
        {
            return;
        }
        // A deep link is added like a manual import, connected or not.
        if !model
            .lock()
            .expect("view model lock")
            .can_begin_external_import()
        {
            return;
        }
        pending
            .lock()
            .expect("pending subscription imports lock")
            .dispatch_next(Instant::now(), |url| ui.invoke_import_url(url.into()));
    });
    timer
}

fn wire_external_activations(
    ui: &AppWindow,
    activations: std::sync::mpsc::Receiver<single_instance::Activation>,
    model: Arc<Mutex<DesktopViewModel>>,
    consent: Arc<Mutex<PendingSubscriptionConsents>>,
    pending: Arc<Mutex<PendingSubscriptionImports>>,
) {
    let weak = ui.as_weak();
    thread::spawn(move || {
        while let Ok(activation) = activations.recv() {
            let weak = weak.clone();
            let model = model.clone();
            let consent = consent.clone();
            let pending = pending.clone();
            if slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else {
                    return;
                };
                match activation {
                    single_instance::Activation::Background => {}
                    single_instance::Activation::Show => restore_and_focus(&ui),
                    single_instance::Activation::HandedOff(appimage) => {
                        ui.set_handoff_notice_for(appimage.into());
                    }
                    single_instance::Activation::InstallSubscription(url) => {
                        let current = {
                            let mut model = model.lock().expect("view model lock");
                            model.open_panel(Panel::None);
                            snapshot(&model)
                        };
                        apply_snapshot(&ui, &current);
                        queue_subscription_import(&ui, &consent, &pending, url);
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

fn startup_should_connect(
    mode: LaunchMode,
    enabled: bool,
    profile_ready: bool,
    network_ready: bool,
) -> bool {
    mode == LaunchMode::Background && enabled && profile_ready && network_ready
}

fn wire_startup_auto_connect(
    ui: &AppWindow,
    model: Arc<Mutex<DesktopViewModel>>,
    mode: LaunchMode,
    enabled_at_launch: bool,
    pending: Rc<Cell<bool>>,
) -> Timer {
    let timer = Timer::default();
    if !startup_should_connect(mode, enabled_at_launch, true, true) {
        return timer;
    }
    let weak = ui.as_weak();
    timer.start(TimerMode::Repeated, Duration::from_secs(2), move || {
        if !pending.get() {
            return;
        }
        let Some(ui) = weak.upgrade() else {
            pending.set(false);
            return;
        };
        if !ui.get_auto_connect_on_startup() {
            pending.set(false);
            return;
        }
        let profile_ready = {
            let model = model.lock().expect("view model lock");
            let presentation = model.presentation();
            let catalog = model.catalog_presentation();
            if presentation.is_connected || presentation.is_error {
                pending.set(false);
            }
            presentation.primary_action == PrimaryAction::Connect
                && presentation.primary_enabled
                && catalog.revision.is_some()
                && !catalog.loading
                && !catalog.groups.is_empty()
        };
        if !pending.get() || !profile_ready {
            return;
        }
        let network_ready = multicore_core::default_ipv4_interface().is_ok();
        if startup_should_connect(mode, enabled_at_launch, profile_ready, network_ready) {
            pending.set(false);
            ui.invoke_primary_action();
        }
    });
    timer
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

const INSTALL_SUBSCRIPTION_ROUTE: &str = "multicore://install-sub";
/// Happ-style form used by panel subscription pages (Remnawave's
/// `{{SUBSCRIPTION_LINK}}`): the subscription URL follows verbatim.
const ADD_SUBSCRIPTION_ROUTE: &str = "multicore://add/";

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
            if let Some(prefix) = argument.get(..ADD_SUBSCRIPTION_ROUTE.len())
                && prefix.eq_ignore_ascii_case(ADD_SUBSCRIPTION_ROUTE)
            {
                let target = &argument[ADD_SUBSCRIPTION_ROUTE.len()..];
                let is_raw = ["http://", "https://"].iter().any(|scheme| {
                    target
                        .get(..scheme.len())
                        .is_some_and(|head| head.eq_ignore_ascii_case(scheme))
                });
                let subscription_url = if is_raw {
                    target.to_owned()
                } else {
                    decode_percent_encoded_url(target)?
                };
                return deep_link_subscription(subscription_url);
            }
            let (route, query) = argument
                .split_once('?')
                .ok_or("unsupported launch argument")?;
            // Windows ShellExecute canonicalizes an empty authority path to `/`.
            // Scheme and host are case-insensitive; only an empty or root path is valid.
            if !route
                .strip_suffix('/')
                .unwrap_or(route)
                .eq_ignore_ascii_case(INSTALL_SUBSCRIPTION_ROUTE)
            {
                return Err("unsupported installer URI route");
            }
            let encoded = query
                .strip_prefix("url=")
                .ok_or("installer URI must contain one subscription URL")?;
            deep_link_subscription(decode_percent_encoded_url(encoded)?)
        }
        _ => Err("background mode and installer URI cannot be combined"),
    }
}

fn deep_link_subscription(subscription_url: String) -> Result<LaunchArguments, &'static str> {
    if subscription_url.len() > view_model::MAX_SUBSCRIPTION_URL_BYTES
        || subscription_url
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b' ')
    {
        return Err("subscription URL contains invalid bytes or is too long");
    }
    let parsed =
        reqwest::Url::parse(&subscription_url).map_err(|_| "subscription URL is malformed")?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || parsed.fragment().is_some()
    {
        return Err("subscription URL must use HTTP or HTTPS");
    }
    Ok(LaunchArguments {
        mode: LaunchMode::Foreground,
        subscription_url: Some(subscription_url),
    })
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

/// An error state (failed connect, unreachable daemon) has no tunnel up, so
/// it is as safe to update from as a ready one. Treating it as busy used to
/// route the install through the restart confirmation, whose "disconnect
/// first" path does nothing for these states.
fn runtime_allows_update(presentation: &StatePresentation) -> bool {
    !presentation.is_connected && !presentation.is_busy && !presentation.is_degraded
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UpdateConfirmPlan {
    InstallNow,
    DisconnectFirst,
    Wait,
}

/// What confirming "update and restart" may do right now. Only a
/// Disconnect/Cleanup primary action is ever started for an update: any other
/// action (connect, retry) would leave a mutation running that nothing
/// completes and silently block the main button.
fn update_confirm_plan(presentation: &StatePresentation) -> UpdateConfirmPlan {
    if runtime_allows_update(presentation) {
        UpdateConfirmPlan::InstallNow
    } else if !presentation.is_busy
        && matches!(
            presentation.primary_action,
            PrimaryAction::Disconnect | PrimaryAction::Cleanup
        )
    {
        UpdateConfirmPlan::DisconnectFirst
    } else {
        UpdateConfirmPlan::Wait
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UpdateInstallIntent {
    InstallNow,
    ConfirmRestart,
}

fn update_install_intent(runtime_idle: bool) -> UpdateInstallIntent {
    if runtime_idle {
        UpdateInstallIntent::InstallNow
    } else {
        UpdateInstallIntent::ConfirmRestart
    }
}

fn apply_update_state(ui: &AppWindow, state: &UpdateState, runtime_idle: bool) {
    let presentation = state.presentation(runtime_idle);
    ui.set_update_status(presentation.status.into());
    ui.set_update_detail(presentation.detail.into());
    ui.set_update_action_label(presentation.action_label.into());
    ui.set_update_action_enabled(presentation.action_enabled);
    ui.set_update_available(presentation.available);
    ui.set_update_busy(presentation.busy);
    ui.set_update_toast_title(presentation.toast_title.into());
    ui.set_update_toast_copy(presentation.toast_copy.into());
    ui.set_update_toast_key(presentation.toast_key.into());
    ui.set_update_in_progress(presentation.in_progress);
    ui.set_update_failed(presentation.failed);
    ui.set_update_progress(presentation.progress_permille as f32 / 1000.0);
}

fn load_preferences() -> (Option<PreferenceStore>, AppPreferences) {
    let store = match PreferenceStore::from_user_data_root() {
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

/// Physical inner widths `(from, to)` for a rail toggle, or `None` when the
/// window keeps its size. Updates `growth` for the collapse that follows.
fn resize_for_rail(
    window: &slint::winit_030::winit::window::Window,
    collapsed: bool,
    growth: &std::cell::Cell<Option<RailGrowth>>,
) -> Option<(u32, u32)> {
    let maximized = window.is_maximized();
    let inner = window.inner_size();
    if collapsed {
        return rail_shrink_width(growth.take(), maximized, inner.width)
            .map(|width| (inner.width, width));
    }
    let monitor = window.current_monitor()?;
    let (Some((work_position, work_size)), Ok(outer_position)) =
        (monitor_work_area(&monitor), window.outer_position())
    else {
        return None;
    };
    // Grow by exactly what the rail gains, so the content keeps its width.
    let wanted = (f64::from(EXPANDED_RAIL_WIDTH - COLLAPSED_RAIL_WIDTH) * window.scale_factor())
        .round() as u32;
    let delta = rail_grow_delta(
        maximized,
        outer_position.x,
        window.outer_size().width,
        work_position.x,
        work_size.width,
        wanted,
    );
    if delta == 0 {
        growth.set(None);
        return None;
    }
    let grown_width = inner.width + delta;
    let _ = set_current_monitor_max_size(window);
    growth.set(Some(RailGrowth { delta, grown_width }));
    Some((inner.width, grown_width))
}

/// Eases the window's inner width from `from` to `to` over RAIL_RESIZE_MS in
/// step with the rail's own width animation. The timer only runs for that
/// quarter second; a new toggle first lands the previous animation on its target.
fn animate_rail_resize(
    ui: &AppWindow,
    animation: &std::rc::Rc<(Timer, std::cell::Cell<Option<u32>>)>,
    from: u32,
    to: u32,
) {
    use slint::winit_030::winit::dpi::PhysicalSize;

    let resize = |ui: &AppWindow, width: u32| {
        let _ = ui.window().with_winit_window(|window| {
            let height = window.inner_size().height;
            let _ = window.request_inner_size(PhysicalSize::new(width, height));
        });
    };
    if ui.get_reduced_motion() || from == to {
        resize(ui, to);
        return;
    }
    animation.1.set(Some(to));
    // The rail follows the same eased progress as the window, frame by frame.
    let (rail_from, rail_to) = if to > from {
        (COLLAPSED_RAIL_WIDTH, EXPANDED_RAIL_WIDTH)
    } else {
        (EXPANDED_RAIL_WIDTH, COLLAPSED_RAIL_WIDTH)
    };
    ui.set_rail_width_live(rail_from as f32);
    let started = std::time::Instant::now();
    let weak = ui.as_weak();
    let state = std::rc::Rc::downgrade(animation);
    animation
        .0
        .start(TimerMode::Repeated, Duration::from_millis(16), move || {
            let (Some(ui), Some(state)) = (weak.upgrade(), state.upgrade()) else {
                return;
            };
            let elapsed = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
            resize(&ui, rail_resize_frame(from, to, elapsed, RAIL_RESIZE_MS));
            if elapsed >= RAIL_RESIZE_MS {
                state.0.stop();
                state.1.set(None);
                ui.set_rail_width_live(-1.0);
            } else {
                let rail = rail_resize_frame(rail_from, rail_to, elapsed, RAIL_RESIZE_MS);
                ui.set_rail_width_live(rail as f32);
            }
        });
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
            WriterResult::Failed(failure) => {
                // The GUI process has no console: the low-level log (in «Сохранить лог…») is
                // the only place this reason can be read from later.
                let line = failure.log_line();
                eprintln!("preferences save {line}");
                daemon::record_low_level("preferences_save_failed", line);
            }
        }
    }
}

fn preference_error_for_result(result: &WriterResult) -> &'static str {
    match result {
        WriterResult::Saved(_) => "",
        WriterResult::Failed(_) => preference_save_error(),
    }
}

fn flush_preferences(
    ui: &AppWindow,
    model: &Arc<Mutex<DesktopViewModel>>,
    writer: &Arc<Mutex<PreferenceWriter>>,
    tracker: &Arc<Mutex<PersistenceTracker>>,
) {
    let observation = observe_window(ui);
    let route_preferences = model.lock().expect("view model lock").route_preferences();
    let preferences = {
        let mut tracker = tracker.lock().expect("preference tracker lock");
        tracker.set_ambient_background(ui.get_ambient_background_enabled());
        tracker.set_auto_connect_on_startup(ui.get_auto_connect_on_startup());
        tracker.set_navigation_collapsed(ui.get_navigation_collapsed());
        tracker.set_announcement_seen(ui.get_announcement_seen().into());
        tracker.set_appimage_handoff_seen(ui.get_handoff_notice_seen().into());
        tracker.set_service_offer_dismissed(ui.get_service_offer_dismissed());
        tracker.set_rail_growth(ui.get_rail_growth_delta(), ui.get_rail_grown_width());
        tracker.set_tun_enabled(ui.get_tun_enabled());
        if let Some((profile, selected_group, selections)) = route_preferences {
            tracker.set_route_preferences(profile, selected_group, selections);
        }
        tracker.snapshot_for_flush(observation.as_ref())
    };
    let saved = writer
        .lock()
        .expect("preference writer lock")
        .stop_and_flush(&preferences);
    match saved {
        Ok(()) => {
            tracker
                .lock()
                .expect("preference tracker lock")
                .mark_persisted(preferences);
            ui.set_preference_error("".into());
        }
        Err(failure) => {
            ui.set_preference_error(preference_save_error().into());
            let line = failure.log_line();
            eprintln!("preferences flush {line}");
            daemon::record_low_level("preferences_flush_failed", line);
        }
    }
}

fn wire_preference_persistence(
    ui: &AppWindow,
    model: Arc<Mutex<DesktopViewModel>>,
    writer: Arc<Mutex<PreferenceWriter>>,
    tracker: Arc<Mutex<PersistenceTracker>>,
) -> Timer {
    let timer = Timer::default();
    let weak = ui.as_weak();
    let last_monitor_signature = Cell::new(None);
    let last_scale_factor = Cell::new(ui.window().scale_factor());
    timer.start(TimerMode::Repeated, Duration::from_millis(500), move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        // Logos and group icons are resampled for one scale factor (`crisp_image`); moving
        // to a display with another one redraws them at its exact pixel size.
        let scale_factor = ui.window().scale_factor();
        if scale_factor != last_scale_factor.replace(scale_factor) {
            apply_latest_snapshot(&ui, &model);
        }
        match ui.window().with_winit_window(current_monitor_signature) {
            Some(current) => {
                let previous = last_monitor_signature.get();
                if monitor_cap_changed(previous, current)
                    && let Some(applied) =
                        ui.window().with_winit_window(set_current_monitor_max_size)
                {
                    last_monitor_signature.set(applied);
                }
            }
            // Released while hidden in the tray: cap the recreated window again.
            None => last_monitor_signature.set(None),
        }
        process_writer_results(
            &ui,
            &writer.lock().expect("preference writer lock"),
            &tracker,
        );
        let observation = observe_window(&ui);
        let route_preferences = model.lock().expect("view model lock").route_preferences();
        let preferences = {
            let mut tracker = tracker.lock().expect("preference tracker lock");
            tracker.set_ambient_background(ui.get_ambient_background_enabled());
            tracker.set_auto_connect_on_startup(ui.get_auto_connect_on_startup());
            tracker.set_navigation_collapsed(ui.get_navigation_collapsed());
            tracker.set_announcement_seen(ui.get_announcement_seen().into());
            tracker.set_appimage_handoff_seen(ui.get_handoff_notice_seen().into());
            tracker.set_service_offer_dismissed(ui.get_service_offer_dismissed());
            tracker.set_rail_growth(ui.get_rail_growth_delta(), ui.get_rail_grown_width());
            tracker.set_tun_enabled(ui.get_tun_enabled());
            let (language, set_by_user) = language::persisted();
            tracker.set_language(language, set_by_user);
            if let Some((profile, selected_group, selections)) = route_preferences {
                tracker.set_route_preferences(profile, selected_group, selections);
            }
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
    check_started_at: Rc<Cell<Instant>>,
) {
    let weak = ui.as_weak();
    let action_model = model.clone();
    let action_state = state.clone();
    ui.on_update_action(move || {
        let runtime_idle =
            runtime_allows_update(&action_model.lock().expect("view model lock").presentation());
        let action = action_state
            .lock()
            .expect("update state lock")
            .presentation(runtime_idle)
            .action;
        match action {
            UpdateAction::Check => run_update_check(
                weak.clone(),
                action_model.clone(),
                action_state.clone(),
                check_started_at.clone(),
                true,
            ),
            UpdateAction::Install => match update_install_intent(runtime_idle) {
                UpdateInstallIntent::InstallNow => {
                    start_update_install(weak.clone(), action_model.clone(), action_state.clone());
                }
                UpdateInstallIntent::ConfirmRestart => {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_update_confirm_open(true);
                    }
                }
            },
            UpdateAction::None => {}
        }
    });

    let weak = ui.as_weak();
    ui.on_cancel_update_restart(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_update_confirm_open(false);
        }
    });

    let weak = ui.as_weak();
    ui.on_confirm_update_restart(move || {
        let (plan, current) = {
            let model = model.lock().expect("view model lock");
            (update_confirm_plan(&model.presentation()), snapshot(&model))
        };
        // Connected: only the download starts here. The tunnel stays up until the update is
        // downloaded and verified (see `continue_ready_update`).
        let started = match plan {
            UpdateConfirmPlan::InstallNow | UpdateConfirmPlan::DisconnectFirst => {
                start_update_install(weak.clone(), model.clone(), state.clone());
                true
            }
            UpdateConfirmPlan::Wait => false,
        };
        // Nothing could start (an operation is still running): keep the
        // dialog open instead of closing it as if the update had begun. Its
        // confirm button is disabled while the runtime is busy.
        if let Some(ui) = weak.upgrade() {
            if started {
                ui.set_update_confirm_open(false);
            }
            apply_snapshot(&ui, &current);
        }
    });
}

/// How long after the last update check the next one starts (plus `update_check_jitter`).
/// Hourly is cheap: a check reads the tiny `release.txt` and asks the rate-limited releases
/// API only when it names a newer version (or, without it, at most every 4 hours).
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Up to this much is added to each interval so clients started together do not all ask
/// GitHub at the same minute.
const UPDATE_CHECK_MAX_JITTER: Duration = Duration::from_secs(10 * 60);

fn update_check_due(elapsed: Duration, jitter: Duration, busy: bool) -> bool {
    elapsed >= UPDATE_CHECK_INTERVAL + jitter.min(UPDATE_CHECK_MAX_JITTER) && !busy
}

/// A fresh random delay in `0..=UPDATE_CHECK_MAX_JITTER` (whole seconds).
fn update_check_jitter() -> Duration {
    let mut bytes = [0_u8; 8];
    let random = getrandom::fill(&mut bytes).map_or(0, |()| u64::from_le_bytes(bytes));
    Duration::from_secs(random % (UPDATE_CHECK_MAX_JITTER.as_secs() + 1))
}

fn wire_periodic_update_check(
    ui: &AppWindow,
    model: Arc<Mutex<DesktopViewModel>>,
    state: Arc<Mutex<UpdateState>>,
    check_started_at: Rc<Cell<Instant>>,
) -> Timer {
    let timer = Timer::default();
    if updater::configured_repository().ok().flatten().is_none() {
        return timer;
    }
    let weak = ui.as_weak();
    let jitter = Cell::new(update_check_jitter());
    // A cheap once-a-minute look at the clock; the check itself runs about hourly.
    timer.start(TimerMode::Repeated, Duration::from_secs(60), move || {
        let busy = matches!(
            &*state.lock().expect("update state lock"),
            UpdateState::Checking | UpdateState::Updating(..)
        );
        if update_check_due(check_started_at.get().elapsed(), jitter.get(), busy) {
            jitter.set(update_check_jitter());
            run_update_check(
                weak.clone(),
                model.clone(),
                state.clone(),
                check_started_at.clone(),
                false,
            );
        }
    });
    timer
}

/// Whether an update is installed by the always-ready service instead of this process: the
/// service install lives in Program Files, which the desktop cannot write. Windows only;
/// Linux packages are updated by the package manager.
fn update_through_service(service_mode: bool) -> bool {
    cfg!(windows) && service_mode
}

/// Starts an update run of the offered release, in one step under the lock: a second press
/// (or a retry while one runs) finds no available release and never starts a second download.
fn begin_update_download(state: &Mutex<UpdateState>) -> Option<GitHubRelease> {
    state.lock().expect("update state lock").begin_update()
}

fn show_update_state(weak: &slint::Weak<AppWindow>, state: &Mutex<UpdateState>, idle: bool) {
    if let Some(ui) = weak.upgrade() {
        apply_update_state(&ui, &state.lock().expect("update state lock"), idle);
    }
}

/// On the event loop: moves the running update to `stage` (only forward) and redraws.
fn advance_update(weak: &slint::Weak<AppWindow>, state: &Mutex<UpdateState>, stage: UpdateStage) {
    let mut current = state.lock().expect("update state lock");
    if current.advance(stage)
        && let Some(ui) = weak.upgrade()
    {
        apply_update_state(&ui, &current, true);
    }
}

/// From a worker: hands a stage the download reached to the event loop. Reports arrive in
/// order, and before the worker's own result.
fn report_update_stage(
    weak: &slint::Weak<AppWindow>,
    state: &Arc<Mutex<UpdateState>>,
    stage: UpdateStage,
) {
    let (weak, state) = (weak.clone(), state.clone());
    let _ = slint::invoke_from_event_loop(move || advance_update(&weak, &state, stage));
}

/// How often the service's download progress is asked for while it stages an update.
const SERVICE_PROGRESS_POLL: Duration = Duration::from_millis(500);

/// The stage a progress answer of the service means; `None` when it has nothing running
/// (yet, or any more). A size the service does not know yet is the release's.
fn service_update_stage(progress: DaemonUpdateProgress, release_size: u64) -> Option<UpdateStage> {
    match progress.stage {
        DaemonUpdateStage::Idle => None,
        DaemonUpdateStage::Downloading => Some(UpdateStage::Downloading(
            multicore_release::DownloadProgress {
                received: progress.received,
                total: if progress.total > 0 {
                    progress.total
                } else {
                    release_size
                },
            },
        )),
        DaemonUpdateStage::Verifying => Some(UpdateStage::Verifying),
    }
}

/// The service downloads and verifies `release` (`StageUpdate`) while a poller asks for its
/// progress on a channel of its own; without an answer (an older service) the bar just has no
/// figures. The poller ends with the staging.
fn stage_through_service(
    client: Arc<dyn DaemonClient>,
    release: &GitHubRelease,
    report: impl Fn(UpdateStage) + Send + 'static,
) -> Result<ReadyPackage, String> {
    let done = Arc::new(AtomicBool::new(false));
    {
        let (client, done, size) = (client.clone(), done.clone(), release.asset().size());
        thread::spawn(move || {
            loop {
                thread::sleep(SERVICE_PROGRESS_POLL);
                if done.load(Ordering::Acquire) {
                    break;
                }
                if let Some(stage) = client
                    .service_update_progress()
                    .ok()
                    .and_then(|progress| service_update_stage(progress, size))
                {
                    report(stage);
                }
            }
        });
    }
    let result = client
        .stage_update_via_service(&release.version().to_string())
        .map(|()| ReadyPackage::Service)
        .map_err(|error| error.to_string());
    done.store(true, Ordering::Release);
    result
}

/// How long «Перезапуск…» stays on screen before the app quits for the updater (which waits
/// for this process to exit): long enough to be seen, short against the swap itself.
const RESTART_NOTICE: Duration = Duration::from_millis(700);

/// From a worker: the updater took over (show «Перезапуск…», note the version for the next
/// start, then quit so it can replace this client), or the install failed and the release
/// stays offered with the reason.
fn finish_update_install(
    weak: slint::Weak<AppWindow>,
    state: Arc<Mutex<UpdateState>>,
    release: GitHubRelease,
    result: Result<(), String>,
) {
    let _ = slint::invoke_from_event_loop(move || match result {
        Ok(()) => {
            advance_update(&weak, &state, UpdateStage::Restarting);
            updater::remember_restart_into(&release.version());
            Timer::single_shot(RESTART_NOTICE, move || {
                let _keep_state_alive = state;
                let _keep_window_alive = weak;
                event_loop_guard::request_quit();
            });
        }
        Err(error) => update_install_failed(&weak, &state, release, error),
    });
}

fn update_install_failed(
    weak: &slint::Weak<AppWindow>,
    state: &Mutex<UpdateState>,
    release: GitHubRelease,
    error: String,
) {
    let mut current = state.lock().expect("update state lock");
    // Keep the release so the home banner stays with the reason
    // and a retry, instead of vanishing as a check error would.
    *current = UpdateState::InstallFailed(release, error);
    if let Some(ui) = weak.upgrade() {
        apply_update_state(&ui, &current, true);
    }
}

/// A downloaded and verified update, waiting for the tunnel to stop.
struct ReadyUpdate {
    release: GitHubRelease,
    package: ReadyPackage,
}

enum ReadyPackage {
    /// Per-user install: the verified package in the user's update directory.
    Local(updater::StagedUpdate),
    /// Service install: the service holds the verified release; `Update` hands it over.
    Service,
}

/// How long a ready update waits for a running operation (connect, switch, reconciliation)
/// before looking again, and how often it looks before giving up (about a minute).
const READY_UPDATE_WAIT: Duration = Duration::from_millis(500);
const MAX_READY_UPDATE_WAITS: u32 = 120;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadyUpdateStep {
    Apply,
    Disconnect,
    WaitAgain,
    GiveUp,
}

/// What a downloaded update does next. Idle → apply. Connected → disconnect, but only once:
/// still connected after our disconnect (it failed, or the user connected again) means the
/// update gives up and the user keeps the connection. Busy → wait, but not forever.
fn ready_update_step(
    plan: UpdateConfirmPlan,
    disconnect_tried: bool,
    waits: u32,
) -> ReadyUpdateStep {
    match plan {
        UpdateConfirmPlan::InstallNow => ReadyUpdateStep::Apply,
        _ if waits >= MAX_READY_UPDATE_WAITS => ReadyUpdateStep::GiveUp,
        UpdateConfirmPlan::DisconnectFirst if disconnect_tried => ReadyUpdateStep::GiveUp,
        UpdateConfirmPlan::DisconnectFirst => ReadyUpdateStep::Disconnect,
        UpdateConfirmPlan::Wait => ReadyUpdateStep::WaitAgain,
    }
}

/// Downloads and verifies first while everything keeps running (a service install has the
/// service stage the release; a per-user install downloads into its update directory), with
/// the real progress on screen. Only a ready update goes on (`continue_ready_update`): applied
/// at once when idle, after stopping the tunnel when connected. A failed download leaves the
/// connection alone and offers a retry.
fn start_update_install(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    state: Arc<Mutex<UpdateState>>,
) {
    let Some(release) = begin_update_download(&state) else {
        return;
    };
    if let Some(ui) = weak.upgrade() {
        // A run shows its toast even where an earlier one was hidden.
        ui.set_update_banner_dismissed_for(SharedString::new());
    }
    show_update_state(&weak, &state, true);
    let (service_mode, client) = {
        let model = model.lock().expect("view model lock");
        (model.service_mode(), model.daemon_client())
    };
    thread::spawn(move || {
        let report = {
            let (weak, state) = (weak.clone(), state.clone());
            move |stage| report_update_stage(&weak, &state, stage)
        };
        let result = if update_through_service(service_mode) {
            stage_through_service(client, &release, report)
        } else {
            updater::download_and_verify(&release, report).map(ReadyPackage::Local)
        };
        let _ = slint::invoke_from_event_loop(move || match result {
            Ok(package) => {
                advance_update(&weak, &state, UpdateStage::Installing);
                let ready = ReadyUpdate { release, package };
                continue_ready_update(weak, model, state, ready, false, 0);
            }
            Err(error) => update_install_failed(&weak, &state, release, error),
        });
    });
}

/// On the event loop: moves a ready update on by `ready_update_step`. Runs again after its
/// own disconnect and after each wait, so a user who disconnected (or quit) meanwhile, a
/// reconciliation, or a running switch are all taken as they are at that moment.
fn continue_ready_update(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    state: Arc<Mutex<UpdateState>>,
    ready: ReadyUpdate,
    disconnect_tried: bool,
    waits: u32,
) {
    let (step, action, client) = {
        let mut model = model.lock().expect("view model lock");
        let presentation = model.presentation();
        let plan = match update_confirm_plan(&presentation) {
            // Another operation (import, selection) holds the main button: wait for it.
            UpdateConfirmPlan::DisconnectFirst if !presentation.primary_enabled => {
                UpdateConfirmPlan::Wait
            }
            plan => plan,
        };
        let step = ready_update_step(plan, disconnect_tried, waits);
        // Checked under the same lock, so begin_primary_action can only
        // start the Disconnect/Cleanup the step was based on.
        let action = match step {
            ReadyUpdateStep::Disconnect => model.begin_primary_action(),
            _ => None,
        };
        (step, action, model.daemon_client())
    };
    match (step, action) {
        (ReadyUpdateStep::Apply, _) => apply_ready_update(weak, model, state, ready),
        (
            ReadyUpdateStep::Disconnect,
            Some((PrimaryAction::Disconnect | PrimaryAction::Cleanup, mutation)),
        ) => {
            if let Some(ui) = weak.upgrade() {
                apply_latest_snapshot(&ui, &model);
            }
            let (after_weak, after_model) = (weak.clone(), model.clone());
            run_disconnect_for_update(
                weak,
                model,
                client,
                mutation,
                Box::new(move || {
                    continue_ready_update(after_weak, after_model, state, ready, true, 0);
                }),
            );
        }
        (ReadyUpdateStep::GiveUp, _) => {
            update_install_failed(
                &weak,
                &state,
                ready.release,
                tr!("Подключение не остановилось, обновление не установлено. Повторите установку.")
                    .into(),
            );
            let has_profile = model
                .lock()
                .expect("view model lock")
                .presentation()
                .has_profile;
            if has_profile {
                load_catalog(weak, model);
            }
        }
        _ => {
            Timer::single_shot(READY_UPDATE_WAIT, move || {
                continue_ready_update(weak, model, state, ready, disconnect_tried, waits + 1);
            });
        }
    }
}

/// The tunnel is down: hand the verified update over and quit for it.
fn apply_ready_update(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    state: Arc<Mutex<UpdateState>>,
    ready: ReadyUpdate,
) {
    let client = model.lock().expect("view model lock").daemon_client();
    thread::spawn(move || {
        let ReadyUpdate { release, package } = ready;
        let result = match package {
            ReadyPackage::Local(staged) => updater::start_apply(&release, &staged),
            // The service re-checks the staged archive and installs it without downloading.
            ReadyPackage::Service => client
                .update_via_service(&release.version().to_string())
                .map_err(|error| error.to_string()),
        };
        finish_update_install(weak, state, release, result);
    });
}

fn run_update_check(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    state: Arc<Mutex<UpdateState>>,
    check_started_at: Rc<Cell<Instant>>,
    manual: bool,
) {
    check_started_at.set(Instant::now());
    let previous = {
        let mut current = state.lock().expect("update state lock");
        let previous = std::mem::replace(&mut *current, UpdateState::Checking);
        if let Some(ui) = weak.upgrade() {
            let runtime_idle =
                runtime_allows_update(&model.lock().expect("view model lock").presentation());
            apply_update_state(&ui, &current, runtime_idle);
        }
        previous
    };
    let known = previous
        .available_release()
        .map(|release| release.version());
    thread::spawn(move || {
        // Nothing new (rate limited, or the API fallback is not due): keep what was shown.
        let result = match updater::check_latest(manual, known) {
            updater::CheckOutcome::State(state) => updater::with_pending_install_failure(state),
            updater::CheckOutcome::Unchanged => match previous {
                UpdateState::Checking => UpdateState::Deferred,
                previous => previous,
            },
        };
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

/// After a compositor-driven move or resize the window manager owns the pointer, and on Linux
/// (Wayland, and X11's _NET_WM_MOVERESIZE) the button release that ends it never reaches us:
/// Slint keeps the title bar's TouchArea grabbed, so the first click afterwards is swallowed.
/// A pointer exit clears that grab. It is dispatched once the current input event has been
/// handled, because Slint restores its mouse state after dispatch and would drop a nested event.
#[cfg(target_os = "linux")]
fn release_pointer_grab_after_compositor_move(ui: &AppWindow) {
    let weak = ui.as_weak();
    Timer::single_shot(Duration::ZERO, move || {
        if let Some(ui) = weak.upgrade() {
            ui.window()
                .dispatch_event(slint::platform::WindowEvent::PointerExited);
        }
    });
}

/// Windows runs the move in a modal loop and hands the release back, so nothing is stuck there.
#[cfg(not(target_os = "linux"))]
fn release_pointer_grab_after_compositor_move(_ui: &AppWindow) {}

fn wire_window_controls(ui: &AppWindow, tray_available: bool) {
    let weak = ui.as_weak();
    ui.on_window_drag(move || {
        if let Some(ui) = weak.upgrade() {
            let _ = ui.window().with_winit_window(|window| window.drag_window());
            release_pointer_grab_after_compositor_move(&ui);
        }
    });

    let weak = ui.as_weak();
    ui.on_window_minimize(move || match minimize_disposition(tray_available) {
        MinimizeDisposition::HideToTray => {
            if let Some(ui) = weak.upgrade() {
                ui.set_shell_active(false);
                hide_window_to_tray(&ui);
            }
        }
        MinimizeDisposition::MinimizeToTaskbar => {
            if let Some(ui) = weak.upgrade() {
                ui.set_shell_active(shell_active_after_native_minimize_change(true));
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
                let _ = window.drag_resize_window(edge.into());
            });
            release_pointer_grab_after_compositor_move(&ui);
        }
    });

    // The window grows with the expanded rail so the content keeps its width,
    // and shrinks back on collapse unless the user resized it in between.
    let weak = ui.as_weak();
    let rail_animation = std::rc::Rc::new((Timer::default(), std::cell::Cell::new(None::<u32>)));
    ui.on_navigation_toggled(move |collapsed| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        // The growth lives on the window in logical px (persisted with the other
        // preferences); resize_for_rail works in physical px for this monitor.
        let scale = f64::from(ui.window().scale_factor());
        let to_physical = |logical: i32| (f64::from(logical.max(0)) * scale).round() as u32;
        let growth = std::cell::Cell::new((ui.get_rail_growth_delta() > 0).then(|| RailGrowth {
            delta: to_physical(ui.get_rail_growth_delta()),
            grown_width: to_physical(ui.get_rail_grown_width()),
        }));
        // Land an in-flight animation first, so the new toggle measures real sizes.
        if let Some(target) = rail_animation.1.take() {
            rail_animation.0.stop();
            ui.set_rail_width_live(-1.0);
            let _ = ui.window().with_winit_window(|window| {
                let height = window.inner_size().height;
                let _ = window.request_inner_size(slint::winit_030::winit::dpi::PhysicalSize::new(
                    target, height,
                ));
            });
        }
        let widths = ui
            .window()
            .with_winit_window(|window| resize_for_rail(window, collapsed, &growth))
            .flatten();
        if let Some((from, to)) = widths {
            animate_rail_resize(&ui, &rail_animation, from, to);
        }
        let to_logical = |physical: u32| (f64::from(physical) / scale).round() as i32;
        let (delta, grown_width) = growth
            .get()
            .map(|growth| (to_logical(growth.delta), to_logical(growth.grown_width)))
            .unwrap_or((0, 0));
        ui.set_rail_growth_delta(delta);
        ui.set_rail_grown_width(grown_width);
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
                hide_window_to_tray(&ui);
            }
        }
        CloseDisposition::Exit => {
            event_loop_guard::request_quit();
        }
    }
}

/// Wayland cannot hide a native Winit window: Slint must destroy its surface
/// and recreate it on show(). The explicit run_event_loop_until_quit() keeps
/// tray events and timers alive while there are no visible windows.
/// Windows hides only the native window, so reopening is instant, and releases its
/// renderer once it stays hidden (`renderer_release`).
fn hide_window_to_tray(ui: &AppWindow) {
    #[cfg(target_os = "linux")]
    {
        let window = ui.window();
        let size = window.size().to_logical(window.scale_factor());
        let maximized = window.is_maximized();
        match ui.hide() {
            Ok(()) if !window.has_winit_window() => {
                // Slint 1.17 recreates Wayland windows from default attributes.
                // An explicit size avoids its first-show preferred-size reset.
                window.set_size(size);
                if maximized {
                    // Refresh the attributes even if Slint's cached state is true.
                    window.set_maximized(false);
                    window.set_maximized(true);
                }
            }
            Ok(()) => {}
            Err(error) => {
                ui.set_shell_active(true);
                eprintln!("failed to hide window to tray: {error}");
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = ui.window().with_winit_window(|window| {
            window.set_visible(false);
        });
        #[cfg(windows)]
        renderer_release::release_when_left_hidden(ui);
    }
}

fn apply_tray_popup_model(popup: &TrayPopup, model: TrayMenuModel) {
    let scale = popup.window().scale_factor();
    popup.set_dumb_mode(model.dumb_mode);
    // The popup shows one group at a time: the one selected on the home screen.
    let shown_group = model
        .groups
        .iter()
        .find(|entry| entry.checked)
        .or_else(|| model.groups.first())
        .and_then(|entry| match &entry.command {
            TrayCommand::SelectGroup { group_id } => Some(group_id.clone()),
            _ => None,
        });
    let groups = model
        .groups
        .iter()
        .take(12)
        .filter_map(|entry| match &entry.command {
            TrayCommand::SelectGroup { group_id } => Some(TrayMenuRow {
                id: group_id.clone().into(),
                group_id: "".into(),
                icon: entry.icon.clone().into(),
                icon_image: catalog_icon_image(
                    entry.icon_id.as_deref(),
                    &entry.icon,
                    TRAY_SEGMENT_ICON_PX,
                    scale,
                ),
                icon_loading: entry.icon_pending,
                label: entry.label.clone().into(),
                detail: entry.detail.clone().into(),
                tone: "".into(),
                selected: shown_group.as_deref() == Some(group_id.as_str()),
                enabled: entry.enabled,
                measuring: false,
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    let nodes = model
        .nodes
        .iter()
        .take(64)
        .filter_map(|entry| match &entry.command {
            TrayCommand::SelectNode { group_id, node_id }
                if shown_group.as_deref() == Some(group_id.as_str()) =>
            {
                Some(TrayMenuRow {
                    id: node_id.clone().into(),
                    group_id: group_id.clone().into(),
                    icon: entry.icon.clone().into(),
                    icon_image: catalog_icon_image(
                        entry.icon_id.as_deref(),
                        &entry.icon,
                        TRAY_ROW_ICON_PX,
                        scale,
                    ),
                    icon_loading: entry.icon_pending,
                    label: entry.label.clone().into(),
                    detail: entry.detail.clone().into(),
                    tone: entry.tone.clone().into(),
                    selected: entry.checked,
                    enabled: entry.enabled,
                    measuring: entry.measuring,
                })
            }
            _ => None,
        })
        .collect::<Vec<_>>();

    popup.set_status_text(model.status.into());
    popup.set_connection_label(model.connection_label.into());
    popup.set_connection_enabled(model.connection_enabled);
    popup.set_connected(model.connected);
    popup.set_has_profile(model.has_profile);
    popup.set_group_rows(ModelRc::new(VecModel::from(groups)));
    popup.set_node_rows(ModelRc::new(VecModel::from(nodes)));
}

/// Mirrors what the main window already shows (subscription, connection state,
/// real exit node, live timer) into the tray popup. Only runs while the popup is
/// open, and only writes values that changed, so an idle popup does not redraw.
fn sync_tray_live_state(popup: &TrayPopup, main: &AppWindow) {
    macro_rules! mirror {
        ($get:ident, $set:ident, $source:expr) => {{
            let value = $source;
            if popup.$get() != value {
                popup.$set(value);
            }
        }};
    }
    mirror!(
        get_subscription_title,
        set_subscription_title,
        main.get_subscription_title()
    );
    mirror!(
        get_subscription_remaining,
        set_subscription_remaining,
        main.get_subscription_remaining()
    );
    mirror!(
        get_subscription_days,
        set_subscription_days,
        main.get_subscription_days()
    );
    mirror!(
        get_subscription_used_fraction,
        set_subscription_used_fraction,
        main.get_subscription_used_fraction()
    );
    mirror!(
        get_subscription_unlimited,
        set_subscription_unlimited,
        main.get_subscription_unlimited()
    );
    mirror!(
        get_subscription_quota_tone,
        set_subscription_quota_tone,
        main.get_subscription_quota_tone()
    );
    mirror!(
        get_subscription_failed,
        set_subscription_failed,
        !main.get_subscription_refresh_error().is_empty()
    );
    mirror!(get_toggle_title, set_toggle_title, main.get_toggle_title());
    mirror!(get_toggle_on, set_toggle_on, main.get_toggle_on());
    mirror!(get_toggle_busy, set_toggle_busy, main.get_toggle_busy());
    mirror!(
        get_toggle_detail_is_exit,
        set_toggle_detail_is_exit,
        main.get_toggle_detail_is_exit()
    );
    mirror!(get_status_tone, set_status_tone, main.get_status_tone());
    mirror!(get_supporting, set_supporting, main.get_supporting());
    mirror!(
        get_session_uptime,
        set_session_uptime,
        main.get_session_uptime()
    );
    mirror!(get_exit_latency, set_exit_latency, main.get_node_latency());
    mirror!(
        get_reduced_motion,
        set_reduced_motion,
        main.get_reduced_motion()
    );
    // The real node behind an automatic group when known, else the exit itself.
    let real = main.get_exit_now_name();
    let (name, icon, image) = if real.is_empty() {
        (
            main.get_node_name(),
            main.get_node_icon(),
            main.get_node_icon_image(),
        )
    } else {
        (
            real,
            main.get_exit_now_icon(),
            main.get_exit_now_icon_image(),
        )
    };
    mirror!(get_exit_name, set_exit_name, name);
    mirror!(get_exit_icon, set_exit_icon, icon);
    mirror!(get_exit_icon_image, set_exit_icon_image, image);
}

fn hide_tray_popup(weak: &slint::Weak<TrayPopup>, open: &Rc<Cell<bool>>) {
    if let Some(popup) = weak.upgrade() {
        popup.set_resize_active("".into());
        let _ = popup.hide();
    }
    open.set(false);
}

/// The tray popup and its focus-loss timer. Linux creates it once at startup. On Windows
/// it exists only while it is used: each window draws with its own Direct3D device, which
/// alone keeps tens of megabytes of driver memory, so the popup is created on the first
/// open and dropped (window, device and all) once it has stayed closed for about
/// `renderer_release::RELEASE_AFTER` (`wire_tray_popup_release`).
struct TrayPopupSlot {
    popup: RefCell<Option<(TrayPopup, Timer)>>,
    create: Box<dyn Fn() -> Result<(TrayPopup, Timer), slint::PlatformError>>,
    open: Rc<Cell<bool>>,
    /// When the popup was last seen open.
    #[cfg(windows)]
    last_open: Cell<std::time::Instant>,
}

impl TrayPopupSlot {
    fn new(
        open: Rc<Cell<bool>>,
        create: Box<dyn Fn() -> Result<(TrayPopup, Timer), slint::PlatformError>>,
    ) -> Self {
        Self {
            popup: RefCell::new(None),
            create,
            open,
            #[cfg(windows)]
            last_open: Cell::new(std::time::Instant::now()),
        }
    }

    /// The current popup, or a dangling handle when there is none.
    fn weak(&self) -> slint::Weak<TrayPopup> {
        self.popup
            .borrow()
            .as_ref()
            .map(|(popup, _)| popup.as_weak())
            .unwrap_or_default()
    }

    /// The popup, created first when there is none (then `true`).
    fn get_or_create(&self) -> Result<(TrayPopup, bool), slint::PlatformError> {
        if let Some((popup, _)) = self.popup.borrow().as_ref() {
            return Ok((popup.clone_strong(), false));
        }
        let (popup, focus_timer) = (self.create)()?;
        let handle = popup.clone_strong();
        *self.popup.borrow_mut() = Some((popup, focus_timer));
        Ok((handle, true))
    }
}

/// Drops the closed tray popup about `RELEASE_AFTER` after it was last open.
#[cfg(windows)]
fn wire_tray_popup_release(slot: Rc<TrayPopupSlot>) -> Timer {
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_secs(15), move || {
        if slot.open.get() {
            slot.last_open.set(std::time::Instant::now());
        } else if slot.last_open.get().elapsed() >= renderer_release::RELEASE_AFTER {
            // Out of the slot first: dropping the component runs its destructors.
            let released = slot.popup.borrow_mut().take();
            drop(released);
        }
    });
    timer
}

/// Size and resize state of the tray popup, shared by opening and the edge drags.
struct TrayPopupGeometry {
    /// Logical size the user chose (or the default), restored on every open.
    size: Cell<(u32, u32)>,
    grip: Cell<TrayPopupGrip>,
    drag: RefCell<Option<TrayResizeDrag>>,
}

struct TrayResizeDrag {
    start: PhysicalRect,
    cursor: Option<(i32, i32)>,
    handle: TrayResizeHandle,
    monitors: Vec<MonitorRect>,
}

impl TrayPopupGeometry {
    fn new(saved: Option<TrayPopupSizePreference>) -> Self {
        let size = saved.map_or(TRAY_POPUP_MIN, |size| {
            (
                size.width.clamp(TRAY_POPUP_MIN.0, TRAY_POPUP_MAX.0),
                size.height.clamp(TRAY_POPUP_MIN.1, TRAY_POPUP_MAX.1),
            )
        });
        Self {
            size: Cell::new(size),
            grip: Cell::new(TrayPopupGrip {
                up: true,
                left: true,
            }),
            drag: RefCell::new(None),
        }
    }
}

fn tray_popup_outer_rect(window: &slint::winit_030::winit::window::Window) -> Option<PhysicalRect> {
    let position = window.outer_position().ok()?;
    let size = window.outer_size();
    Some(PhysicalRect {
        x: position.x,
        y: position.y,
        width: size.width,
        height: size.height,
    })
}

fn scaled_size(size: (u32, u32), scale: f64) -> (u32, u32) {
    (
        (f64::from(size.0) * scale).round() as u32,
        (f64::from(size.1) * scale).round() as u32,
    )
}

/// Edge drags on the popup: its tray corner stays put, the size is clamped to its
/// limits and the work area, and the final size is remembered.
fn wire_tray_popup_resize(
    popup: &TrayPopup,
    geometry: Rc<TrayPopupGeometry>,
    preference_tracker: Arc<Mutex<PersistenceTracker>>,
) {
    let weak = popup.as_weak();
    let start_geometry = geometry.clone();
    popup.on_resize_start(move |handle| {
        let Some(popup) = weak.upgrade() else {
            return;
        };
        let drag = popup
            .window()
            .with_winit_window(|window| {
                Some(TrayResizeDrag {
                    start: tray_popup_outer_rect(window)?,
                    cursor: global_cursor_position(),
                    handle: TrayResizeHandle {
                        horizontal: handle.as_str() != "v",
                        vertical: handle.as_str() != "h",
                    },
                    monitors: monitor_rects(window),
                })
            })
            .flatten();
        *start_geometry.drag.borrow_mut() = drag;
    });

    let weak = popup.as_weak();
    let drag_geometry = geometry.clone();
    popup.on_resize_drag(move |dx, dy| {
        let Some(popup) = weak.upgrade() else {
            return;
        };
        let drag = drag_geometry.drag.borrow();
        let Some(drag) = drag.as_ref() else {
            return;
        };
        let grip = drag_geometry.grip.get();
        let _ = popup.window().with_winit_window(|window| {
            let scale = window.scale_factor();
            let current = tray_popup_outer_rect(window).unwrap_or(drag.start);
            // Absolute from the press where the desktop pointer is known; else the
            // offset inside the popup, which resets as the dragged edge follows it.
            let (base, delta) = match (drag.cursor, global_cursor_position()) {
                (Some(start), Some(now)) => (drag.start, (now.0 - start.0, now.1 - start.1)),
                _ => (
                    current,
                    (
                        (f64::from(dx) * scale).round() as i32,
                        (f64::from(dy) * scale).round() as i32,
                    ),
                ),
            };
            let limits = (
                scaled_size(TRAY_POPUP_MIN, scale),
                scaled_size(TRAY_POPUP_MAX, scale),
            );
            let next = resize_tray_popup(base, delta, drag.handle, grip, limits, &drag.monitors);
            if next != current {
                set_tray_popup_rect(window, next);
            }
        });
    });

    let weak = popup.as_weak();
    popup.on_resize_end(move || {
        geometry.drag.borrow_mut().take();
        let Some(popup) = weak.upgrade() else {
            return;
        };
        let scale = f64::from(popup.window().scale_factor());
        let size = popup.window().size();
        let logical = (
            ((f64::from(size.width) / scale).round() as u32)
                .clamp(TRAY_POPUP_MIN.0, TRAY_POPUP_MAX.0),
            ((f64::from(size.height) / scale).round() as u32)
                .clamp(TRAY_POPUP_MIN.1, TRAY_POPUP_MAX.1),
        );
        geometry.size.set(logical);
        preference_tracker
            .lock()
            .expect("preference tracker lock")
            .set_tray_popup_size(logical);
    });
}

fn toggle_tray_popup(
    slot: &TrayPopupSlot,
    main_weak: &slint::Weak<AppWindow>,
    model: &Arc<Mutex<DesktopViewModel>>,
    geometry: &Rc<TrayPopupGeometry>,
    anchor: PhysicalPosition,
) {
    let open = &slot.open;
    if open.get() {
        hide_tray_popup(&slot.weak(), open);
        return;
    }
    let Some(main) = main_weak.upgrade() else {
        return;
    };
    let (popup, created) = match slot.get_or_create() {
        Ok(popup) => popup,
        Err(error) => {
            eprintln!("tray popup unavailable: {error}");
            return;
        }
    };
    #[cfg(windows)]
    slot.last_open.set(std::time::Instant::now());
    apply_tray_popup_model(
        &popup,
        tray_menu_model(&model.lock().expect("view model lock")),
    );
    sync_tray_live_state(&popup, &main);
    // The main window has no native window while it is released in the tray, and a new
    // popup gets its own only on the next event-loop turn.
    let monitors = main
        .window()
        .with_winit_window(monitor_rects)
        .or_else(|| popup.window().with_winit_window(monitor_rects))
        .unwrap_or_default();
    // The remembered size, fitted to this monitor, anchored at the tray corner;
    // the edges away from that corner resize it.
    let grip = tray_popup_grip(anchor, &monitors);
    geometry.grip.set(grip);
    popup.set_grow_up(grip.up);
    popup.set_grow_left(grip.left);
    if created {
        preplace_new_tray_popup(&popup, anchor, geometry, &monitors);
    }
    let _ = popup.show();
    let placed = popup
        .window()
        .with_winit_window(|window| place_tray_popup_window(window, anchor, geometry, &monitors))
        .is_some();
    if !placed {
        let weak = popup.as_weak();
        let geometry = geometry.clone();
        let _ = slint::spawn_local(async move {
            let Some(popup) = weak.upgrade() else {
                return;
            };
            let Ok(window) = popup.window().winit_window().await else {
                return;
            };
            let monitors = monitor_rects(&window);
            let grip = tray_popup_grip(anchor, &monitors);
            geometry.grip.set(grip);
            popup.set_grow_up(grip.up);
            popup.set_grow_left(grip.left);
            place_tray_popup_window(&window, anchor, &geometry, &monitors);
        });
    }
    open.set(true);
}

/// A new popup's window is created on the next event-loop turn and shown at once: put it
/// where it will open (off-screen when no monitor is known yet) so no frame shows elsewhere.
fn preplace_new_tray_popup(
    popup: &TrayPopup,
    anchor: PhysicalPosition,
    geometry: &TrayPopupGeometry,
    monitors: &[MonitorRect],
) {
    let window = popup.window();
    let monitor = tray_popup_monitor(anchor, monitors);
    let scale_milli = monitor.map_or_else(
        || (window.scale_factor() * 1000.0).round() as u32,
        |monitor| monitor.scale_milli,
    );
    let size = tray_popup_size(geometry.size.get(), scale_milli, monitor);
    let position = place_tray_popup(anchor, size, monitors, None).unwrap_or(PhysicalPosition {
        x: -32_000,
        y: -32_000,
    });
    window.set_position(slint::PhysicalPosition::new(position.x, position.y));
    window.set_size(slint::PhysicalSize::new(size.0, size.1));
}

/// Native placement of the open popup: its size on this monitor, anchored at the tray
/// corner, rounded corners, above other windows, focused.
fn place_tray_popup_window(
    window: &slint::winit_030::winit::window::Window,
    anchor: PhysicalPosition,
    geometry: &TrayPopupGeometry,
    monitors: &[MonitorRect],
) {
    use slint::winit_030::winit::dpi::PhysicalSize as WinitPhysicalSize;
    use slint::winit_030::winit::window::WindowLevel;
    let monitor = tray_popup_monitor(anchor, monitors);
    let scale_milli = monitor.map_or_else(
        || (window.scale_factor() * 1000.0).round() as u32,
        |monitor| monitor.scale_milli,
    );
    let popup_size = tray_popup_size(geometry.size.get(), scale_milli, monitor);
    apply_tray_popup_native_rounding(window);
    window.set_window_level(WindowLevel::AlwaysOnTop);
    match place_tray_popup(anchor, popup_size, monitors, None) {
        Some(position) => set_tray_popup_rect(
            window,
            PhysicalRect {
                x: position.x,
                y: position.y,
                width: popup_size.0,
                height: popup_size.1,
            },
        ),
        None => {
            let _ = window.request_inner_size(WinitPhysicalSize::new(popup_size.0, popup_size.1));
        }
    }
    window.focus_window();
    window.request_redraw();
}

fn dispatch_tray_popup_command(
    popup: &slint::Weak<TrayPopup>,
    open: &Rc<Cell<bool>>,
    ui: &slint::Weak<AppWindow>,
    model: &Arc<Mutex<DesktopViewModel>>,
    command: TrayCommand,
    preference_writer: &Arc<Mutex<PreferenceWriter>>,
    preference_tracker: &Arc<Mutex<PersistenceTracker>>,
) {
    hide_tray_popup(popup, open);
    handle_tray_command(ui, model, command, preference_writer, preference_tracker);
}

/// Runs a popup action that keeps the popup open (route and group picks, the
/// connect switch), then refreshes what the popup shows.
fn dispatch_tray_popup_selection(
    popup: &slint::Weak<TrayPopup>,
    ui: &slint::Weak<AppWindow>,
    model: &Arc<Mutex<DesktopViewModel>>,
    command: TrayCommand,
    preference_writer: &Arc<Mutex<PreferenceWriter>>,
    preference_tracker: &Arc<Mutex<PersistenceTracker>>,
) {
    handle_tray_command(ui, model, command, preference_writer, preference_tracker);
    if let Some(popup) = popup.upgrade() {
        apply_tray_popup_model(
            &popup,
            tray_menu_model(&model.lock().expect("view model lock")),
        );
        if let Some(main) = ui.upgrade() {
            sync_tray_live_state(&popup, &main);
        }
    }
}

fn wire_tray_popup_actions(
    popup: &TrayPopup,
    ui: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    preference_writer: Arc<Mutex<PreferenceWriter>>,
    preference_tracker: Arc<Mutex<PersistenceTracker>>,
    open: Rc<Cell<bool>>,
) {
    let popup_weak = popup.as_weak();
    let dismiss_popup = popup_weak.clone();
    let dismiss_open = open.clone();
    popup.on_dismiss(move || hide_tray_popup(&dismiss_popup, &dismiss_open));

    let wire_command = |command: TrayCommand| {
        let popup = popup_weak.clone();
        let open = open.clone();
        let ui = ui.clone();
        let model = model.clone();
        let writer = preference_writer.clone();
        let tracker = preference_tracker.clone();
        move || {
            dispatch_tray_popup_command(
                &popup,
                &open,
                &ui,
                &model,
                command.clone(),
                &writer,
                &tracker,
            );
        }
    };
    // Opening the window and quitting close the popup; the switch keeps it open
    // so "Подключение…" and the result stay visible.
    popup.on_open_main(wire_command(TrayCommand::ShowWindow));
    // Without a profile the main window opens on the inline onboarding.
    popup.on_add_subscription(wire_command(TrayCommand::ShowWindow));
    popup.on_exit_app(wire_command(TrayCommand::Exit));

    let wire_in_place = |command_for: fn(String, String) -> TrayCommand| {
        let popup = popup_weak.clone();
        let ui = ui.clone();
        let model = model.clone();
        let writer = preference_writer.clone();
        let tracker = preference_tracker.clone();
        move |first: SharedString, second: SharedString| {
            dispatch_tray_popup_selection(
                &popup,
                &ui,
                &model,
                command_for(first.to_string(), second.to_string()),
                &writer,
                &tracker,
            );
        }
    };
    let toggle = wire_in_place(|_, _| TrayCommand::ToggleConnection);
    popup.on_toggle_connection(move || toggle("".into(), "".into()));
    let show_group = wire_in_place(|group_id, _| TrayCommand::SelectGroup { group_id });
    popup.on_toggle_group(move |group_id| show_group(group_id, "".into()));
    popup.on_select_node(wire_in_place(|group_id, node_id| TrayCommand::SelectNode {
        group_id,
        node_id,
    }));
    // Right click on a tray route re-measures it in place, like on the home list.
    let measure_ui = ui.clone();
    let measure_model = model.clone();
    let measure_popup = popup_weak.clone();
    popup.on_measure_node(move |_group_id, node_id| {
        measure_catalog_node(measure_ui.clone(), measure_model.clone(), node_id.as_str());
        refresh_tray_popup(&measure_popup, &measure_model);
    });
}

/// Rebuilds the open tray popup from the model at once (it otherwise follows on its timer).
fn refresh_tray_popup(popup: &slint::Weak<TrayPopup>, model: &Arc<Mutex<DesktopViewModel>>) {
    if let Some(popup) = popup.upgrade() {
        let next = tray_menu_model(&model.lock().expect("view model lock"));
        apply_tray_popup_model(&popup, next);
    }
}

fn wire_tray_popup_focus_loss(
    popup: &TrayPopup,
    open: Rc<Cell<bool>>,
    model: Arc<Mutex<DesktopViewModel>>,
    main: slint::Weak<AppWindow>,
) -> Timer {
    let timer = Timer::default();
    let weak = popup.as_weak();
    // A popup created on demand opens right away: give its first ticks the same grace.
    let grace_ticks = Rc::new(Cell::new(2_u8));
    let timer_grace = grace_ticks.clone();
    let mut initial_model_cache = TrayPopupModelCache::default();
    initial_model_cache.observe(&tray_menu_model(&model.lock().expect("view model lock")));
    let model_cache = Rc::new(RefCell::new(initial_model_cache));
    timer.start(TimerMode::Repeated, Duration::from_millis(120), move || {
        if !open.get() {
            timer_grace.set(2);
            return;
        }
        if timer_grace.get() > 0 {
            timer_grace.set(timer_grace.get() - 1);
            return;
        }
        let Some(popup) = weak.upgrade() else {
            return;
        };
        let next_model = tray_menu_model(&model.lock().expect("view model lock"));
        if model_cache.borrow_mut().observe(&next_model) {
            apply_tray_popup_model(&popup, next_model);
        }
        if let Some(main) = main.upgrade() {
            sync_tray_live_state(&popup, &main);
        }
        let focused = popup
            .window()
            .with_winit_window(|window| window.has_focus())
            .unwrap_or(false);
        if !focused {
            let _ = popup.hide();
            open.set(false);
        }
    });
    timer
}

fn tray_menu_model(model: &DesktopViewModel) -> TrayMenuModel {
    let presentation = model.presentation();
    let catalog = model.catalog_presentation();
    let tray_groups = model.tray_catalog_groups(12, 64);
    let status = if presentation.is_connected {
        tr!("В сети · {}", presentation.node)
    } else {
        presentation.headline.clone()
    };
    build_menu_model(TrayInput {
        status,
        connection_label: presentation.primary_label.into(),
        connection_enabled: (presentation.primary_enabled || presentation.primary_queueable)
            && presentation.primary_action != PrimaryAction::None,
        connected: presentation.is_connected,
        has_profile: presentation.has_profile,
        groups: tray_groups
            .into_iter()
            .map(|group| {
                let group_display = derive_catalog_display(&group.label, CatalogDisplayKind::Group);
                let selected_node =
                    derive_catalog_display(&group.selected_node_label, CatalogDisplayKind::Node);
                let nodes = group
                    .nodes
                    .into_iter()
                    .map(|node| {
                        let display = derive_catalog_display(&node.label, CatalogDisplayKind::Node);
                        TrayNode {
                            id: node.id,
                            label: display.label,
                            icon: display.icon,
                            icon_id: node.icon_id,
                            icon_pending: node.icon_pending,
                            selected: node.selected,
                            latency_text: node.latency_text,
                            latency_tone: node.latency_tone,
                            measuring: node.measuring,
                        }
                    })
                    .collect();
                TrayGroup {
                    selected: catalog.selected_group_id.as_deref() == Some(group.id.as_str()),
                    id: group.id,
                    label: group_display.label,
                    icon: group_display.icon,
                    icon_id: group.icon_id,
                    icon_pending: group.icon_pending,
                    selected_node_label: selected_node.label,
                    nodes,
                }
            })
            .collect(),
        route_selection_enabled: catalog.selection_enabled,
        dumb_mode: catalog.dumb_mode,
        icon_state: tray::tray_icon_state(&presentation, model.connection_uses_tun()),
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
            flush_preferences(&ui, model, preference_writer, preference_tracker);
            event_loop_guard::request_quit();
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
                tr!("Не удалось изменить автозапуск. Проверьте права пользователя.").into(),
            );
        }
    }
}

fn wire_import(
    ui: &AppWindow,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
    pending: Arc<Mutex<PendingSubscriptionImports>>,
) {
    let weak = ui.as_weak();
    ui.on_import_url(move |url| {
        let url = url.trim().to_owned();
        let (mutation, client, current) = {
            let mut model = model.lock().expect("view model lock");
            profile_preferences
                .lock()
                .expect("profile preference reconciler lock")
                .capture_current(&model);
            let mutation = model.begin_import(&url);
            let client = model.daemon_client();
            (mutation, client, snapshot(&model))
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
        }
        let Some(mutation) = mutation else {
            return false;
        };

        run_import(
            weak.clone(),
            model.clone(),
            client,
            mutation,
            url,
            profile_preferences.clone(),
            pending.clone(),
        );
        true
    });
}

/// The saved local proxy port, as Settings shows it ("" before the daemon chose one).
fn saved_local_proxy_address() -> String {
    local_proxy_address(
        data_root::user_data_root()
            .ok()
            .and_then(|root| multicore_core::local_proxy_port::read_local_proxy_port(&root)),
    )
}

/// «Локальный прокси: 127.0.0.1:PORT» in Settings and its copy action. The daemon saves the
/// port at start and changes it only when another app took it, which a connect reveals.
fn wire_local_proxy(ui: &AppWindow) {
    ui.set_local_proxy_address(saved_local_proxy_address().into());
    let weak = ui.as_weak();
    ui.on_copy_local_proxy(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let address = ui.get_local_proxy_address().to_string();
        if address.is_empty() || clipboard_text::write_text(ui.window(), &address).is_err() {
            return;
        }
        ui.set_local_proxy_copied(true);
        let reset = weak.clone();
        Timer::single_shot(Duration::from_secs(2), move || {
            if let Some(ui) = reset.upgrade() {
                ui.set_local_proxy_copied(false);
            }
        });
    });
}

fn wire_clipboard_import(ui: &AppWindow, model: Arc<Mutex<DesktopViewModel>>) {
    let weak = ui.as_weak();
    let paste_model = model.clone();
    ui.on_paste_subscription(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        match clipboard_text::read_subscription_text(ui.window()) {
            Ok(url) => {
                ui.set_subscription_draft(url.clone().into());
                ui.invoke_import_url(url.into());
            }
            Err(error) => {
                let current = {
                    let mut model = paste_model.lock().expect("view model lock");
                    model.set_import_error_message(error.user_message());
                    snapshot(&model)
                };
                apply_snapshot(&ui, &current);
            }
        }
    });

    let weak = ui.as_weak();
    ui.on_dismiss_first_run_import(move || {
        let current = {
            let mut model = model.lock().expect("view model lock");
            model.dismiss_import_error();
            snapshot(&model)
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
        }
    });
}

fn wire_subscription_refresh(
    ui: &AppWindow,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
) {
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
        run_subscription_refresh(
            weak.clone(),
            model.clone(),
            client,
            mutation,
            profile_preferences.clone(),
        );
    });
}

fn wire_automatic_subscription_refresh(
    ui: &AppWindow,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
) -> Timer {
    const RETRY_BACKOFF_SECS: u64 = 15 * 60;

    // Ticks every 2 s: the first ticks wait for the app to be ready and refresh once on
    // launch; after that the provider-interval check runs every 30 ticks (once a minute).
    const TICK_SECS: u64 = 2;
    const INTERVAL_CHECK_TICKS: u64 = 30;

    let timer = Timer::default();
    let weak = ui.as_weak();
    let retry_after = Rc::new(Cell::new(0_u64));
    let launch_refresh_pending = Rc::new(Cell::new(true));
    let ticks = Rc::new(Cell::new(0_u64));
    timer.start(
        TimerMode::Repeated,
        Duration::from_secs(TICK_SECS),
        move || {
            ticks.set(ticks.get() + 1);
            let launch = launch_refresh_pending.get();
            if !launch && !ticks.get().is_multiple_of(INTERVAL_CHECK_TICKS) {
                return;
            }
            let now_unix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let (mutation, client, current) = {
                let mut model = model.lock().expect("view model lock");
                let due = if launch {
                    model.launch_subscription_refresh_ready()
                } else {
                    now_unix >= retry_after.get()
                        && model.automatic_subscription_refresh_due_at(now_unix)
                };
                let mutation = due.then(|| model.begin_subscription_refresh()).flatten();
                (mutation, model.daemon_client(), snapshot(&model))
            };
            if let Some(ui) = weak.upgrade() {
                apply_snapshot(&ui, &current);
            }
            if let Some(mutation) = mutation {
                launch_refresh_pending.set(false);
                retry_after.set(now_unix.saturating_add(RETRY_BACKOFF_SECS));
                run_subscription_refresh(
                    weak.clone(),
                    model.clone(),
                    client,
                    mutation,
                    profile_preferences.clone(),
                );
            }
        },
    );
    timer
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DeleteFocusTarget {
    ManagementProfile(String),
    AddSubscription,
    DeleteControl(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DeleteFocusPlan {
    deleting_profile_id: String,
    successful_target: DeleteFocusTarget,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DeleteFocusState {
    AwaitingCompletion(DeleteFocusPlan),
    Immediate(DeleteFocusTarget),
}

impl DeleteFocusPlan {
    fn completion(&self, succeeded: bool) -> DeleteFocusTarget {
        if succeeded {
            self.successful_target.clone()
        } else {
            DeleteFocusTarget::DeleteControl(self.deleting_profile_id.clone())
        }
    }
}

fn delete_focus_plan(
    profile_ids: &[&str],
    deleting_profile_id: &str,
    accepted: bool,
) -> DeleteFocusState {
    if !accepted {
        return DeleteFocusState::Immediate(DeleteFocusTarget::DeleteControl(
            deleting_profile_id.to_owned(),
        ));
    }
    let successful_target = profile_ids
        .iter()
        .position(|profile_id| *profile_id == deleting_profile_id)
        .and_then(|deleting_index| {
            profile_ids
                .get(deleting_index + 1)
                .or_else(|| {
                    deleting_index
                        .checked_sub(1)
                        .and_then(|index| profile_ids.get(index))
                })
                .copied()
        })
        .map(|profile_id| DeleteFocusTarget::ManagementProfile(profile_id.to_owned()))
        .unwrap_or(DeleteFocusTarget::AddSubscription);
    DeleteFocusState::AwaitingCompletion(DeleteFocusPlan {
        deleting_profile_id: deleting_profile_id.to_owned(),
        successful_target,
    })
}

fn apply_delete_focus_completion(ui: &AppWindow, completion: DeleteFocusTarget) {
    match completion {
        DeleteFocusTarget::ManagementProfile(profile_id) => {
            ui.set_profile_edit_id("".into());
            ui.set_profile_edit_draft("".into());
            ui.set_profile_delete_confirm_id("".into());
            ui.set_profile_management_focus_id(profile_id.into());
            ui.set_profile_management_focus_serial(
                ui.get_profile_management_focus_serial().saturating_add(1),
            );
        }
        DeleteFocusTarget::AddSubscription => {
            ui.set_profile_edit_id("".into());
            ui.set_profile_edit_draft("".into());
            ui.set_profile_delete_confirm_id("".into());
            ui.set_profile_management_focus_id("".into());
            ui.set_profile_management_focus_serial(
                ui.get_profile_management_focus_serial().saturating_add(1),
            );
        }
        DeleteFocusTarget::DeleteControl(profile_id) => {
            ui.set_profile_delete_confirm_id("".into());
            ui.set_profile_delete_control_focus_id(profile_id.into());
            ui.set_profile_delete_control_focus_serial(
                ui.get_profile_delete_control_focus_serial()
                    .saturating_add(1),
            );
        }
    }
}

fn wire_profile_switcher(
    ui: &AppWindow,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
) {
    let weak = ui.as_weak();
    let open_model = model.clone();
    let open_preferences = profile_preferences.clone();
    ui.on_open_profile_menu(move || {
        let current = {
            let mut model = open_model.lock().expect("view model lock");
            model.open_panel(Panel::Subscription);
            snapshot(&model)
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
        }
        load_profile_list(
            weak.clone(),
            open_model.clone(),
            open_preferences.clone(),
            false,
        );
    });

    let weak = ui.as_weak();
    let retry_model = model.clone();
    let retry_preferences = profile_preferences.clone();
    ui.on_retry_profile_menu(move || {
        if weak.upgrade().is_some_and(|ui| {
            ui.get_active_panel() == "subscription" || ui.get_local_page() == "subscriptions"
        }) {
            load_profile_list(
                weak.clone(),
                retry_model.clone(),
                retry_preferences.clone(),
                false,
            );
        }
    });

    let weak = ui.as_weak();
    let close_model = model.clone();
    ui.on_close_profile_menu(move || {
        let current = {
            let mut model = close_model.lock().expect("view model lock");
            model.open_panel(Panel::None);
            snapshot(&model)
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
        }
    });

    let weak = ui.as_weak();
    let add_model = model.clone();
    ui.on_add_subscription(move || {
        let current = {
            let mut model = add_model.lock().expect("view model lock");
            model.open_panel(Panel::None);
            snapshot(&model)
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
            ui.set_local_page("subscriptions".into());
            ui.set_subscription_draft("".into());
            ui.set_replace_editor_open(true);
        }
    });

    let weak = ui.as_weak();
    let activate_model = model.clone();
    let activate_preferences = profile_preferences.clone();
    ui.on_activate_profile(move |profile_id| {
        let (token, client, current) = {
            let mut model = activate_model.lock().expect("view model lock");
            activate_preferences
                .lock()
                .expect("profile preference reconciler lock")
                .capture_current(&model);
            let token = model.begin_profile_activation(profile_id.as_str());
            (token, model.daemon_client(), snapshot(&model))
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
        }
        if let Some(token) = token {
            run_profile_activation(
                weak.clone(),
                activate_model.clone(),
                activate_preferences.clone(),
                client,
                token,
                profile_id.to_string(),
            );
        }
    });

    let weak = ui.as_weak();
    let refresh_model = model.clone();
    let refresh_preferences = profile_preferences.clone();
    ui.on_refresh_profile(move |profile_id| {
        let (token, client, current) = {
            let mut model = refresh_model.lock().expect("view model lock");
            let token = model.begin_profile_target_refresh(profile_id.as_str());
            (token, model.daemon_client(), snapshot(&model))
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
        }
        if let Some(token) = token {
            run_profile_target_refresh(
                weak.clone(),
                refresh_model.clone(),
                refresh_preferences.clone(),
                client,
                token,
                profile_id.to_string(),
            );
        }
    });

    let weak = ui.as_weak();
    let edit_model = model.clone();
    let edit_preferences = profile_preferences.clone();
    ui.on_replace_profile_source(move |profile_id, url| {
        let (token, client, current) = {
            let mut model = edit_model.lock().expect("view model lock");
            let token = model.begin_profile_source_edit(profile_id.as_str(), url.as_str());
            (token, model.daemon_client(), snapshot(&model))
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
        }
        if let Some(token) = token {
            run_profile_source_edit(
                weak.clone(),
                edit_model.clone(),
                edit_preferences.clone(),
                client,
                token,
                profile_id.to_string(),
                url.to_string(),
            );
        }
    });

    let weak = ui.as_weak();
    let delete_model = model;
    ui.on_delete_profile(move |profile_id| {
        let (token, client, current, focus_state) = {
            let mut model = delete_model.lock().expect("view model lock");
            let profiles = model.profile_presentation().profiles;
            let profile_ids = profiles
                .iter()
                .map(|profile| profile.id.as_str())
                .collect::<Vec<_>>();
            let token = model.begin_profile_delete(profile_id.as_str());
            let focus_state = delete_focus_plan(&profile_ids, profile_id.as_str(), token.is_some());
            (token, model.daemon_client(), snapshot(&model), focus_state)
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
            match &focus_state {
                DeleteFocusState::AwaitingCompletion(_) => {
                    ui.set_profile_delete_confirm_id("".into());
                }
                DeleteFocusState::Immediate(completion) => {
                    apply_delete_focus_completion(&ui, completion.clone());
                }
            }
        }
        if let (Some(token), DeleteFocusState::AwaitingCompletion(focus_plan)) =
            (token, focus_state)
        {
            run_profile_delete(
                weak.clone(),
                delete_model.clone(),
                profile_preferences.clone(),
                client,
                token,
                profile_id.to_string(),
                focus_plan,
            );
        }
    });
}

fn wire_primary_action(
    ui: &AppWindow,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
    startup_connect_pending: Rc<Cell<bool>>,
) {
    let weak = ui.as_weak();
    let queued_press = Rc::new(Timer::default());
    ui.on_primary_action(move || {
        startup_connect_pending.set(false);
        let (action, client, queued, current, waiting) = {
            let mut model = model.lock().expect("view model lock");
            // Re-scan right at the start of the attempt: warn before a
            // conflicting app causes a failed Connect, or explain one that
            // just happened.
            model.set_conflicting_apps(conflicting_apps::scan_running());
            let action = model.begin_primary_action();
            // Held by background work (right after launch: the status check, the launch
            // subscription refresh, restored routes): the press waits for that work instead
            // of being lost.
            let waiting = action.is_none() && model.queue_primary_action(Instant::now());
            let client = model.daemon_client();
            let queued = model.queued_node_selections();
            (action, client, queued, snapshot(&model), waiting)
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
        }
        if waiting {
            run_queued_primary_action(&queued_press, &weak, &model, &profile_preferences);
        }
        dispatch_primary_action(&weak, &model, &profile_preferences, action, client, queued);
    });
}

/// While a press of the switch waits for background work, checks every
/// [`QUEUED_PRIMARY_POLL`] whether the work ended and then starts it like a free press.
/// The timer runs only while a press waits.
fn run_queued_primary_action(
    timer: &Rc<Timer>,
    weak: &slint::Weak<AppWindow>,
    model: &Arc<Mutex<DesktopViewModel>>,
    profile_preferences: &Arc<Mutex<ProfilePreferenceReconciler>>,
) {
    if timer.running() {
        return;
    }
    let this = Rc::downgrade(timer);
    let (weak, model, profile_preferences) =
        (weak.clone(), model.clone(), profile_preferences.clone());
    timer.start(TimerMode::Repeated, QUEUED_PRIMARY_POLL, move || {
        let (action, client, queued, current) = {
            let mut model = model.lock().expect("view model lock");
            let action = model.begin_queued_primary_action(Instant::now());
            if action.is_none() && model.has_queued_primary_action() {
                return;
            }
            if action.is_some() {
                model.set_conflicting_apps(conflicting_apps::scan_running());
            }
            let client = model.daemon_client();
            let queued = model.queued_node_selections();
            (action, client, queued, snapshot(&model))
        };
        // Started, or dropped (it waited too long, or the work ended in another state).
        if let Some(timer) = this.upgrade() {
            timer.stop();
        }
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
        }
        dispatch_primary_action(&weak, &model, &profile_preferences, action, client, queued);
    });
}

/// Tick of [`run_queued_primary_action`]: short enough that a waiting press starts right
/// after the work it waited for.
const QUEUED_PRIMARY_POLL: Duration = Duration::from_millis(100);

fn dispatch_primary_action(
    weak: &slint::Weak<AppWindow>,
    model: &Arc<Mutex<DesktopViewModel>>,
    profile_preferences: &Arc<Mutex<ProfilePreferenceReconciler>>,
    action: Option<(PrimaryAction, MutationToken)>,
    client: Arc<dyn DaemonClient>,
    queued: Vec<QueuedNodeSelection>,
) {
    match action {
        Some((PrimaryAction::Connect, mutation)) => {
            let tun = weak.upgrade().is_none_or(|ui| connect_uses_tun(&ui));
            model
                .lock()
                .expect("view model lock")
                .set_connection_uses_tun(tun);
            run_connect(weak.clone(), model.clone(), client, mutation, queued, tun);
        }
        Some((PrimaryAction::Disconnect | PrimaryAction::Cleanup, mutation)) => {
            run_disconnect(weak.clone(), model.clone(), client, mutation);
        }
        Some((PrimaryAction::Retry, mutation)) => {
            run_status(
                weak.clone(),
                model.clone(),
                client,
                mutation,
                profile_preferences.clone(),
            );
        }
        Some((PrimaryAction::None, _)) | None => {}
    }
}

/// "Установить службу": download and verify the installer in the background, then quit.
/// `main` starts the installer only after the event loop, tray and owned daemon are gone,
/// because its migration step uninstalls this per-user copy.
fn wire_service_install(ui: &AppWindow) {
    let weak = ui.as_weak();
    ui.on_install_service(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_service_install_busy() {
            return;
        }
        ui.set_service_install_busy(true);
        ui.set_service_install_error("".into());
        let weak = weak.clone();
        thread::spawn(move || {
            let result = service_install::download_installer();
            let _ = slint::invoke_from_event_loop(move || match result {
                Ok(installer) => {
                    service_install::launch_after_exit(installer);
                    let _keep_window_alive = weak;
                    event_loop_guard::request_quit();
                }
                Err(error) => {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_service_install_busy(false);
                        ui.set_service_install_error(error.into());
                    }
                }
            });
        });
    });
}

/// Draws a troubleshooting run's cards, fix offers and fix results.
fn present_troubleshoot_run(ui: &AppWindow, run: &troubleshoot::TroubleshootRun) {
    let card = |finding: &multicore_core::troubleshoot::Finding, badge: &str| UiTroubleshootCard {
        tone: troubleshoot::tone_name(finding.tone).into(),
        title: finding.localized_title().into(),
        detail: finding.localized_detail().into(),
        badge: badge.into(),
    };
    let summary = troubleshoot::summary(run.findings());
    ui.set_troubleshoot_summary_tone(troubleshoot::tone_name(summary.tone).into());
    ui.set_troubleshoot_summary_title(summary.title.into());
    ui.set_troubleshoot_summary_detail(summary.detail.into());
    let (status_text, status_tone) = troubleshoot::status_pill(run.findings());
    ui.set_troubleshoot_status_text(status_text.into());
    ui.set_troubleshoot_status_tone(troubleshoot::tone_name(status_tone).into());
    ui.set_troubleshoot_cards(ModelRc::new(VecModel::from(
        run.findings()
            .iter()
            .map(|finding| card(finding, troubleshoot::finding_badge(finding.tone)))
            .collect::<Vec<_>>(),
    )));
    ui.set_troubleshoot_fix_results(ModelRc::new(VecModel::from(
        run.fix_results()
            .iter()
            .map(|finding| card(finding, ""))
            .collect::<Vec<_>>(),
    )));
    ui.set_troubleshoot_fixes(ModelRc::new(VecModel::from(
        run.offers()
            .iter()
            .map(|(offer, checked)| UiTroubleshootFix {
                id: offer.id.as_str().into(),
                title: offer.localized_title().into(),
                detail: offer.localized_detail().into(),
                needs_admin: offer.needs_admin,
                needs_reboot: offer.needs_reboot,
                checked: *checked,
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_troubleshoot_checked_count(run.checked_count() as i32);
    ui.set_troubleshoot_reboot(run.reboot_recommended());
}

fn present_troubleshoot_steps(
    ui: &AppWindow,
    steps: &[multicore_core::troubleshoot::CheckStep],
    active: usize,
) {
    ui.set_troubleshoot_step_index(active as i32);
    ui.set_troubleshoot_steps(ModelRc::new(VecModel::from(
        steps
            .iter()
            .enumerate()
            .map(|(index, step)| UiTroubleshootStep {
                // A finished step drops its ellipsis.
                // The step names come from `multicore_core` in Russian, as catalog msgids.
                label: if index < active {
                    multicore_i18n::translate(step.label())
                        .trim_end_matches('…')
                        .into()
                } else {
                    multicore_i18n::translate(step.label()).into()
                },
                state: match index.cmp(&active) {
                    std::cmp::Ordering::Less => "done",
                    std::cmp::Ordering::Equal => "active",
                    std::cmp::Ordering::Greater => "pending",
                }
                .into(),
            })
            .collect::<Vec<_>>(),
    )));
}

/// «Устранить неполадки»: the checks run one by one through the daemon (the page names each
/// step), then only the fixes the user ticked, or one of the two separate repair actions.
///
/// The last run stays in memory for the session: reopening the page shows its results (with
/// «Проверить заново») instead of the start screen. It is not saved across restarts: findings
/// name processes, ports and paths that are stale by then, and a new check takes seconds.
fn wire_troubleshoot(ui: &AppWindow, model: Arc<Mutex<DesktopViewModel>>) -> Timer {
    use multicore_core::troubleshoot::{CheckStep, PrivilegedFix};
    ui.set_troubleshoot_windows(cfg!(windows));
    let run = Arc::new(Mutex::new(troubleshoot::TroubleshootRun::default()));
    // When the last check finished, for «последняя проверка N мин назад».
    let checked_at: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let ago_timer = Timer::default();
    {
        let (weak, checked_at) = (ui.as_weak(), checked_at.clone());
        ago_timer.start(TimerMode::Repeated, Duration::from_secs(20), move || {
            let at = *checked_at.lock().expect("troubleshoot clock lock");
            if let (Some(ui), Some(at)) = (weak.upgrade(), at) {
                ui.set_troubleshoot_checked_ago(troubleshoot::checked_ago(at.elapsed()).into());
            }
        });
    }

    // A language switch redraws the steps, the finished run and «N мин назад».
    {
        let (weak, run, checked_at) = (ui.as_weak(), run.clone(), checked_at.clone());
        on_language_refresh(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let phase = ui.get_troubleshoot_phase();
            if phase != "start" {
                let steps = CheckStep::for_platform(cfg!(windows));
                let active = usize::try_from(ui.get_troubleshoot_step_index()).unwrap_or(0);
                present_troubleshoot_steps(&ui, &steps, active.min(steps.len()));
            }
            if matches!(phase.as_str(), "results" | "fixing") {
                present_troubleshoot_run(&ui, &run.lock().expect("troubleshoot run lock"));
            }
            if let Some(at) = *checked_at.lock().expect("troubleshoot clock lock") {
                ui.set_troubleshoot_checked_ago(troubleshoot::checked_ago(at.elapsed()).into());
            }
        });
    }
    let (weak, start_run, start_model, start_checked_at) =
        (ui.as_weak(), run.clone(), model.clone(), checked_at.clone());
    ui.on_troubleshoot_start(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if matches!(ui.get_troubleshoot_phase().as_str(), "running" | "fixing")
            || !ui.get_troubleshoot_repair_busy().is_empty()
        {
            return;
        }
        *start_run.lock().expect("troubleshoot run lock") =
            troubleshoot::TroubleshootRun::default();
        let steps = CheckStep::for_platform(cfg!(windows));
        present_troubleshoot_steps(&ui, &steps, 0);
        ui.set_troubleshoot_error("".into());
        ui.set_troubleshoot_repair_confirm("".into());
        ui.set_troubleshoot_phase("running".into());
        let client = start_model.lock().expect("view model lock").daemon_client();
        let (weak, run, checked_at) = (weak.clone(), start_run.clone(), start_checked_at.clone());
        thread::spawn(move || {
            for (index, step) in steps.iter().enumerate() {
                let (progress_weak, progress_steps) = (weak.clone(), steps.clone());
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = progress_weak.upgrade() {
                        present_troubleshoot_steps(&ui, &progress_steps, index);
                    }
                });
                match client.troubleshoot_check(*step) {
                    Ok(report) => run.lock().expect("troubleshoot run lock").add_step(report),
                    Err(error) => {
                        let weak = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                ui.set_troubleshoot_error(error.to_string().into());
                                ui.set_troubleshoot_phase("start".into());
                            }
                        });
                        return;
                    }
                }
            }
            let finished_at = Instant::now();
            *checked_at.lock().expect("troubleshoot clock lock") = Some(finished_at);
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    present_troubleshoot_steps(&ui, &steps, steps.len());
                    present_troubleshoot_run(&ui, &run.lock().expect("troubleshoot run lock"));
                    ui.set_troubleshoot_checked_ago(
                        troubleshoot::checked_ago(finished_at.elapsed()).into(),
                    );
                    ui.set_troubleshoot_phase("results".into());
                }
            });
        });
    });

    let (weak, toggle_run) = (ui.as_weak(), run.clone());
    ui.on_troubleshoot_toggle_fix(move |index| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let mut run = toggle_run.lock().expect("troubleshoot run lock");
        run.toggle(usize::try_from(index).unwrap_or(usize::MAX));
        present_troubleshoot_run(&ui, &run);
    });

    // The confirmed fixes, or one separate action ("wintun" / "service").
    let run_fixes = {
        let (weak, run, model) = (ui.as_weak(), run.clone(), model.clone());
        move |ids: Vec<String>, repair: &'static str| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if ids.is_empty() {
                return;
            }
            ui.set_troubleshoot_error("".into());
            if repair.is_empty() {
                ui.set_troubleshoot_phase("fixing".into());
            } else {
                ui.set_troubleshoot_repair_busy(repair.into());
            }
            let client = model.lock().expect("view model lock").daemon_client();
            let (weak, run) = (weak.clone(), run.clone());
            thread::spawn(move || {
                let result = client.troubleshoot_fix(&ids);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else {
                        return;
                    };
                    let mut run = run.lock().expect("troubleshoot run lock");
                    match result {
                        Ok(report) => run.apply_report(&ids, report),
                        Err(error) => ui.set_troubleshoot_error(error.to_string().into()),
                    }
                    present_troubleshoot_run(&ui, &run);
                    if repair.is_empty() {
                        ui.set_troubleshoot_phase("results".into());
                    } else {
                        ui.set_troubleshoot_repair_busy("".into());
                    }
                });
            });
        }
    };
    let (apply_fixes, apply_run) = (run_fixes.clone(), run.clone());
    ui.on_troubleshoot_apply(move || {
        let ids = apply_run
            .lock()
            .expect("troubleshoot run lock")
            .checked_ids();
        apply_fixes(ids, "");
    });
    ui.on_troubleshoot_repair(move |kind| {
        let (fix, repair) = match kind.as_str() {
            "wintun" => (PrivilegedFix::ReinstallWintun, "wintun"),
            "service" => (PrivilegedFix::ReinstallService, "service"),
            _ => return,
        };
        run_fixes(vec![fix.token()], repair);
    });

    let weak = ui.as_weak();
    ui.on_troubleshoot_retry_connect(move || {
        let current = {
            let mut model = model.lock().expect("view model lock");
            model.prepare_retry_connect();
            snapshot(&model)
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
            ui.invoke_primary_action();
        }
    });
    ago_timer
}

/// "Удалить все данные" (second click): start the resetting process, then quit. Leaving
/// `run_desktop` drops the bootstrap (disconnect, stop and reap the owned daemon) and then the
/// single-instance guard, which is what the resetting process waits for.
fn wire_data_reset(ui: &AppWindow) {
    let weak = ui.as_weak();
    ui.on_reset_all_data(move || match data_reset::schedule_reset_and_relaunch() {
        Ok(()) => {
            event_loop_guard::request_quit();
        }
        Err(error) => {
            eprintln!("data reset could not be started: {error}");
            if let Some(ui) = weak.upgrade() {
                ui.set_reset_data_armed(false);
            }
        }
    });
}

/// Opens the local Mihomo dashboard in the system's default browser. The button that triggers
/// this is only enabled while `dashboard_available` (a live, authenticated controller session)
/// is true, but the daemon re-checks that itself: a race between disconnect and a stale click
/// simply surfaces as a "not connected" error here, discarded like any other transient failure of
/// an external-browser launch. The URL contains this session's Mihomo controller secret and must
/// never be logged, so it is deliberately not attached to any error or diagnostic path.
fn wire_dashboard_action(ui: &AppWindow, model: Arc<Mutex<DesktopViewModel>>) {
    ui.on_open_dashboard(move || {
        let client = model.lock().expect("view model lock").daemon_client();
        thread::spawn(move || {
            if let Ok(url) = client.dashboard_url() {
                let _ = webbrowser::open(&url);
            }
        });
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
                // The filter chips' ids (Slint compares them); never shown as text.
                ui.set_event_filter("Все".into()); // i18n-ignore: filter id
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
    // "Повторить" on the failed-catalog card runs the same load as opening routes.
    let retry_weak = ui.as_weak();
    let retry_model = model.clone();
    ui.on_retry_catalog(move || {
        load_catalog(retry_weak.clone(), retry_model.clone());
    });

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
    let measure_weak = ui.as_weak();
    let measure_model = model.clone();
    ui.on_measure_catalog_node(move |id| {
        measure_catalog_node(measure_weak.clone(), measure_model.clone(), id.as_str());
    });
    let weak = ui.as_weak();
    ui.on_select_catalog_node(move |id| {
        let (request, current, sweep_running) = {
            let mut model = model.lock().expect("view model lock");
            let request = model.begin_node_selection(id.as_str());
            (request, snapshot(&model), model.latency_sweep_running())
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
        }
        // The route just picked is measured at once rather than waiting its turn in the sweep.
        if sweep_running {
            measure_catalog_node(weak.clone(), model.clone(), id.as_str());
        }
        let Some(request) = request else {
            return;
        };
        run_node_selection(weak.clone(), model.clone(), request);
    });
}

/// Route search on home filters every group's nodes locally: `/v1/catalog` already
/// carries them all, so typing never waits on the daemon.
fn wire_route_search(ui: &AppWindow, model: Arc<Mutex<DesktopViewModel>>) {
    let weak = ui.as_weak();
    let search_model = model.clone();
    ui.on_route_search_changed(move |query| {
        let catalog = search_model
            .lock()
            .expect("view model lock")
            .catalog_presentation();
        if let Some(ui) = weak.upgrade() {
            apply_route_search(&ui, &catalog, query.as_str());
        }
    });
    let weak = ui.as_weak();
    ui.on_choose_route_search_hit(move |group_id, node_id| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        // Same path as the tray: switch the viewed group first, then pick the node there.
        let selected_group = model
            .lock()
            .expect("view model lock")
            .catalog_presentation()
            .selected_group_id;
        if selected_group.as_deref() != Some(group_id.as_str()) {
            ui.invoke_select_catalog_group(group_id);
        }
        ui.invoke_select_catalog_node(node_id);
    });
}

fn apply_route_search(ui: &AppWindow, catalog: &CatalogPresentation, query: &str) {
    ui.set_route_search_loading(catalog.loading && catalog.search_groups.is_empty());
    let scale = ui.window().scale_factor();
    let hits = search_routes(&catalog.search_groups, query)
        .into_iter()
        .map(|hit| UiRouteSearchHit {
            icon_image: catalog_icon_image(
                hit.icon_id.as_deref(),
                &hit.icon,
                ROUTE_ROW_ICON_PX,
                scale,
            ),
            icon: hit.icon.into(),
            icon_loading: hit.icon_pending,
            group_id: hit.group_id.into(),
            group_label: hit.group_label.into(),
            node_id: hit.node_id.into(),
            prefix: hit.prefix.into(),
            matched: hit.matched.into(),
            suffix: hit.suffix.into(),
            selected: hit.selected,
            latency_text: hit.latency_text.into(),
            latency_tone: hit.latency_tone.into(),
        })
        .collect::<Vec<_>>();
    ui.set_route_search_hits(ModelRc::new(VecModel::from(hits)));
}

/// The speedtest button: checks latency now, bypassing the 60 s timer.
fn wire_manual_latency(ui: &AppWindow, model: Arc<Mutex<DesktopViewModel>>) {
    let weak = ui.as_weak();
    ui.on_check_latency(move || {
        let (request, current) = {
            let mut model = model.lock().expect("view model lock");
            let request = model.begin_forced_latency_request();
            (request, snapshot(&model))
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
        }
        if let Some(request) = request {
            run_latency_check(weak.clone(), model.clone(), request);
        }
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

/// Feeds the home toggle's exit line with session uptime and live speed. Polls `/v1/session`
/// about once a second, but only while connected and the window is on screen; any other tick
/// returns at once without touching the daemon.
fn wire_session_stats(ui: &AppWindow, model: Arc<Mutex<DesktopViewModel>>) -> Timer {
    let timer = Timer::default();
    let weak = ui.as_weak();
    let traffic = Arc::new(Mutex::new(SessionTraffic::default()));
    let in_flight = Arc::new(AtomicBool::new(false));
    timer.start(TimerMode::Repeated, Duration::from_secs(1), move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let (connected, cores_ready) = {
            let model = model.lock().expect("view model lock");
            (model.presentation().is_connected, model.cores_ready())
        };
        if !connected && !cores_ready {
            traffic.lock().expect("session traffic lock").reset();
            apply_session_line(&ui, &SessionLine::default());
            return;
        }
        if !ui.window().is_visible() || ui.window().is_minimized() || !ui.get_shell_active() {
            return;
        }
        if in_flight.swap(true, Ordering::AcqRel) {
            return;
        }
        let client = model.lock().expect("view model lock").daemon_client();
        let weak = weak.clone();
        let model = model.clone();
        let traffic = traffic.clone();
        let in_flight = in_flight.clone();
        thread::spawn(move || {
            let line = match client.session() {
                Ok(session) => traffic.lock().expect("session traffic lock").sample(
                    &session,
                    Instant::now(),
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_or(0, |elapsed| {
                            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
                        }),
                ),
                Err(_) => {
                    traffic.lock().expect("session traffic lock").reset();
                    SessionLine::default()
                }
            };
            in_flight.store(false, Ordering::Release);
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else {
                    return;
                };
                // The connection may have ended while the request was in flight. Before
                // connecting (warm service cores) only the exit is meaningful.
                let (connected, cores_ready) = {
                    let model = model.lock().expect("view model lock");
                    (model.presentation().is_connected, model.cores_ready())
                };
                let line = line.for_state(connected, cores_ready);
                apply_session_line(&ui, &line);
            });
        });
    });
    timer
}

/// While the service warms the cores after start-up (or after a subscription change), polls
/// `/v1/status` so a waiting latency check starts as soon as they are up: every half second
/// for the first seconds of a warm-up (a switch warms the new subscription's cores in a second
/// or two, and the routes wait for them), every two seconds after that.
fn wire_cores_ready_watch(ui: &AppWindow, model: Arc<Mutex<DesktopViewModel>>) -> Timer {
    /// Fast polls per warm-up: 20 x 500 ms.
    const FAST_POLLS: u32 = 20;
    let timer = Timer::default();
    let weak = ui.as_weak();
    let in_flight = Arc::new(AtomicBool::new(false));
    let ticks = Cell::new(0_u32);
    let fast_polls = Cell::new(FAST_POLLS);
    let warming = RefCell::new(None::<Option<String>>);
    timer.start(TimerMode::Repeated, CORES_READY_POLL, move || {
        let (watching, ready, profile) = {
            let model = model.lock().expect("view model lock");
            (
                model.service_mode()
                    && !model.presentation().is_connected
                    && model.presentation().has_profile,
                model.cores_ready(),
                model.active_profile_id().map(str::to_owned),
            )
        };
        ticks.set(ticks.get().wrapping_add(1));
        // A new warm-up (the cores went down, or another subscription is now active) gets
        // its own window of fast polls.
        let warm_up = (watching && !ready).then_some(profile);
        if warm_up.is_some() && *warming.borrow() != warm_up {
            fast_polls.set(FAST_POLLS);
        }
        *warming.borrow_mut() = warm_up;
        // Once ready, keep checking every 10 s so a later loss of the cores shows up
        // before latency checks start failing.
        let due = if ready {
            ticks.get().is_multiple_of(20)
        } else if fast_polls.get() > 0 {
            true
        } else {
            ticks.get().is_multiple_of(4)
        };
        if !watching || !due || in_flight.swap(true, Ordering::AcqRel) {
            return;
        }
        if !ready {
            fast_polls.set(fast_polls.get().saturating_sub(1));
        }
        let client = model.lock().expect("view model lock").daemon_client();
        let (weak, model, in_flight) = (weak.clone(), model.clone(), in_flight.clone());
        thread::spawn(move || {
            let result = client.status();
            let release = model
                .lock()
                .expect("view model lock")
                .note_cores_ready_status(result);
            in_flight.store(false, Ordering::Release);
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    apply_latest_snapshot(&ui, &model);
                }
                if release {
                    begin_latency_check(weak, model);
                }
            });
        });
    });
    timer
}

/// Tick of [`wire_cores_ready_watch`].
const CORES_READY_POLL: Duration = Duration::from_millis(500);

/// Follows the daemon while connected (and while it reconnects by itself after the service
/// lost the connection): the service can drop a connection without any request of the
/// desktop's, and nothing else asks the daemon then. Runs whether or not the window is on
/// screen, since the tray shows the connection too. A local `/v1/status` every two seconds.
fn wire_connection_watch(ui: &AppWindow, model: Arc<Mutex<DesktopViewModel>>) -> Timer {
    let timer = Timer::default();
    let weak = ui.as_weak();
    let in_flight = Arc::new(AtomicBool::new(false));
    timer.start(TimerMode::Repeated, CONNECTION_WATCH_POLL, move || {
        let client = {
            let model = model.lock().expect("view model lock");
            if !model.watches_connection(Instant::now()) {
                return;
            }
            model.daemon_client()
        };
        if in_flight.swap(true, Ordering::AcqRel) {
            return;
        }
        let (weak, model, in_flight) = (weak.clone(), model.clone(), in_flight.clone());
        thread::spawn(move || {
            let result = client.status();
            let changed = model
                .lock()
                .expect("view model lock")
                .note_connection_status(result, Instant::now());
            in_flight.store(false, Ordering::Release);
            if changed {
                let current = snapshot(&model.lock().expect("view model lock"));
                // The cores changed under the catalog: reload it (latency checks need it).
                apply_snapshot_from_worker(weak, current, model, true, false);
            }
        });
    });
    timer
}

/// Tick of [`wire_connection_watch`].
const CONNECTION_WATCH_POLL: Duration = Duration::from_secs(2);

fn apply_session_line(ui: &AppWindow, line: &SessionLine) {
    if ui.get_session_uptime() != line.uptime.as_str() {
        ui.set_session_uptime(line.uptime.as_str().into());
    }
    if ui.get_traffic_down() != line.down.as_str() {
        ui.set_traffic_down(line.down.as_str().into());
    }
    if ui.get_traffic_up() != line.up.as_str() {
        ui.set_traffic_up(line.up.as_str().into());
    }
    let (icon, name) =
        exit_now_display(&line.exit_now, ui.get_node_name().as_str()).unwrap_or_default();
    if ui.get_exit_now_name() != name.as_str() {
        ui.set_exit_now_icon_image(emoji_images::image_for(&icon));
        ui.set_exit_now_icon(icon.as_str().into());
        ui.set_exit_now_name(name.as_str().into());
    }
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

/// Periodically re-scans for other Mihomo/Xray-based VPN client apps known
/// to conflict with MultiCore's TUN adapter, WinTun driver session, and
/// local ports, so the banner can warn *before* a Connect attempt fails
/// because of one. The scan is cheap and local (see `conflicting_apps.rs`);
/// it only runs while the window is visible and only while not already
/// connected, since a working tunnel means there is nothing to warn about.
fn wire_conflicting_apps_scan(ui: &AppWindow, model: Arc<Mutex<DesktopViewModel>>) -> Timer {
    let timer = Timer::default();
    let weak = ui.as_weak();
    timer.start(TimerMode::Repeated, Duration::from_secs(7), move || {
        if !weak.upgrade().is_some_and(|ui| ui.window().is_visible()) {
            return;
        }
        if model
            .lock()
            .expect("view model lock")
            .presentation()
            .is_connected
        {
            return;
        }
        let current = {
            let mut model = model.lock().expect("view model lock");
            model.set_conflicting_apps(conflicting_apps::scan_running());
            snapshot(&model)
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
        }
    });
    timer
}

fn wire_conflicting_apps_banner(ui: &AppWindow, model: Arc<Mutex<DesktopViewModel>>) {
    let weak = ui.as_weak();
    let dismiss_model = model.clone();
    ui.on_dismiss_conflicting_apps_banner(move || {
        let current = {
            let mut model = dismiss_model.lock().expect("view model lock");
            model.dismiss_conflicting_apps_banner();
            snapshot(&model)
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
        }
    });

    let weak = ui.as_weak();
    ui.on_close_conflicting_apps(move || {
        let apps = model.lock().expect("view model lock").conflicting_apps();
        if apps.is_empty() {
            return;
        }
        if let Some(ui) = weak.upgrade() {
            ui.set_conflicting_apps_closing(true);
        }
        let model = model.clone();
        let weak = weak.clone();
        thread::spawn(move || {
            // User-privilege-only, best-effort: never elevated. A process
            // this user's own token cannot terminate (most commonly: the
            // app's real engine runs as a Windows service in Session 0,
            // separate from its GUI) is simply not counted -
            // record_close_attempt below turns that into a specific note
            // instead of the button silently appearing to do nothing.
            for app in &apps {
                conflicting_apps::close_running(app);
            }
            let current = {
                let mut model = model.lock().expect("view model lock");
                model.set_conflicting_apps(conflicting_apps::scan_running());
                model.record_close_attempt(&apps);
                snapshot(&model)
            };
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_conflicting_apps_closing(false);
                    apply_snapshot(&ui, &current);
                }
            });
        });
    });
}

fn wire_event_filter(ui: &AppWindow, model: Arc<Mutex<DesktopViewModel>>) {
    let weak = ui.as_weak();
    ui.on_filter_events(move |filter| {
        let filter = filter.to_string();
        if let Some(ui) = weak.upgrade() {
            ui.set_event_filter(filter.clone().into());
        }
        let level = match filter.as_str() {
            // Chip ids from the Slint filter row, not shown as text.
            "Ошибки" => Some("error".to_owned()), // i18n-ignore: filter id
            "Система" => Some("system".to_owned()), // i18n-ignore: filter id
            "Ядра" => Some("cores".to_owned()),   // i18n-ignore: filter id
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

/// `diagnostics.txt` of a saved archive: every known event, newest first, plus the low-level
/// log of failed daemon calls.
fn diagnostics_export(model: &Arc<Mutex<DesktopViewModel>>) -> String {
    let mut text = model
        .lock()
        .expect("view model lock")
        .diagnostics_export_text();
    // Failures in the desktop-to-local-daemon HTTP call itself (couldn't
    // connect, response didn't parse, ...) never produce a daemon-side
    // event - the daemon has no way to know its own response failed to
    // reach or decode here. Fold in the raw low-level log so this export
    // still carries real diagnostic signal for exactly that case.
    if let Some(low_level) = daemon::read_low_level_log() {
        text.push_str("\n\n--- low-level ---\n");
        text.push_str(&low_level);
    }
    text
}

/// «Сохранить лог…» opens the «Сохранить лог» popup: which connections go into the archive
/// (see `session_logs`), «Последнее подключение» preselected; «Сохранить» continues with
/// `save-diagnostics(range)`.
fn wire_log_range(ui: &AppWindow) {
    let weak = ui.as_weak();
    ui.on_open_log_range(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let root = data_root::user_data_root().ok();
        let hints = session_logs::range_hints(
            root.as_deref(),
            multicore_core::core_sessions::unix_ms_now(),
        );
        ui.set_log_range_latest_hint(hints.latest.into());
        ui.set_log_range_week_hint(hints.week.into());
        ui.set_log_range_all_hint(hints.all.into());
        ui.set_log_range(session_logs::LogRange::Latest.key().into());
        ui.set_log_range_open(true);
    });
}

/// «Сохранить» in the «Сохранить лог» popup: asks where to save, then writes the diagnostics
/// archive (see `log_bundle`) with the connections of the chosen range. The label reports
/// «Сохраняем…», then «Сохранено ✓» or «Не сохранено».
fn wire_save_diagnostics(ui: &AppWindow, model: Arc<Mutex<DesktopViewModel>>) {
    static IN_PROGRESS: AtomicBool = AtomicBool::new(false);
    let weak = ui.as_weak();
    ui.on_save_diagnostics(move |range| {
        let range = session_logs::LogRange::from_key(range.as_str());
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if IN_PROGRESS.swap(true, Ordering::AcqRel) {
            return;
        }
        let (service_mode, cores_ready) = {
            let model = model.lock().expect("view model lock");
            (model.service_mode(), model.cores_ready())
        };
        let facts = log_bundle::UiFacts {
            scale_factor: ui.window().scale_factor(),
            connection: ui.get_toggle_title().to_string(),
            service_mode,
            cores_ready,
            tun_enabled: ui.get_tun_enabled(),
            session_uptime: ui.get_session_uptime().to_string(),
        };
        let owner = save_dialog::OwnerWindow::of(ui.window());
        let (weak, model) = (weak.clone(), model.clone());
        thread::spawn(move || {
            let outcome = save_diagnostics_archive(&weak, &model, owner, &facts, range);
            IN_PROGRESS.store(false, Ordering::Release);
            if let Some(Err(error)) = &outcome {
                daemon::record_low_level("diagnostics_archive_failed", error);
            }
            let state = match outcome {
                None => return,
                Some(Ok(false)) => "saved",
                Some(Ok(true)) => "saved-downloads",
                Some(Err(_)) => "failed",
            };
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else {
                    return;
                };
                ui.set_diagnostics_save_state(state.into());
                let reset = weak.clone();
                Timer::single_shot(Duration::from_secs(3), move || {
                    if let Some(ui) = reset.upgrade()
                        && ui.get_diagnostics_save_state() == state
                    {
                        ui.set_diagnostics_save_state("".into());
                    }
                });
            });
        });
    });
}

/// The worker half of «Сохранить лог…». `None` when the user cancelled the dialog; otherwise
/// whether the archive went to the Downloads fallback, or why it could not be written.
fn save_diagnostics_archive(
    weak: &slint::Weak<AppWindow>,
    model: &Arc<Mutex<DesktopViewModel>>,
    owner: save_dialog::OwnerWindow,
    facts: &log_bundle::UiFacts,
    range: session_logs::LogRange,
) -> Option<Result<bool, String>> {
    let name = log_bundle::default_file_name(chrono::Local::now());
    let (path, fallback) = match save_dialog::choose(owner, &name) {
        Ok(save_dialog::SaveTarget::Chosen(path)) => (path, false),
        Ok(save_dialog::SaveTarget::Fallback(path)) => (path, true),
        Ok(save_dialog::SaveTarget::Cancelled) => return None,
        Err(error) => return Some(Err(error)),
    };
    let saving = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = saving.upgrade() {
            ui.set_diagnostics_save_state("saving".into());
        }
    });
    // Pull the newest events first.
    let request = model
        .lock()
        .expect("view model lock")
        .begin_events_request();
    if let Some(request) = request {
        let result = request
            .client
            .events(request.after, request.known_epoch.as_deref());
        let events = model
            .lock()
            .expect("view model lock")
            .finish_events(request.token, result);
        if let Some(events) = events {
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    apply_events(&ui, events);
                }
            });
        }
    }
    let diagnostics = diagnostics_export(model);
    let client = model.lock().expect("view model lock").daemon_client();
    let daemon_diagnostics = match client.diagnostics() {
        Ok(diagnostics) => serde_json::to_string_pretty(&diagnostics)
            .unwrap_or_else(|error| serde_json::json!({ "error": error.to_string() }).to_string()),
        Err(error) => serde_json::json!({ "error": error.to_string() }).to_string(),
    };
    // The service's own log is SYSTEM/root-only: the daemon asks the service for it.
    let (service_log_note, service_logs) = log_bundle::service_log_entries(client.service_log());
    // The connections of the chosen range: the user's own journal and index, and the
    // service's journal of this user through the daemon.
    let data_root = data_root::user_data_root().ok();
    let plan = session_logs::SessionPlan::read(
        data_root.as_deref(),
        range,
        multicore_core::core_sessions::unix_ms_now(),
    );
    let service_journal = plan.since_ms().map(|since| client.core_journal(since));
    let (sessions_note, core_sessions) =
        session_logs::session_entries(&plan, data_root.as_deref(), service_journal, &service_logs);
    let mut inputs = log_bundle::BundleInputs::for_this_user(
        log_bundle::system_report(facts) + &service_log_note + &sessions_note,
        diagnostics,
        daemon_diagnostics,
    );
    inputs.service_logs = service_logs;
    inputs.core_sessions = core_sessions;
    inputs.windows_errors = log_bundle::windows_error_events();
    Some(
        log_bundle::write_bundle(&path, &inputs)
            .map(|()| fallback)
            .map_err(|error| error.to_string()),
    )
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

fn run_refresh(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
) {
    let (client, mutation, current) = {
        let mut model = model.lock().expect("view model lock");
        let mutation = model.begin_refresh();
        (model.daemon_client(), mutation, snapshot(&model))
    };
    if let Some(ui) = weak.upgrade() {
        apply_snapshot(&ui, &current);
    }
    run_status(weak, model, client, mutation, profile_preferences);
}

fn run_status(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    client: Arc<dyn DaemonClient>,
    mutation: MutationToken,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
) {
    thread::spawn(move || {
        let result = client.status();
        let refresh_catalog = result.is_ok();
        model
            .lock()
            .expect("view model lock")
            .finish_refresh(mutation, result);
        let _ = slint::invoke_from_event_loop(move || {
            let current = if let Some(ui) = weak.upgrade() {
                apply_latest_snapshot(&ui, &model)
            } else {
                snapshot(&model.lock().expect("view model lock"))
            };
            if refresh_catalog && !current.import_pending {
                load_profile_list(weak, model, profile_preferences, true);
            } else if refresh_catalog && current.import_pending {
                model
                    .lock()
                    .expect("view model lock")
                    .defer_profile_list_reload();
            }
        });
    });
}

fn run_import(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    client: Arc<dyn DaemonClient>,
    mutation: MutationToken,
    url: String,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
    pending: Arc<Mutex<PendingSubscriptionImports>>,
) {
    thread::spawn(move || {
        let result = client.import_subscription(&url);
        let import_succeeded = result.is_ok();
        {
            let mut model = model.lock().expect("view model lock");
            model.finish_import(mutation, result);
        }
        let _ = slint::invoke_from_event_loop(move || {
            let (queue_outcome, next_host) = {
                let mut pending = pending.lock().expect("pending subscription imports lock");
                let outcome = pending.finish_attempt(&url, import_succeeded, Instant::now());
                let next_host = pending.current_host();
                (outcome, next_host)
            };
            if let Some(ui) = weak.upgrade() {
                let current = apply_latest_snapshot(&ui, &model);
                if matches!(queue_outcome, PendingImportOutcome::Failed) {
                    prefill_subscription(&ui, url);
                }
                if matches!(
                    queue_outcome,
                    PendingImportOutcome::Succeeded | PendingImportOutcome::Failed
                ) {
                    ui.set_deep_link_import_host(next_host.unwrap_or_default().into());
                }
                if import_succeeded && !current.import_pending {
                    ui.set_import_success_serial(ui.get_import_success_serial().wrapping_add(1));
                }
            }
            advance_profile_reconciliation(weak, model, profile_preferences, import_succeeded);
        });
    });
}

/// The mode for the next connect: TUN unless the user switched it off (Windows only).
fn connect_uses_tun(ui: &AppWindow) -> bool {
    !ui.get_tun_toggle_available() || ui.get_tun_enabled()
}

fn run_connect(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    client: Arc<dyn DaemonClient>,
    mutation: MutationToken,
    queued: Vec<QueuedNodeSelection>,
    tun: bool,
) {
    thread::spawn(move || {
        let result = client.connect_with_tun(tun);
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
        apply_snapshot_from_worker(weak.clone(), current, model.clone(), refresh_catalog, false);
        // A connect may have moved the local proxy to another port (the saved one was taken).
        let proxy_address = saved_local_proxy_address();
        let proxy_ui = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = proxy_ui.upgrade() {
                ui.set_local_proxy_address(proxy_address.into());
            }
        });
        // Pull the bounded core log right away, success or failure, so the
        // first seconds of Xray/Mihomo startup are already cached in the
        // Events feed (see view_model::filtered_events) instead of only
        // being fetched if the user happens to open that panel afterward -
        // by then the window this covers may already be gone.
        load_diagnostics(weak, model);
    });
}

fn run_profile_activation(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
    client: Arc<dyn DaemonClient>,
    token: ProfileRequestToken,
    profile_id: String,
) {
    thread::spawn(move || {
        let result = client.activate_subscription(&profile_id);
        let current = {
            let mut model = model.lock().expect("view model lock");
            let succeeded = model.finish_profile_activation(token, result);
            if succeeded {
                model.open_panel(Panel::None);
            }
            snapshot(&model)
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_snapshot(&ui, &current);
            }
            advance_profile_reconciliation(weak, model, profile_preferences, false);
        });
    });
}

fn run_profile_target_refresh(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
    client: Arc<dyn DaemonClient>,
    token: ProfileRequestToken,
    profile_id: String,
) {
    thread::spawn(move || {
        let result = client.refresh_subscription_by_id(&profile_id);
        let current = {
            let mut model = model.lock().expect("view model lock");
            model.finish_profile_target_refresh(token, result);
            snapshot(&model)
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_snapshot(&ui, &current);
            }
            advance_profile_reconciliation(weak, model, profile_preferences, false);
        });
    });
}

fn run_profile_source_edit(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
    client: Arc<dyn DaemonClient>,
    token: ProfileRequestToken,
    profile_id: String,
    url: String,
) {
    thread::spawn(move || {
        let result = client.replace_subscription_source(&profile_id, &url);
        let (current, succeeded) = {
            let mut model = model.lock().expect("view model lock");
            let succeeded = model.finish_profile_source_edit(token, result);
            (snapshot(&model), succeeded)
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_snapshot(&ui, &current);
                if succeeded {
                    ui.set_profile_edit_id("".into());
                    ui.set_profile_edit_draft("".into());
                }
            }
            advance_profile_reconciliation(weak, model, profile_preferences, false);
        });
    });
}

fn run_profile_delete(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
    client: Arc<dyn DaemonClient>,
    token: ProfileRequestToken,
    profile_id: String,
    focus_plan: DeleteFocusPlan,
) {
    thread::spawn(move || {
        let result = client.delete_subscription(&profile_id);
        let (current, succeeded) = {
            let mut model = model.lock().expect("view model lock");
            let succeeded = model.finish_profile_delete(token, result);
            (snapshot(&model), succeeded)
        };
        let completion = focus_plan.completion(succeeded);
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_snapshot(&ui, &current);
                apply_delete_focus_completion(&ui, completion);
            }
            advance_profile_reconciliation(weak, model, profile_preferences, false);
        });
    });
}

fn load_profile_list(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
    catalog_after: bool,
) {
    let (token, client, current) = {
        let mut model = model.lock().expect("view model lock");
        if matches!(
            model.profile_presentation().pending_operation,
            Some(
                SubscriptionProfileOperation::Activate
                    | SubscriptionProfileOperation::Refresh
                    | SubscriptionProfileOperation::Edit
                    | SubscriptionProfileOperation::Delete
            )
        ) {
            return;
        }
        let token = model.begin_profile_list();
        (token, model.daemon_client(), snapshot(&model))
    };
    if let Some(ui) = weak.upgrade() {
        apply_snapshot(&ui, &current);
    }
    let Some(token) = token else {
        return;
    };
    thread::spawn(move || {
        let result = client.subscriptions();
        let succeeded = result.is_ok();
        let accepted = {
            let mut model = model.lock().expect("view model lock");
            let accepted = model.finish_profile_list(token, result);
            if accepted && succeeded {
                profile_preferences
                    .lock()
                    .expect("profile preference reconciler lock")
                    .reconcile_profile_list(&model);
            }
            accepted
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_latest_snapshot(&ui, &model);
            }
            if accepted && succeeded {
                advance_profile_reconciliation(weak, model, profile_preferences, catalog_after);
            }
        });
    });
}

fn advance_profile_reconciliation(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
    catalog_after: bool,
) {
    let (close_menu, reload_status, reload_list, reload_catalog) = {
        let mut model = model.lock().expect("view model lock");
        let close_menu = model.take_profile_close_intent();
        if close_menu {
            model.open_panel(Panel::None);
        }
        let reload_status = model.take_profile_status_reload_needed();
        let reload_list = !reload_status && model.take_profile_list_reload_needed();
        let reload_catalog =
            !reload_status && !reload_list && model.take_profile_catalog_reload_needed();
        (close_menu, reload_status, reload_list, reload_catalog)
    };
    if close_menu && let Some(ui) = weak.upgrade() {
        apply_snapshot(&ui, &snapshot(&model.lock().expect("view model lock")));
    }
    if reload_status {
        run_profile_status_reconciliation(weak, model, profile_preferences, catalog_after);
    } else if reload_list {
        load_profile_list(weak, model, profile_preferences, catalog_after);
    } else if reload_catalog || catalog_after {
        load_profile_catalog(weak, model, profile_preferences);
    }
}

fn run_profile_status_reconciliation(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
    catalog_after: bool,
) {
    let (client, mutation, current) = {
        let mut model = model.lock().expect("view model lock");
        let mutation = model.begin_refresh();
        (model.daemon_client(), mutation, snapshot(&model))
    };
    if let Some(ui) = weak.upgrade() {
        apply_snapshot(&ui, &current);
    }
    thread::spawn(move || {
        let result = client.status();
        let succeeded = result.is_ok();
        model
            .lock()
            .expect("view model lock")
            .finish_refresh(mutation, result);
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_latest_snapshot(&ui, &model);
            }
            if succeeded {
                advance_profile_reconciliation(weak, model, profile_preferences, catalog_after);
            }
        });
    });
}

fn run_subscription_refresh(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    client: Arc<dyn DaemonClient>,
    mutation: MutationToken,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
) {
    thread::spawn(move || {
        let result = client.refresh_subscription();
        let current = {
            let mut model = model.lock().expect("view model lock");
            let succeeded = model.finish_subscription_refresh(mutation, result);
            (snapshot(&model), succeeded)
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_snapshot(&ui, &current.0);
            }
            if current.1 && current.0.presentation.has_profile {
                load_catalog(weak.clone(), model.clone());
            }
            advance_profile_reconciliation(weak, model, profile_preferences, false);
        });
    });
}

fn run_disconnect(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    client: Arc<dyn DaemonClient>,
    mutation: MutationToken,
) {
    run_disconnect_with_update(weak, model, client, mutation, None);
}

/// Disconnects for a ready update; `after` runs on the event loop once the disconnect answered
/// (a reconciliation it needs runs on as usual and is waited for by the update).
fn run_disconnect_for_update(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    client: Arc<dyn DaemonClient>,
    mutation: MutationToken,
    after: Box<dyn FnOnce() + Send>,
) {
    run_disconnect_with_update(weak, model, client, mutation, Some(after));
}

fn run_disconnect_with_update(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    client: Arc<dyn DaemonClient>,
    mutation: MutationToken,
    after_update: Option<Box<dyn FnOnce() + Send>>,
) {
    thread::spawn(move || {
        let result = client.disconnect();
        let disconnected = result.is_ok();
        let (reconciliation, current) = {
            let mut model = model.lock().expect("view model lock");
            let reconcile_status = model.finish_disconnect(mutation, result);
            let reconciliation = reconcile_status.then(|| model.begin_disconnect_reconciliation());
            (reconciliation, snapshot(&model))
        };
        if let Some(after) = after_update {
            let (weak, model) = (weak.clone(), model.clone());
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    apply_latest_snapshot(&ui, &model);
                }
                after();
            });
        } else {
            // A disconnect clears the catalog revision: reload it, or (with the service's warm
            // cores) latency checks would never run again.
            apply_snapshot_from_worker(weak.clone(), current, model.clone(), disconnected, false);
        }
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
        let current = if let Some(ui) = weak.upgrade() {
            let current = apply_latest_snapshot(&ui, &model);
            if import_succeeded && !current.import_pending {
                ui.set_import_success_serial(ui.get_import_success_serial().wrapping_add(1));
            }
            current
        } else {
            current
        };
        if refresh_catalog && current.presentation.has_profile && !current.import_pending {
            load_catalog(weak, model);
        } else if refresh_catalog && current.presentation.has_profile && current.import_pending {
            model
                .lock()
                .expect("view model lock")
                .defer_profile_catalog_reload();
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
    load_catalog_impl(weak, model, None);
}

fn load_profile_catalog(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
) {
    load_catalog_impl(weak, model, Some(profile_preferences));
}

fn load_catalog_impl(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Option<Arc<Mutex<ProfilePreferenceReconciler>>>,
) {
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
        let catalog_succeeded = result.is_ok();
        let accepted = {
            let mut model = model.lock().expect("view model lock");
            model
                .finish_catalog(request.token, result)
                .map(|_| {
                    if let Some(preferences) = &profile_preferences {
                        preferences
                            .lock()
                            .expect("profile preference reconciler lock")
                            .apply_ready_hints_after_catalog(&mut model, catalog_succeeded);
                    }
                })
                .is_some()
        };
        if !accepted {
            return;
        }
        let icon_weak = weak.clone();
        let icon_model = model.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let latency_queued = weak
                .upgrade()
                .is_some_and(|ui| apply_latest_snapshot(&ui, &model).latency.queued);
            if latency_queued {
                begin_latency_check(weak, model);
            }
        });
        fetch_missing_group_icons(&request.client, icon_weak.clone(), icon_model.clone());
        poll_catalog_icons(&request.client, icon_weak, icon_model);
    });
}

/// How often the catalog is re-read while the daemon still downloads group icons.
const ICON_POLL_INTERVAL: Duration = Duration::from_millis(1_500);
/// The daemon gives its icon downloads 20 s; polling stops well after that regardless.
const ICON_POLL_LIMIT: Duration = Duration::from_secs(60);
static ICON_POLL_RUNNING: AtomicBool = AtomicBool::new(false);

/// While the daemon still downloads group icons (`icon_pending` in the catalog) or the
/// provider logo (`service_logo_pending` in the status), re-reads the catalog / status every
/// `ICON_POLL_INTERVAL` and takes only those fields, so placeholders turn into images without
/// a catalog reload, which would hold the route list for a moment each time. One poller at a
/// time.
fn poll_catalog_icons(
    client: &Arc<dyn daemon::DaemonClient>,
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
) {
    let images_pending = |model: &DesktopViewModel| {
        model.catalog_icons_pending() || model.subscription_logo_pending()
    };
    if !images_pending(&model.lock().expect("view model lock"))
        || ICON_POLL_RUNNING.swap(true, Ordering::SeqCst)
    {
        return;
    }
    let client = client.clone();
    thread::spawn(move || {
        let started = Instant::now();
        loop {
            thread::sleep(ICON_POLL_INTERVAL);
            let (icons, logo) = {
                let model = model.lock().expect("view model lock");
                (
                    model.catalog_icons_pending(),
                    model.subscription_logo_pending(),
                )
            };
            if !(icons || logo) || started.elapsed() > ICON_POLL_LIMIT {
                break;
            }
            let mut changed = false;
            if icons && let Ok(catalog) = client.catalog() {
                changed |= model
                    .lock()
                    .expect("view model lock")
                    .apply_catalog_icons(&catalog);
            }
            if logo && let Ok(status) = client.status() {
                changed |= model
                    .lock()
                    .expect("view model lock")
                    .apply_service_logo(&status);
            }
            if !changed {
                continue;
            }
            fetch_missing_group_icons(&client, weak.clone(), model.clone());
            let (weak, model) = (weak.clone(), model.clone());
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    apply_latest_snapshot(&ui, &model);
                }
            });
        }
        ICON_POLL_RUNNING.store(false, Ordering::SeqCst);
    });
}

/// Downloads (from the local daemon) and decodes the group icons the catalog names that are
/// not cached yet, on the calling worker thread, then redraws once if any arrived.
fn fetch_missing_group_icons(
    client: &Arc<dyn daemon::DaemonClient>,
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
) {
    let wanted = model.lock().expect("view model lock").catalog_icon_ids();
    let mut fetched = false;
    for id in wanted {
        if crisp_image::has_group_icon(&id) {
            continue;
        }
        match client.group_icon(&id) {
            Ok(png) => fetched |= crisp_image::store_group_icon(&id, &png),
            Err(error) => daemon::record_low_level("group_icon_fetch_failed", &error),
        }
    }
    if fetched {
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_latest_snapshot(&ui, &model);
            }
        });
    }
}

fn run_latency_check(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    mut request: LatencyRequest,
) {
    // The swept groups' selected routes are asked on their own first, so their rows update
    // as soon as they are measured rather than when the whole sweep answers.
    if let Some(priority) = request.priority.take() {
        run_node_latency(weak.clone(), model.clone(), priority);
    }
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

/// Right click on a route (home or tray), or a route picked while a sweep runs: measures only
/// that node, at once (the daemon's priority lane does not queue behind a sweep). Each row
/// shows its own placeholder while its request runs; several rows may be measured at once.
fn measure_catalog_node(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    node_id: &str,
) {
    let (request, current) = {
        let mut model = model.lock().expect("view model lock");
        let request = model.begin_node_latency_request(node_id);
        (request, snapshot(&model))
    };
    let Some(request) = request else {
        return;
    };
    if let Some(ui) = weak.upgrade() {
        apply_snapshot(&ui, &current);
    }
    run_node_latency(weak, model, request);
}

/// Sends a priority measurement of particular routes and shows each value when it arrives.
fn run_node_latency(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    request: NodeLatencyRequest,
) {
    thread::spawn(move || {
        let result = request
            .client
            .node_latencies(Some(request.catalog_revision), &request.node_ids);
        let completion = {
            let mut model = model.lock().expect("view model lock");
            model
                .finish_node_latency(
                    request.catalog_revision,
                    &request.node_ids,
                    request.token,
                    result,
                )
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

fn apply_latest_snapshot(ui: &AppWindow, model: &Arc<Mutex<DesktopViewModel>>) -> UiSnapshot {
    let current = snapshot(&model.lock().expect("view model lock"));
    apply_snapshot(ui, &current);
    current
}

fn snapshot(model: &DesktopViewModel) -> UiSnapshot {
    UiSnapshot {
        presentation: model.presentation(),
        panel: model.panel(),
        import_pending: model.import_pending(),
        import_error: model.import_error().unwrap_or_default().to_owned(),
        import_notice: model.import_notice().unwrap_or_default().to_owned(),
        subscription: model.subscription_presentation(),
        profiles: model.profile_presentation(),
        catalog: model.catalog_presentation(),
        diagnostics: model.diagnostics_presentation(),
        latency: model.latency_presentation(),
        conflicting_apps: model.conflicting_apps_presentation(),
        cores_ready: model.cores_ready(),
        service_mode: model.service_mode(),
    }
}

/// What the home screen's connection toggle shows for a connection state.
#[derive(Debug, PartialEq, Eq)]
struct TogglePresentation {
    title: String,
    /// Switch position: on while connected or while a connection is being made.
    on: bool,
    /// A connect or disconnect is in progress.
    busy: bool,
    /// The line under the title names the exit node; otherwise it carries
    /// the state's own explanation (an error, degraded mode).
    detail_is_exit: bool,
}

fn toggle_presentation(presentation: &StatePresentation) -> TogglePresentation {
    let (title, on, busy, detail_is_exit) = if presentation.is_busy {
        // Disconnecting still reports is_connected, so busy is decided first.
        if presentation.is_connected {
            (tr!("Отключение…").to_owned(), false, true, true)
        } else {
            (tr!("Подключение…").to_owned(), true, true, true)
        }
    } else if presentation.is_connected {
        (tr!("Защита включена").to_owned(), true, false, true)
    } else if presentation.is_error || presentation.is_degraded {
        (presentation.eyebrow.to_owned(), false, false, false)
    } else {
        (tr!("Защита выключена").to_owned(), false, false, true)
    };
    TogglePresentation {
        title,
        on,
        busy,
        detail_is_exit,
    }
}

/// First letter or digit of a subscription name, upper-cased, for the chip avatar.
fn subscription_initial(name: &str) -> String {
    name.chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_else(|| tr!("П").into())
}

/// Share of the traffic limit used, for the ring gauges (0 without a limit).
fn used_fraction(quota: &view_model::QuotaPresentation) -> f32 {
    quota
        .used_permille
        .map_or(0.0, |permille| f32::from(permille) / 1000.0)
}

/// Notices a subscription switch between snapshots, so the new subscription's provider
/// notice opens once and hides itself again (see `announcement-auto-hide`).
#[derive(Debug, Default)]
struct SwitchNotice {
    active: Option<String>,
    pending_since: Option<Instant>,
}

impl SwitchNotice {
    /// How long after a switch its notice may still arrive (a refresh can bring it).
    const WINDOW: Duration = Duration::from_secs(30);

    /// Feeds one snapshot's active profile; true when this snapshot should open its notice
    /// as the switch notice. The first profile after launch is not a switch, and a moment
    /// without an active profile (a reload, a deleted profile) keeps the last one.
    fn observe(&mut self, active: Option<&str>, has_notice: bool, now: Instant) -> bool {
        if let Some(current) = active {
            if self
                .active
                .as_deref()
                .is_some_and(|previous| previous != current)
            {
                self.pending_since = Some(now);
            }
            self.active = Some(current.to_owned());
        }
        match self.pending_since {
            Some(since) if now.duration_since(since) <= Self::WINDOW => {
                if has_notice {
                    self.pending_since = None;
                }
                has_notice
            }
            Some(_) => {
                self.pending_since = None;
                false
            }
            None => false,
        }
    }
}

thread_local! {
    static SWITCH_NOTICE: RefCell<SwitchNotice> = RefCell::new(SwitchNotice::default());
}

/// Stable 64-bit FNV-1a fingerprint of a provider notice (`None` when there is
/// none). Stable across Rust versions, unlike `DefaultHasher`.
fn announcement_fingerprint(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let hash = text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    Some(format!("{hash:016x}"))
}

/// Splits a leading emoji (a provider flag) off `name` as an image, since the UI
/// font draws regional-indicator flags as letters ("AL").
fn split_leading_emoji_image(name: &str) -> (slint::Image, &str) {
    let name = name.trim();
    match emoji_images::leading_emoji_len(name) {
        Some(end) => (
            emoji_images::image_for(&name[..end]),
            name[end..].trim_start(),
        ),
        None => (slint::Image::default(), name),
    }
}

/// Brings the window to the current language (a provider header may have changed it while a
/// daemon status was applied): switches the bundled Slint translation, and the `language`
/// property's change handler (`wire_language`) re-renders the texts built in Rust.
fn sync_language(ui: &AppWindow) {
    let current = language::current();
    ui.set_language_set_by_user(language::user_chosen());
    ui.set_language_from_provider(language::source() == language::Source::Provider);
    if ui.get_language() != current.code() {
        select_slint_language(current);
        ui.set_language(current.code().into());
    }
}

fn select_slint_language(language: language::Lang) {
    if let Err(error) = slint::select_bundled_translation(language.slint_bundle()) {
        eprintln!("interface language could not be selected: {error}");
    }
}

/// «Язык / Language» in Settings, and the re-render after any language change.
fn wire_language(
    ui: &AppWindow,
    model: Arc<Mutex<DesktopViewModel>>,
    update_state: Arc<Mutex<UpdateState>>,
    tray_popup: Rc<TrayPopupSlot>,
) {
    let weak = ui.as_weak();
    ui.on_select_language(move |code| {
        let (Some(ui), Some(language)) = (weak.upgrade(), language::Lang::from_tag(&code)) else {
            return;
        };
        language::choose(language);
        sync_language(&ui);
    });
    let weak = ui.as_weak();
    ui.on_language_changed(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        // The tray popup exists only while it is in use; a new one is built in the current language.
        refresh_localized_texts(&ui, &model, &update_state, &tray_popup.weak());
    });
}

/// Re-applies every text the host builds, in the current language. Texts already shown as a
/// one-off result (an error of the last action, a toast) keep the language they were made in.
fn refresh_localized_texts(
    ui: &AppWindow,
    model: &Arc<Mutex<DesktopViewModel>>,
    update_state: &Arc<Mutex<UpdateState>>,
    tray_popup: &slint::Weak<TrayPopup>,
) {
    let current = apply_latest_snapshot(ui, model);
    apply_update_state(
        ui,
        &update_state.lock().expect("update state lock"),
        runtime_allows_update(&current.presentation),
    );
    apply_launch_at_sign_in(ui, windows_settings::query_launch_at_sign_in());
    refresh_tray_popup(tray_popup, model);
    let feed = model.lock().expect("view model lock").event_feed();
    apply_events(ui, feed);
    load_events(ui.as_weak(), model.clone());
    LANGUAGE_REFRESHERS.with(|refreshers| {
        for refresh in refreshers.borrow().iter() {
            refresh();
        }
    });
}

thread_local! {
    /// Re-renders of Rust-built texts kept outside the view model (the troubleshooting run,
    /// …), run by `refresh_localized_texts` after a language switch. UI thread only.
    static LANGUAGE_REFRESHERS: RefCell<Vec<Box<dyn Fn()>>> = RefCell::new(Vec::new());
}

/// Registers a re-render for `refresh_localized_texts`.
fn on_language_refresh(refresh: impl Fn() + 'static) {
    LANGUAGE_REFRESHERS.with(|refreshers| refreshers.borrow_mut().push(Box::new(refresh)));
}

fn apply_snapshot(ui: &AppWindow, snapshot: &UiSnapshot) {
    sync_language(ui);
    let presentation = &snapshot.presentation;
    let toggle = toggle_presentation(presentation);
    ui.set_toggle_title(toggle.title.into());
    ui.set_toggle_on(toggle.on);
    ui.set_toggle_busy(toggle.busy);
    ui.set_toggle_detail_is_exit(toggle.detail_is_exit);
    ui.set_status_eyebrow(SharedString::from(presentation.eyebrow));
    ui.set_headline(presentation.headline.clone().into());
    ui.set_supporting(presentation.supporting.clone().into());
    let (node_icon, node_name) = split_leading_symbol(&presentation.node);
    ui.set_node_icon_image(catalog_icon_image(
        snapshot.catalog.exit_icon_id.as_deref(),
        &node_icon,
        EXIT_LINE_ICON_PX,
        ui.window().scale_factor(),
    ));
    ui.set_node_icon(node_icon.into());
    ui.set_node_name(node_name.into());
    ui.set_has_profile(presentation.has_profile);
    ui.set_primary_label(SharedString::from(presentation.primary_label));
    ui.set_primary_enabled(presentation.primary_enabled);
    ui.set_primary_queueable(presentation.primary_queueable);
    ui.set_update_runtime_busy(presentation.is_busy);
    ui.set_dashboard_available(presentation.is_connected || snapshot.cores_ready);
    // Never offer the service while the daemon already runs on it (or it is registered).
    ui.set_service_card_visible(service_install::card_visible(
        snapshot.service_mode || service_install::service_installed(),
    ));
    ui.set_troubleshoot_offer_visible(troubleshoot::offered_after_failure(presentation));
    ui.set_troubleshoot_service_reinstall(troubleshoot::service_reinstall_available(
        cfg!(windows),
        service_install::per_user_install(),
        snapshot.service_mode,
        service_install::service_installed(),
    ));
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
    ui.set_import_notice(snapshot.import_notice.clone().into());
    ui.set_subscription_title(snapshot.subscription.display_name.clone().into());
    ui.set_subscription_initial(subscription_initial(&snapshot.subscription.display_name).into());
    ui.set_subscription_usage(snapshot.subscription.usage.clone().into());
    ui.set_subscription_expiry(snapshot.subscription.expiry.clone().into());
    let quota = &snapshot.subscription.quota;
    ui.set_subscription_remaining(quota.remaining.clone().into());
    ui.set_subscription_days(quota.days.clone().into());
    ui.set_subscription_used_fraction(used_fraction(quota));
    ui.set_subscription_unlimited(quota.used_permille.is_none());
    ui.set_subscription_quota_tone(quota.tone.into());
    ui.set_subscription_quota_tooltip(quota.tooltip.as_str().into());
    ui.set_subscription_updated(snapshot.subscription.updated.clone().into());
    ui.set_subscription_updated_stale(snapshot.subscription.updated_stale);
    ui.set_subscription_announcement_text(snapshot.subscription.announcement_text.clone().into());
    // A notice the user has not seen yet opens its popover once, on launch or
    // when a refresh brings a new one; an unchanged notice stays behind the bell. Like a
    // switch's notice it hides itself after 15 s (paused while the pointer is over it).
    let fingerprint = announcement_fingerprint(&snapshot.subscription.announcement_text);
    if let Some(fingerprint) = &fingerprint
        && ui.get_announcement_seen().as_str() != fingerprint
    {
        ui.set_announcement_seen(fingerprint.as_str().into());
        ui.set_announcement_unread(true);
        ui.set_announcement_auto_hide(true);
        ui.set_announcement_open(true);
    }
    // Switching subscriptions shows the new one's notice, which hides itself after 15 s
    // (paused while the pointer is over it); the bell still reopens it.
    let switched = SWITCH_NOTICE.with(|notice| {
        notice.borrow_mut().observe(
            snapshot.profiles.active_id.as_deref(),
            fingerprint.is_some(),
            Instant::now(),
        )
    });
    if switched && let Some(fingerprint) = fingerprint {
        ui.set_announcement_seen(fingerprint.into());
        ui.set_announcement_auto_hide(true);
        ui.set_announcement_open(true);
    }
    ui.set_subscription_announcement_tone(snapshot.subscription.announcement_tone.clone().into());
    let referenced_logo_paths = snapshot
        .subscription
        .service_logo_path
        .iter()
        .map(String::as_str)
        .chain(
            snapshot
                .profiles
                .profiles
                .iter()
                .filter_map(|profile| profile.service_logo_path.as_deref()),
        )
        .collect::<Vec<_>>();
    // Each logo is resampled to the exact physical size of the slot it is drawn in.
    let scale = ui.window().scale_factor();
    let (service_logo, subscription_profiles) = logo_cache::with_logo_cache(|cache| {
        cache.retain_paths(referenced_logo_paths.iter().copied());
        let service_logo = snapshot
            .subscription
            .service_logo_path
            .as_deref()
            .and_then(|path| cache.image_for_path(path, QUOTA_LOGO_PX, scale));
        let profiles = snapshot
            .profiles
            .profiles
            .iter()
            .map(|profile| {
                let path = profile.service_logo_path.as_deref();
                let logo = path.and_then(|path| cache.image_for_path(path, PROFILE_LOGO_PX, scale));
                let logo_small =
                    path.and_then(|path| cache.image_for_path(path, QUOTA_LOGO_PX, scale));
                UiSubscriptionProfile {
                    id: profile.id.clone().into(),
                    title: profile.title.clone().into(),
                    usage: profile.usage.clone().into(),
                    expiry: profile.expiry.clone().into(),
                    active: profile.active,
                    available: profile.available,
                    has_logo: logo.is_some(),
                    logo: logo.unwrap_or_default(),
                    logo_small: logo_small.unwrap_or_default(),
                    fallback: profile
                        .title
                        .chars()
                        .next()
                        .map(|letter| letter.to_uppercase().to_string())
                        .unwrap_or_else(|| "M".into())
                        .into(),
                    remaining: profile.quota.remaining.clone().into(),
                    days: profile.quota.days.clone().into(),
                    used_fraction: used_fraction(&profile.quota),
                    unlimited: profile.quota.used_permille.is_none(),
                    quota_tone: profile.quota.tone.into(),
                    expired: profile.quota.expired,
                    quota_tooltip: profile.quota.tooltip.as_str().into(),
                }
            })
            .collect::<Vec<_>>();
        (service_logo, profiles)
    });
    ui.set_has_service_logo(service_logo.is_some());
    ui.set_service_logo(service_logo.unwrap_or_default());
    ui.set_subscription_refresh_available(snapshot.subscription.refresh_available);
    ui.set_subscription_refresh_enabled(presentation.has_profile && !presentation.is_busy);
    ui.set_subscription_management_enabled(
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
    ui.set_subscription_profiles(ModelRc::new(VecModel::from(subscription_profiles)));
    ui.set_profile_menu_loading(snapshot.profiles.loading);
    ui.set_profile_menu_error(snapshot.profiles.error.clone().unwrap_or_default().into());
    ui.set_profile_mutation_error(
        snapshot
            .profiles
            .mutation_error
            .clone()
            .unwrap_or_default()
            .into(),
    );
    ui.set_profile_switch_locked(snapshot.profiles.switch_locked);
    ui.set_active_profile_id(
        snapshot
            .profiles
            .active_id
            .clone()
            .unwrap_or_default()
            .into(),
    );
    ui.set_profile_pending_operation(
        match snapshot.profiles.pending_operation {
            Some(SubscriptionProfileOperation::List) => "list",
            Some(SubscriptionProfileOperation::Activate) => "activate",
            Some(SubscriptionProfileOperation::Refresh) => "refresh",
            Some(SubscriptionProfileOperation::Edit) => "edit",
            Some(SubscriptionProfileOperation::Delete) => "delete",
            None => "none",
        }
        .into(),
    );
    ui.set_profile_pending_id(
        snapshot
            .profiles
            .pending_id
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
    ui.set_latency_check_available(snapshot.latency.available);
    ui.set_latency_check_error(snapshot.latency.error.clone().unwrap_or_default().into());
    ui.set_diagnostic_mappings(ModelRc::new(VecModel::from(
        snapshot
            .diagnostics
            .mappings
            .iter()
            .map(|mapping| {
                let (icon_image, proxy_name) = split_leading_emoji_image(&mapping.proxy_name);
                UiProxyMapping {
                    icon_image,
                    proxy_name: proxy_name.into(),
                    address: mapping.address.clone().into(),
                    xray_label: mapping.xray_label.clone().into(),
                }
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_conflicting_apps_banner_visible(snapshot.conflicting_apps.visible);
    ui.set_conflicting_apps_message(snapshot.conflicting_apps.message.clone().into());
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

/// Logical sizes of the image slots in app.slint and tray_popup.slint; images are resampled
/// to exactly these (times the scale factor) by `crisp_image`.
const QUOTA_LOGO_PX: f32 = 12.0;
const PROFILE_LOGO_PX: f32 = 18.0;
const GROUP_TAB_ICON_PX: f32 = 15.0;
const ROUTE_ROW_ICON_PX: f32 = 22.0;
const EXIT_LINE_ICON_PX: f32 = 14.0;
const TRAY_SEGMENT_ICON_PX: f32 = 14.0;
const TRAY_ROW_ICON_PX: f32 = 20.0;

/// A group's (or group-node's) downloaded icon when there is one, else the emoji image.
fn catalog_icon_image(
    icon_id: Option<&str>,
    emoji: &str,
    logical_px: f32,
    scale: f32,
) -> slint::Image {
    icon_id
        .and_then(|id| crisp_image::group_icon(id, logical_px, scale))
        .unwrap_or_else(|| emoji_images::image_for(emoji))
}

/// Updates `current` row by row when it already shows the same rows (same ids, same order):
/// the row components are kept, so their own transitions (an icon cross-fading in when its
/// download finishes) can run. Otherwise (another group, rows added or removed) returns a new
/// model to set; replacing the model rebuilds every row.
fn sync_rows<T: Clone + PartialEq + 'static>(
    current: ModelRc<T>,
    rows: Vec<T>,
    id: impl Fn(&T) -> SharedString,
) -> Option<ModelRc<T>> {
    use slint::Model as _;
    if let Some(model) = current.as_any().downcast_ref::<VecModel<T>>()
        && model.row_count() == rows.len()
        && rows
            .iter()
            .enumerate()
            .all(|(index, row)| model.row_data(index).is_some_and(|old| id(&old) == id(row)))
    {
        for (index, row) in rows.into_iter().enumerate() {
            if model.row_data(index).as_ref() != Some(&row) {
                model.set_row_data(index, row);
            }
        }
        return None;
    }
    Some(ModelRc::new(VecModel::from(rows)))
}

/// Latency of the exit shown on the home toggle, when the visible catalog holds
/// it as the selected node; empty otherwise (never a placeholder like "—").
fn apply_catalog(ui: &AppWindow, catalog: &CatalogPresentation) {
    // Keep open search results in step with catalog and latency updates.
    if ui.get_route_search_open() {
        apply_route_search(ui, catalog, ui.get_route_search_query().as_str());
    }
    let scale = ui.window().scale_factor();
    let groups = catalog
        .groups
        .iter()
        .map(|group| {
            let display = derive_catalog_display(&group.label, CatalogDisplayKind::Group);
            UiCatalogGroup {
                id: group.id.clone().into(),
                icon_image: catalog_icon_image(
                    group.icon_id.as_deref(),
                    &display.icon,
                    GROUP_TAB_ICON_PX,
                    scale,
                ),
                icon: display.icon.into(),
                icon_loading: group.icon_pending,
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
                icon_image: catalog_icon_image(
                    node.icon_id.as_deref(),
                    &display.icon,
                    ROUTE_ROW_ICON_PX,
                    scale,
                ),
                icon: display.icon.into(),
                icon_loading: node.icon_pending,
                label: display.label.into(),
                selected: node.selected,
                latency_text: node.latency_text.clone().into(),
                latency_tone: node.latency_tone.clone().into(),
                measuring: node.measuring,
            }
        })
        .collect::<Vec<_>>();
    ui.set_catalog_dumb_mode(catalog.dumb_mode);
    ui.set_node_latency(catalog.exit_latency_text.clone().into());
    if let Some(groups) = sync_rows(ui.get_catalog_groups(), groups, |group| group.id.clone()) {
        ui.set_catalog_groups(groups);
    }
    if let Some(nodes) = sync_rows(ui.get_catalog_nodes(), nodes, |node| node.id.clone()) {
        ui.set_catalog_nodes(nodes);
    }
    ui.set_catalog_loading(catalog.loading);
    ui.set_catalog_selection_enabled(catalog.selection_enabled);
    ui.set_catalog_selection_pending(catalog.selection_pending);
    ui.set_catalog_error(catalog.error.clone().unwrap_or_default().into());
}

#[cfg(test)]
mod row_sync_tests {
    use slint::{Model, ModelRc, VecModel};

    use super::{UiCatalogNode, sync_rows};

    fn node(id: &str, loading: bool) -> UiCatalogNode {
        UiCatalogNode {
            id: id.into(),
            icon_loading: loading,
            ..UiCatalogNode::default()
        }
    }

    #[test]
    fn the_same_rows_are_updated_in_place_and_other_rows_replace_the_model() {
        let current = ModelRc::new(VecModel::from(vec![node("a", true), node("b", false)]));
        // An icon arrived: same rows, so the row components stay (their cross-fade runs).
        assert!(
            sync_rows(
                current.clone(),
                vec![node("a", false), node("b", false)],
                |row| row.id.clone()
            )
            .is_none()
        );
        assert!(!current.row_data(0).unwrap().icon_loading);
        // Another group's rows: a new model.
        let replaced = sync_rows(
            current.clone(),
            vec![node("c", false), node("d", false)],
            |row| row.id.clone(),
        )
        .expect("other rows");
        assert_eq!(replaced.row_data(0).unwrap().id, "c");
        assert!(sync_rows(current, vec![node("a", false)], |row| row.id.clone()).is_some());
    }
}

#[cfg(test)]
mod window_tests {
    use super::{
        DeleteFocusState, DeleteFocusTarget, LaunchArguments, LaunchMode,
        MAX_PENDING_SUBSCRIPTION_IMPORTS, MAX_READY_UPDATE_WAITS, PREFERENCE_RETRY_DELAYS,
        PendingImportOutcome, PendingSubscriptionConsents, PendingSubscriptionImports,
        PersistenceTracker, PreferenceWriteFailure, PreferenceWriter, ProfilePreferenceReconciler,
        ReadyUpdateStep, UpdateConfirmPlan, UpdateInstallIntent, WindowObservation, WriterCommand,
        WriterResult, activation_for_launch_arguments, ambient_motion_allowed,
        begin_update_download, delete_focus_plan, drain_latest_write, initial_window_visible,
        next_maximized, parse_launch_arguments, preference_error_for_result,
        preference_failure_is_terminal, preference_save_error, ready_update_step,
        reduced_motion_from_client_area_animation, retry_delay_after_failure, route_hints_apply_to,
        runtime_allows_update, scale_factor_milli, shell_active_after_native_minimize_change,
        startup_should_connect, update_check_due, update_confirm_plan, update_install_intent,
        update_through_service, visible_page_from_name, visible_page_name,
    };
    use crate::daemon::test_support::MockDaemonClient;
    use crate::daemon::{
        DaemonCatalog, DaemonCatalogGroup, DaemonCatalogNode, DaemonError, DaemonStatus,
    };
    use crate::preferences::{AppPreferences, PreferenceStore, VisiblePage, WindowBounds};
    use crate::single_instance::Activation;
    use crate::view_model::{DesktopViewModel, UiState};
    use crate::windows_shell::{
        CloseDisposition, MinimizeDisposition, close_disposition, minimize_disposition,
        parse_resize_edge,
    };
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

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
    fn ambient_motion_gate_stops_on_every_inactive_path() {
        assert!(ambient_motion_allowed(true, true, false, false, true));
        assert!(!ambient_motion_allowed(false, true, false, false, true));
        assert!(!ambient_motion_allowed(true, false, false, false, true));
        assert!(!ambient_motion_allowed(true, true, true, false, true));
        assert!(!ambient_motion_allowed(true, true, false, true, true));
        assert!(!ambient_motion_allowed(true, true, false, false, false));
    }

    #[test]
    fn branded_tray_popup_source_contract() {
        let app = include_str!("../ui/app.slint");
        let popup = include_str!("../ui/tray_popup.slint").replace("\r\n", "\n");
        let rust = include_str!("main.rs");

        for required in [
            "export { TrayPopup, TrayMenuRow } from \"tray_popup.slint\";",
            "export component TrayPopup inherits Window",
            "no-frame: true;",
            "always-on-top: true;",
            "import \"../assets/fonts/Twemoji.Mozilla.ttf\";",
            "icon: string,",
            "detail: string,",
            "tone: string,",
            "callback toggle-group(string);",
            // Header wordmark + subscription, module with switch, segments, list, footer.
            "text: \"MULTICORE\";",
            "connect-card := Rectangle",
            "card-mesh := Rectangle",
            "switch-track := Rectangle",
            "component TraySegment inherits Rectangle",
            "component TrayNodeRow inherits Rectangle",
            "show-separator: index < root.node-rows.length - 1;",
            "component TrayTextAction inherits Rectangle",
            "TrayInteraction::ShowMenu",
            "fn sync_tray_live_state(",
            "place_tray_popup(",
            ".take(12)",
            ".take(64)",
        ] {
            assert!(
                app.contains(required) || popup.contains(required) || rust.contains(required),
                "missing branded tray popup contract: {required}"
            );
        }
        // The old accordion, big connect button and check marks are gone.
        assert!(!popup.contains("expanded: bool,"));
        assert!(!popup.contains("TrayConnectAction"));
        assert!(!popup.contains("M 1 5 L 4.5 8.5 L 11 1.5"));
        assert!(!rust.contains(concat!("TrayAccordion", "State")));
        // Optimistic switch, as on the home screen.
        assert!(popup.contains("root.toggle-intent = root.toggle-on ? \"off\" : \"on\";"));
        assert!(popup.contains("toggle-intent-expiry := Timer {\n        interval: 8s;"));
        // Busy ring reads animation-tick() only while busy and motion is allowed.
        assert!(popup.contains("if root.shown-busy && !root.reduced-motion: Rectangle {"));
        // The mesh only shows under the pointer.
        assert!(popup.contains("opacity: self.active ? 1 : 0;"));
    }

    #[test]
    fn tray_popup_shows_one_group_with_graded_latency_rows() {
        let popup = include_str!("../ui/tray_popup.slint").replace("\r\n", "\n");
        let rust = include_str!("main.rs");
        // Rust sends only the shown group's nodes; the list has no hidden rows.
        assert!(rust.contains("if shown_group.as_deref() == Some(group_id.as_str()) =>"));
        assert!(popup.contains("for node[index] in root.node-rows: TrayNodeRow {"));
        let row = popup
            .split("component TrayNodeRow inherits Rectangle")
            .nth(1)
            .and_then(|source| source.split("export component TrayPopup").next())
            .expect("tray node row");
        assert!(row.contains("height: 44px;"));
        assert!(row.contains("root.tone == \"success\" ? Theme.accent-green"));
        assert!(row.contains("root.tone == \"warning\" ? Theme.warning"));
        assert!(row.contains("root.tone == \"danger\" ? Theme.error"));
        assert!(row.contains("background: root.selected ? Theme.elevated"));
        assert!(row.contains("if root.show-separator: Rectangle {"));
    }

    #[test]
    fn tray_popup_resizes_from_its_free_edges_and_remembers_the_size() {
        let popup = include_str!("../ui/tray_popup.slint").replace("\r\n", "\n");
        for required in [
            "min-width: 304px;",
            "min-height: 460px;",
            "max-width: 520px;",
            "max-height: 900px;",
            "v-grip := TouchArea",
            "h-grip := TouchArea",
            "corner-grip := TouchArea",
            "callback resize-drag(length, length);",
            "if root.has-profile && !root.subscription-unlimited: Rectangle {",
        ] {
            assert!(
                popup.contains(required),
                "missing tray resize contract: {required}"
            );
        }
        assert!(!popup.contains("width: 304px;\n    height: 460px;"));
        // Resizing shows only the lit edges, never a size readout.
        assert!(!popup.contains(" × "));
        assert!(!popup.contains("size-label"));
        let rust = include_str!("main.rs");
        assert!(rust.contains("wire_tray_popup_resize("));
        assert!(rust.contains(".set_tray_popup_size(logical)"));
        assert!(rust.contains("TrayPopupGeometry::new(loaded_preferences.tray_popup_size)"));
    }

    #[test]
    fn title_chip_motion_settles_without_running_timers() {
        let app = include_str!("../ui/app.slint").replace("\r\n", "\n");
        // Each phase timer runs only inside its phase; the spinner reads the clock
        // only while visible and never under reduced motion.
        for required in [
            "running: root.chip-phase == \"logo\";",
            "running: root.chip-phase == \"hold\";",
            "running: root.chip-logo-entering;",
            "root.chip-logo-entering = !root.reduced-motion;",
            "transform-rotation: self.visible && !root.reduced-motion",
            "if (root.subscription-refresh-error == \"\") {",
        ] {
            assert!(
                app.contains(required),
                "missing chip motion contract: {required}"
            );
        }
    }

    #[test]
    fn tray_popup_uses_an_opaque_fallback_and_native_windows_rounding() {
        let popup = include_str!("../ui/tray_popup.slint");
        let rust = include_str!("main.rs");
        let shell = include_str!("windows_shell.rs");
        let window_header = popup
            .split("export component TrayPopup inherits Window")
            .nth(1)
            .and_then(|source| source.split("in property <string> status-text").next())
            .expect("tray popup window header");

        assert!(window_header.contains("background: Theme.canvas;"));
        assert!(!window_header.contains("background: transparent;"));
        assert!(rust.contains("apply_tray_popup_native_rounding(window)"));
        assert!(shell.contains("DwmSetWindowAttribute"));
        assert!(shell.contains("DWMWA_WINDOW_CORNER_PREFERENCE"));
        assert!(shell.contains("DWMWCP_ROUND"));
    }

    #[test]
    fn tray_popup_lifecycle_keeps_in_place_actions_open_but_closes_terminal_actions() {
        let popup = include_str!("../ui/tray_popup.slint");
        let rust = include_str!("main.rs");

        assert!(popup.contains("event.text == Key.Escape"));
        assert!(popup.contains("root.dismiss();"));
        let dispatch = rust
            .split("fn dispatch_tray_popup_command(")
            .nth(1)
            .and_then(|source| source.split("fn wire_tray_popup_actions(").next())
            .expect("tray popup command dispatcher");
        assert!(
            dispatch.find("hide_tray_popup").unwrap()
                < dispatch.find("handle_tray_command").unwrap()
        );
        let selection_dispatch = rust
            .split("fn dispatch_tray_popup_selection(")
            .nth(1)
            .and_then(|source| source.split("fn wire_tray_popup_actions(").next())
            .expect("tray popup selection dispatcher");
        assert!(!selection_dispatch.contains("hide_tray_popup"));
        assert!(selection_dispatch.contains("handle_tray_command"));
        assert!(selection_dispatch.contains("apply_tray_popup_model"));
        let main_entry = rust
            .split("fn main()")
            .nth(1)
            .and_then(|source| source.split("fn activation_for_launch_arguments(").next())
            .expect("desktop main entrypoint");
        let compact_main = main_entry.split_whitespace().collect::<String>();
        // Every popup the slot creates gets the focus-loss timer, kept with it.
        assert!(compact_main.contains(
            "letfocus_timer=wire_tray_popup_focus_loss(&tray_popup,tray_popup_open.clone(),model.clone(),ui.clone(),);Ok((tray_popup,focus_timer))"
        ));
        // The switch keeps the popup open (it goes through the in-place dispatcher),
        // while opening the window and quitting close it.
        let actions = rust
            .split("fn wire_tray_popup_actions(")
            .nth(1)
            .and_then(|source| source.split("fn wire_tray_popup_focus_loss(").next())
            .expect("tray popup action wiring");
        assert!(actions.contains("wire_in_place(|_, _| TrayCommand::ToggleConnection)"));
        assert!(actions.contains("popup.on_open_main(wire_command(TrayCommand::ShowWindow));"));
        assert!(actions.contains("popup.on_exit_app(wire_command(TrayCommand::Exit));"));
        let compact_rust = rust.split_whitespace().collect::<String>();
        assert!(compact_rust.contains("ifopen.get(){hide_tray_popup"));
        assert!(rust.contains("with_winit_window(|window| window.has_focus())"));
        assert!(rust.contains("let _ = tray_popup.hide();"));
    }

    #[test]
    fn tray_segments_and_rows_use_their_whole_surface_as_the_trigger() {
        let popup = include_str!("../ui/tray_popup.slint");
        for (component, next) in [
            (
                "component TraySegment inherits Rectangle",
                "component TrayNodeRow inherits Rectangle",
            ),
            (
                "component TrayNodeRow inherits Rectangle",
                "export component TrayPopup inherits Window",
            ),
        ] {
            let source = popup
                .split(component)
                .nth(1)
                .and_then(|rest| rest.split(next).next())
                .expect("tray component source");
            for required in [
                "FocusScope {",
                "TouchArea {",
                "clicked => { root.activated(); }",
                "accessible-role: button;",
            ] {
                assert!(source.contains(required), "{component}: {required}");
            }
        }
    }

    #[test]
    fn native_minimize_and_restore_drive_shell_activity_without_tray_wakeup() {
        assert!(!shell_active_after_native_minimize_change(true));
        assert!(shell_active_after_native_minimize_change(false));

        let source = include_str!("../ui/app.slint");
        let handler = source
            .split("changed minimized => {")
            .nth(1)
            .and_then(|source| source.split('}').next())
            .expect("native minimized change handler");
        assert!(handler.contains("root.shell-active = !root.minimized;"));
        assert!(!handler.contains("if (!root.minimized)"));
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
    fn automatic_subscription_refresh_and_home_timestamp_are_wired() {
        let rust = include_str!("main.rs");
        let ui = include_str!("../ui/app.slint");

        for required in [
            "fn wire_automatic_subscription_refresh(",
            "profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>",
            "automatic_subscription_refresh_due_at(now_unix)",
            "Duration::from_secs(60)",
            "set_subscription_updated",
            "set_subscription_updated_stale",
            "advance_profile_reconciliation(weak, model, profile_preferences, false)",
        ] {
            assert!(
                rust.contains(required),
                "missing auto-refresh contract: {required}"
            );
        }
        for required in [
            "in property <string> subscription-updated",
            "in property <bool> subscription-updated-stale",
            "title-subscription := Rectangle",
            "subscription-refresh-action := FocusScope",
            "root.subscription-refresh-pending ? @tr(\"Подписка обновляется\") : @tr(\"Обновить подписку\")",
            "root.refresh-subscription();",
            "accessible-description: @tr(\"{}, осталось {}, {}. Обновлено {}. {}\", root.subscription-title, root.subscription-remaining, root.subscription-days, root.subscription-updated",
            "announcement-popover := Rectangle",
            "announcement-copy := Text",
            "routes-heading := HorizontalLayout",
            "route-updated-meta := Text",
        ] {
            assert!(
                ui.contains(required),
                "missing updated-at UI contract: {required}"
            );
        }
        // Refresh is a 28px icon right after the title-bar subscription chip; it spins
        // while the fetch runs (unless reduced motion is on).
        let refresh_action = ui
            .split("subscription-refresh-action := FocusScope")
            .nth(1)
            .and_then(|source| source.split("announcement-bell := FocusScope").next())
            .expect("subscription refresh action source");
        for unchanged in [
            "x: parent.chip-width + 4px;",
            "y: 1px;",
            "width: 28px;",
            "height: 28px;",
            "@image-url(\"../assets/title-refresh.svg\")",
            "root.subscription-refresh-pending && !root.reduced-motion",
        ] {
            assert!(
                refresh_action.contains(unchanged),
                "refresh action moved: {unchanged}"
            );
        }

        // Provider notices open from the bell as a popover sized to their text.
        let announcement = ui
            .split("announcement-popover := Rectangle")
            .nth(1)
            .expect("announcement source");
        // Checkouts with CRLF line endings (and `@tr()` wrappers) make the source longer.
        let announcement = announcement.chars().take(8000).collect::<String>();
        assert!(announcement.contains("property <bool> shown: root.announcement-open"));
        assert!(announcement.contains("height: announcement-copy.preferred-height + 50px;"));
        assert!(announcement.contains("wrap: word-wrap;"));
        assert!(
            announcement.contains("accessible-description: root.subscription-announcement-text;")
        );
        assert!(ui.contains(
            "if root.subscription-announcement-text != \"\": announcement-bell := FocusScope"
        ));

        let routes_header = ui
            .split("inline-routes := Rectangle")
            .nth(1)
            // The heading row only: the search field that shares its slot follows it.
            .and_then(|source| source.split("route-search-toggle := FocusScope").next())
            .expect("routes header source");
        // The bare date ("29.09.2026") next to "Маршруты" read as unexplained.
        assert!(routes_header.contains(": @tr(\"Обновлено {}\", root.subscription-updated);"));
        // The separator dot is its own element so both of its gaps are equal.
        assert!(routes_header.contains("text: \"·\";"));
        assert!(routes_header.contains("root.subscription-updated-stale ? Theme.warning"));
        assert!(routes_header.contains("horizontal-alignment: left;"));
        assert!(!routes_header.contains("horizontal-alignment: right;"));
        assert!(!routes_header.contains("route-updated-divider := Rectangle"));
        assert!(!routes_header.contains("commands: \"M 7 1.5"));
        // Latency progress is shown in place of the timestamp, not as extra chrome.
        assert!(routes_header.contains("root.latency-check-loading ? @tr(\"Замеряем задержку…\")"));
        assert!(!routes_header.contains("catalog-selection-queued"));
        // Connection card lifts on hover and the mesh/rail decoration fades, not pops.
        assert!(ui.contains("drop-shadow-blur: strip-glow-touch.has-hover ? 20px : 0px;"));
        assert!(ui.contains("animate drop-shadow-blur, drop-shadow-offset-y, drop-shadow-color { duration: 180ms; easing: ease-out; }"));
        assert!(ui.contains("animate opacity { duration: 260ms; easing: ease-out; }"));
        assert!(ui.contains("animate opacity, background { duration: 260ms; easing: ease-out; }"));
        // Speedtest button beside search: same 32x36 hit area, accent ring only while loading.
        let speedtest = ui
            .split("latency-check-toggle := FocusScope")
            .nth(1)
            .and_then(|source| source.split("route-search-bar := Rectangle").next())
            .expect("speedtest button source");
        assert!(speedtest.contains("width: 32px;") && speedtest.contains("height: 36px;"));
        assert!(speedtest.contains("accessible-label: @tr(\"Проверить пинг\");"));
        assert!(speedtest.contains("../assets/gauge.svg"));
        assert!(speedtest.contains("if root.latency-check-loading: latency-check-ring := Path"));
        assert!(speedtest.contains("stroke: #4c9dff;"));
        assert!(speedtest.contains("root.check-latency()"));
        assert!(speedtest.contains("!root.reduced-motion"));
        assert!(rust.contains("ui.on_check_latency("));
        assert!(rust.contains("presentation.has_profile && !presentation.is_busy"));
        assert!(rust.contains("ui.set_subscription_management_enabled("));
    }

    #[test]
    fn route_preferences_flow_from_startup_model_to_atomic_persistence() {
        let mut tracker = PersistenceTracker::new(AppPreferences::default());
        let selections =
            std::collections::BTreeMap::from([("group-a".to_owned(), "node-b".to_owned())]);
        tracker.set_route_preferences(
            "profile-a".to_owned(),
            Some("group-a".to_owned()),
            selections.clone(),
        );
        assert_eq!(tracker.latest_desired().active_profile_hint, None);
        tracker.set_active_profile_hint(Some("profile-a".to_owned()));
        let saved = tracker.snapshot_for_flush(None);
        assert_eq!(saved.active_profile_hint.as_deref(), Some("profile-a"));
        assert_eq!(
            saved
                .last_group_by_profile
                .get("profile-a")
                .map(String::as_str),
            Some("group-a")
        );
        assert_eq!(
            saved.selections_by_profile.get("profile-a"),
            Some(&selections)
        );

        let source = include_str!("main.rs");
        assert!(source.contains("model.restore_route_preferences("));
        assert!(source.contains("set_active_profile_hint(active_id.clone())"));
        assert!(source.contains("tracker.set_route_preferences("));
    }

    #[test]
    fn authoritative_profile_hint_is_updated_independently_of_route_preferences() {
        let mut tracker = PersistenceTracker::new(AppPreferences {
            active_profile_hint: Some("old-profile".into()),
            ..AppPreferences::default()
        });
        tracker.set_route_preferences("new-profile".into(), None, Default::default());
        assert_eq!(
            tracker.latest_desired().active_profile_hint.as_deref(),
            Some("old-profile")
        );
        tracker.set_active_profile_hint(Some("daemon-profile".into()));
        assert_eq!(
            tracker.latest_desired().active_profile_hint.as_deref(),
            Some("daemon-profile")
        );
        tracker.set_active_profile_hint(None);
        assert_eq!(tracker.latest_desired().active_profile_hint, None);
    }

    #[test]
    fn route_hints_are_mutable_across_a_b_a_and_apply_only_while_ready() {
        let tracker = Arc::new(Mutex::new(PersistenceTracker::new(
            AppPreferences::default(),
        )));
        let mut reconciler = ProfilePreferenceReconciler::new(AppPreferences::default(), tracker);
        reconciler.record_hint(
            "profile-a".into(),
            Some("group-a".into()),
            std::collections::BTreeMap::from([("group-a".into(), "node-a".into())]),
        );
        reconciler.set_authoritative_profile(Some("profile-a".into()));
        assert_eq!(
            reconciler.pending_ready_profile.as_deref(),
            Some("profile-a")
        );
        reconciler.record_hint(
            "profile-b".into(),
            Some("group-b".into()),
            std::collections::BTreeMap::from([("group-b".into(), "node-b".into())]),
        );
        reconciler.set_authoritative_profile(Some("profile-b".into()));
        assert_eq!(
            reconciler.pending_ready_profile.as_deref(),
            Some("profile-b")
        );
        reconciler.set_authoritative_profile(Some("profile-a".into()));
        assert_eq!(
            reconciler.pending_ready_profile.as_deref(),
            Some("profile-a")
        );
        assert_eq!(
            reconciler
                .hints
                .last_group_by_profile
                .get("profile-a")
                .map(String::as_str),
            Some("group-a")
        );
        assert_eq!(
            reconciler
                .hints
                .last_group_by_profile
                .get("profile-b")
                .map(String::as_str),
            Some("group-b")
        );

        let ready = UiState::Ready {
            profile: "A".into(),
            node: "auto".into(),
        }
        .presentation();
        let connected = UiState::Connected {
            profile: "A".into(),
            node: "daemon".into(),
        }
        .presentation();
        assert!(route_hints_apply_to(&ready));
        assert!(!route_hints_apply_to(&connected));
    }

    #[test]
    fn failed_catalog_keeps_pending_profile_hints_for_a_successful_retry() {
        let tracker = Arc::new(Mutex::new(PersistenceTracker::new(
            AppPreferences::default(),
        )));
        let mut reconciler = ProfilePreferenceReconciler::new(AppPreferences::default(), tracker);
        reconciler.record_hint(
            "profile-a".into(),
            Some("group-a".into()),
            std::collections::BTreeMap::from([("group-a".into(), "node-a-saved".into())]),
        );
        reconciler.set_authoritative_profile(Some("profile-a".into()));

        let mut model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let status = model.begin_refresh();
        model.finish_refresh(
            status,
            Ok(DaemonStatus {
                state: "ready".into(),
                active_profile_id: Some("profile-a".into()),
                profile: Some("Profile A".into()),
                ..DaemonStatus::default()
            }),
        );

        let failed = model
            .begin_catalog_request()
            .expect("failed catalog request");
        model.finish_catalog(failed.token, Err(DaemonError::new("temporary failure")));
        reconciler.apply_ready_hints_after_catalog(&mut model, false);
        assert_eq!(
            reconciler.pending_ready_profile.as_deref(),
            Some("profile-a"),
            "a transient catalog error must not consume the profile hint"
        );

        let retry = model.begin_catalog_request().expect("catalog retry");
        model.finish_catalog(
            retry.token,
            Ok(DaemonCatalog {
                revision: 7,
                groups: vec![DaemonCatalogGroup {
                    id: "group-a".into(),
                    label: "Primary".into(),
                    selected: true,
                    icon_id: None,
                    icon_pending: false,
                    nodes: vec![
                        DaemonCatalogNode {
                            id: "node-a-default".into(),
                            label: "Default".into(),
                            selected: true,
                            delay_ms: None,
                            icon_id: None,
                            icon_pending: false,
                        },
                        DaemonCatalogNode {
                            id: "node-a-saved".into(),
                            label: "Saved".into(),
                            selected: false,
                            delay_ms: None,
                            icon_id: None,
                            icon_pending: false,
                        },
                    ],
                }],
            }),
        );
        reconciler.apply_ready_hints_after_catalog(&mut model, true);

        assert_eq!(reconciler.pending_ready_profile, None);
        let (_, selected_group, selections) = model
            .route_preferences()
            .expect("restored route preferences");
        let group_key = selected_group.unwrap();
        assert!(group_key.starts_with("label-sha256:"));
        assert_eq!(
            selections.get("group-a").map(String::as_str),
            Some("node-a-saved")
        );
    }

    #[test]
    fn settings_inline_profile_tasks_consume_escape_before_panel_close() {
        let source = include_str!("../ui/app.slint");
        let escape_handler = source
            .split("visual-frame := FocusScope {")
            .nth(1)
            .and_then(|source| source.split("VerticalLayout {").next())
            .expect("visual frame Escape handler");
        assert!(escape_handler.contains("if (root.profile-delete-confirm-id != \"\")"));
        assert!(escape_handler.contains("root.profile-delete-confirm-id = \"\";"));
        assert!(escape_handler.contains("else if (root.profile-edit-id != \"\")"));
        assert!(escape_handler.contains("root.profile-edit-id = \"\";"));
        assert!(escape_handler.contains("root.profile-edit-draft = \"\";"));
        assert!(escape_handler.contains("else { root.escape-pressed(); }"));
    }

    #[test]
    fn delete_focus_plan_waits_for_completion_and_covers_every_outcome() {
        assert_eq!(
            delete_focus_plan(&["profile-a", "profile-b"], "profile-a", false),
            DeleteFocusState::Immediate(DeleteFocusTarget::DeleteControl("profile-a".into())),
            "preflight rejection closes confirmation and restores its delete control"
        );

        let DeleteFocusState::AwaitingCompletion(middle) =
            delete_focus_plan(&["profile-a", "profile-b", "profile-c"], "profile-b", true)
        else {
            panic!("accepted middle-row deletion must wait for completion");
        };
        assert_eq!(
            middle.completion(true),
            DeleteFocusTarget::ManagementProfile("profile-c".into())
        );
        assert_eq!(
            middle.completion(false),
            DeleteFocusTarget::DeleteControl("profile-b".into())
        );

        let DeleteFocusState::AwaitingCompletion(last) =
            delete_focus_plan(&["profile-a", "profile-b"], "profile-b", true)
        else {
            panic!("accepted last-row deletion must wait for completion");
        };
        assert_eq!(
            last.completion(true),
            DeleteFocusTarget::ManagementProfile("profile-a".into())
        );

        let DeleteFocusState::AwaitingCompletion(single) =
            delete_focus_plan(&["profile-a"], "profile-a", true)
        else {
            panic!("accepted single-row deletion must wait for completion");
        };
        assert_eq!(single.completion(true), DeleteFocusTarget::AddSubscription);
    }

    #[test]
    fn profile_delete_ui_defers_management_focus_until_completion() {
        let ui = include_str!("../ui/app.slint");
        let main = include_str!("main.rs");

        for required in [
            "in-out property <string> profile-management-focus-id;",
            "in-out property <int> profile-management-focus-serial;",
            "in-out property <string> profile-delete-control-focus-id;",
            "in-out property <int> profile-delete-control-focus-serial;",
            "root.profile-management-focus-id == profile.id",
            "root.profile-delete-control-focus-id == profile.id",
            "focus-epoch: root.profile-management-focus-id == \"\" ? root.profile-management-focus-serial : 0;",
        ] {
            assert!(
                ui.contains(required),
                "missing delete focus contract: {required}"
            );
        }
        let delete_worker = main
            .split("fn run_profile_delete(")
            .nth(1)
            .and_then(|source| source.split("fn load_profile_list(").next())
            .expect("delete worker");
        assert!(delete_worker.contains("let completion = focus_plan.completion(succeeded);"));
        assert!(
            delete_worker
                .find("apply_snapshot(&ui, &current);")
                .expect("authoritative snapshot application")
                < delete_worker
                    .find("apply_delete_focus_completion(&ui, completion);")
                    .expect("focus completion after snapshot")
        );
        let delete_wiring = main
            .split("ui.on_delete_profile(move |profile_id| {")
            .nth(1)
            .and_then(|source| source.split("fn wire_primary_action(").next())
            .expect("delete callback wiring");
        assert!(delete_wiring.contains("DeleteFocusState::Immediate(completion)"));
        assert!(
            delete_wiring
                .find("apply_snapshot(&ui, &current);")
                .expect("preflight rejection snapshot")
                < delete_wiring
                    .find("apply_delete_focus_completion(&ui, completion.clone());")
                    .expect("preflight rejection focus restoration")
        );
        assert!(main.contains("ui.set_profile_delete_confirm_id(\"\".into());"));
        assert!(ui.contains("root.profile-delete-control-focus-id = profile.id;"));
        assert!(ui.contains("root.profile-delete-control-focus-serial += 1;"));
        assert!(
            !ui.contains("root.profile-delete-confirm-id = \"\"; root.delete-profile(profile.id);")
        );
    }

    #[test]
    fn pointer_closed_selector_does_not_restore_keyboard_focus_ring() {
        let ui = include_str!("../ui/app.slint");
        let panel_change = ui
            .split("changed active-panel => {")
            .nth(1)
            .and_then(|source| source.split("visual-frame := FocusScope {").next())
            .expect("active panel change handler");

        assert!(
            panel_change
                .contains("if (root.profile-menu-was-open && root.profile-menu-keyboard-open)")
        );
        assert!(!panel_change.contains(
            "if (root.profile-menu-was-open) {\n                root.profile-menu-was-open = false;\n                root.profile-shelf-focus-serial += 1;"
        ));
    }

    #[test]
    fn compact_profile_switcher_source_contract() {
        let ui = include_str!("../ui/app.slint");
        let main = include_str!("main.rs");
        let fixture = include_str!("../../../scripts/preview-fixture.ps1");
        let capture = include_str!("../../../scripts/capture-preview.ps1");

        for required in [
            "export struct UiSubscriptionProfile",
            "root.active-panel == \"subscription\"",
            "shelf-open-focus := FocusScope",
            "callback activate-profile(string);",
            "callback refresh-profile(string);",
            "callback replace-profile-source(string, string);",
            "callback delete-profile(string);",
            "callback add-subscription();",
            "callback retry-profile-menu();",
            "profile-menu := Rectangle",
            "profile-scroll := OverlayScroll",
            "Подписок пока нет",
            "Удалить подписку?",
            "До 100 подписок · активна только одна",
            "Изменить URL",
            "root.settings-section == \"updates\"",
            "profile-menu-keyboard-open",
            "parent.has-focus && root.profile-menu-keyboard-open",
            "accessible-label: @tr(\"Выбрать профиль подписки\")",
            "Загружаем профили…",
            "Повторить загрузку профилей",
            "@image-url(\"../assets/refresh.svg\")",
            "@image-url(\"../assets/trash.svg\")",
        ] {
            assert!(
                ui.contains(required),
                "missing profile UI contract: {required}"
            );
        }
        assert!(!ui.contains("label: \"Профили\";"));

        // The switcher is the title-bar chip: transparent at rest, tinted on hover/open.
        assert!(!ui.contains("profile-menu-reserve"));
        let switcher = ui
            .split("subscription-chip := Rectangle")
            .nth(1)
            .and_then(|source| {
                source
                    .split("subscription-refresh-action := FocusScope")
                    .next()
            })
            .expect("title subscription switcher source");
        // Variant C: a quiet chip (no pill) with the traffic ring, the logo in its
        // spot during the refresh motion, and the compact quota after the name.
        assert!(switcher.contains(
            "background: root.profile-menu-open ? #ffffff14 : chip-touch.has-hover ? #ffffff0d : transparent;"
        ));
        assert!(switcher.contains("border-radius: 6px;"));
        assert!(switcher.contains("chip-ring := QuotaRing"));
        assert!(switcher.contains("chip-spinner := Path"));
        assert!(switcher.contains("chip-logo := QuotaLogo"));
        assert!(switcher.contains("letter: root.subscription-initial;"));
        assert!(switcher.contains("text: root.chip-meta-text;"));
        assert!(
            ui.contains("\"· \" + root.subscription-remaining + \" · \" + root.subscription-days")
        );
        assert!(!switcher.contains("chip-divider := Rectangle"));
        assert!(switcher.contains("function toggle-menu(keyboard: bool)"));
        assert!(switcher.contains("root.profile-menu-keyboard-open = keyboard;"));

        let popover = ui
            .split("profile-menu := Rectangle")
            .nth(1)
            .and_then(|source| source.split("// A taskbar restore").next())
            .expect("animated profile popover source");
        // The menu hangs under the title chip, left-aligned with it.
        for required in [
            "x: root.navigation-collapsed ? -8px : 8px;",
            "width: Math.min(320px, parent.width - 16px);",
            "height: Math.min(parent.height - 8px, root.profile-menu-content-height);",
            "opacity: root.profile-menu-open ? 1 : 0;",
            "y: root.profile-menu-open ? -2px : -8px;",
            "text: @tr(\"МОИ ПОДПИСКИ\");",
            "text: @tr(\"ИСТЕКЛА\");",
            "menu-add := FocusScope",
            "menu-all := FocusScope",
            "root.add-subscription();",
            "root.local-page = \"subscriptions\";",
            "animate opacity { duration: 160ms; easing: ease-out; }",
            "animate y { duration: 160ms; easing: ease-out; }",
        ] {
            assert!(
                popover.contains(required),
                "missing polished popover contract: {required}"
            );
        }
        assert!(!ui.contains("profile-menu-shadow := Rectangle"));
        assert!(!popover.contains("profile.active ? #4c9dff0c"));
        assert!(!ui.contains("if root.profile-menu-open: profile-menu := Rectangle"));
        assert!(!ui.contains("drop-shadow-blur: 18px"));
        assert!(!ui.contains("text: \"Обн.\""));
        assert!(!ui.contains("text: \"×\""));
        assert!(!ui.contains("home-banner-offset"));
        assert!(ui.contains("Math.min(328px, 8px + root.subscription-profiles.length * 40px)"));
        assert!(!ui.contains("text: \"Профили\""));
        assert!(main.contains("model.finish_profile_list(token, result)"));
        assert!(main.contains("model.finish_profile_activation(token, result)"));
        assert!(main.contains("model.finish_profile_target_refresh(token, result)"));
        assert!(main.contains("model.finish_profile_delete(token, result)"));
        assert!(main.contains("slint::invoke_from_event_loop"));
        assert!(main.contains("set_active_profile_hint(active_id.clone())"));
        assert!(main.contains("capture_current(&model)"));
        assert!(main.contains("apply_ready_hints_after_catalog(&mut model)"));
        for route in [
            "GET\" -and $path -eq \"/v1/subscriptions",
            "PUT\" -and $path -match '^/v1/subscriptions/([^/]+)/activate$'",
            "POST\" -and $path -match '^/v1/subscriptions/([^/]+)/refresh$'",
            "PUT\" -and $path -match '^/v1/subscriptions/([^/]+)/source$'",
            "DELETE\" -and $path -match '^/v1/subscriptions/([^/]+)$'",
        ] {
            assert!(fixture.contains(route), "missing fixture route: {route}");
        }
        assert!(capture.contains("Click-Preview $handle 276 203"));
        assert!(capture.contains("if ($EditProfile)"));
        assert!(capture.contains("($Width - 92) 224"));
        assert!(!capture.contains("if ($ExpandProfile)"));
        assert!(capture.contains("profile flow replaced the navigation rail"));
    }

    #[test]
    fn final_event_loop_exit_flushes_immediate_ambient_changes() {
        let mut tracker = PersistenceTracker::new(AppPreferences::default());
        tracker.set_ambient_background(false);
        let final_snapshot = tracker.snapshot_for_flush(None);
        assert!(!final_snapshot.ambient_background);

        let source = include_str!("main.rs");
        let shutdown = source
            .split("let result = event_loop_guard::run_until_quit(")
            .nth(1)
            .and_then(|source| source.split("fn activation_for_launch_arguments").next())
            .expect("common post-event-loop shutdown");
        assert!(
            shutdown.contains(
                "flush_preferences(&ui, &model, &preference_writer, &preference_tracker);"
            )
        );
        assert!(shutdown.find("flush_preferences").unwrap() < shutdown.find("ui.hide()").unwrap());
        assert!(!shutdown.contains(".stop();"));

        let updater_exit = source
            .split("fn wire_updater(")
            .nth(1)
            .and_then(|source| source.split("fn run_update_check").next())
            .expect("updater wiring");
        assert!(updater_exit.contains("event_loop_guard::request_quit()"));
    }

    #[test]
    fn a_service_install_is_updated_by_the_service() {
        assert_eq!(update_through_service(true), cfg!(windows));
        assert!(!update_through_service(false));
        let rust = include_str!("main.rs");
        let install = rust
            .split("fn start_update_install(")
            .nth(1)
            .and_then(|source| source.split("\nfn ").next())
            .expect("update install");
        for required in [
            "model.service_mode()",
            "update_through_service(service_mode)",
            "stage_through_service(client, &release, report)",
            "updater::download_and_verify(&release, report)",
            "begin_update_download(&state)",
        ] {
            assert!(
                install.replace("\r\n", "\n").contains(required),
                "missing: {required}"
            );
        }
        // The service stages the release while its progress is polled on the side.
        let stage = rust
            .split("fn stage_through_service(")
            .nth(1)
            .and_then(|source| source.split("\nfn ").next())
            .expect("service staging");
        assert!(stage.contains(".stage_update_via_service("));
        assert!(stage.contains(".service_update_progress()"));
        assert!(stage.contains("done.store(true"));
        assert!(!stage.contains(".update_via_service("));
    }

    #[test]
    fn service_progress_answers_become_stages_and_an_idle_service_says_nothing() {
        use super::{UpdateStage, service_update_stage};
        use crate::daemon::{DaemonUpdateProgress, DaemonUpdateStage};
        use multicore_release::DownloadProgress;
        let answer = |stage, received, total| DaemonUpdateProgress {
            stage,
            received,
            total,
        };
        assert_eq!(
            service_update_stage(answer(DaemonUpdateStage::Idle, 0, 0), 52),
            None
        );
        assert_eq!(
            service_update_stage(answer(DaemonUpdateStage::Downloading, 18, 52), 99),
            Some(UpdateStage::Downloading(DownloadProgress {
                received: 18,
                total: 52
            }))
        );
        // Still fetching the release: its size is the one this client was offered.
        assert_eq!(
            service_update_stage(answer(DaemonUpdateStage::Downloading, 0, 0), 52),
            Some(UpdateStage::Downloading(DownloadProgress {
                received: 0,
                total: 52
            }))
        );
        assert_eq!(
            service_update_stage(answer(DaemonUpdateStage::Verifying, 0, 0), 52),
            Some(UpdateStage::Verifying)
        );
    }

    #[test]
    fn the_restart_is_shown_and_remembered_before_the_app_quits_for_the_updater() {
        let rust = include_str!("main.rs").replace("\r\n", "\n");
        let finish = rust
            .split("fn finish_update_install(")
            .nth(1)
            .and_then(|source| source.split("\nfn ").next())
            .expect("finish_update_install");
        let restarting = finish
            .find("advance_update(&weak, &state, UpdateStage::Restarting)")
            .expect("«Перезапуск…» is shown");
        let remembered = finish
            .find("updater::remember_restart_into(&release.version())")
            .expect("the next start can confirm the update");
        let quit = finish
            .find("event_loop_guard::request_quit()")
            .expect("the app quits");
        assert!(restarting < remembered && remembered < quit);
        assert!(finish.contains("Timer::single_shot(RESTART_NOTICE"));
        assert!(super::RESTART_NOTICE <= Duration::from_secs(1));
        // A ready update moves on to «Устанавливаем…» before it is applied.
        let install = rust
            .split("fn start_update_install(")
            .nth(1)
            .and_then(|source| source.split("\nfn ").next())
            .expect("update install");
        assert!(
            install.find("UpdateStage::Installing").unwrap()
                < install.find("continue_ready_update(").unwrap()
        );
        assert!(rust.contains("updater::take_update_confirmation()"));
    }

    #[test]
    fn a_connected_update_downloads_before_anything_is_disconnected() {
        let rust = include_str!("main.rs").replace("\r\n", "\n");
        let body = |name: &str| {
            rust.split(&format!("fn {name}("))
                .nth(1)
                .and_then(|source| source.split("\nfn ").next())
                .unwrap_or_else(|| panic!("missing fn {name}"))
                .to_owned()
        };
        // Confirming while connected only starts the download.
        let confirm = rust
            .split("ui.on_confirm_update_restart(move || {")
            .nth(1)
            .and_then(|source| source.split("fn update_check_due").next())
            .expect("confirm handler");
        assert!(confirm.contains("start_update_install("));
        assert!(!confirm.contains("run_disconnect_for_update"));
        assert!(!confirm.contains("begin_primary_action"));

        // The download (service: stage only; per user: download and verify) runs first, with
        // the tunnel up; only a ready update ever reaches the disconnect.
        let download = body("start_update_install");
        for required in [
            "begin_update_download(&state)",
            "stage_through_service(",
            "updater::download_and_verify(&release, report)",
            "continue_ready_update(",
            "update_install_failed(",
        ] {
            assert!(download.contains(required), "missing: {required}");
        }
        for forbidden in [
            "run_disconnect",
            "begin_primary_action",
            "start_apply",
            ".update_via_service(",
        ] {
            assert!(
                !download.contains(forbidden),
                "download must not {forbidden}"
            );
        }

        let continuation = body("continue_ready_update");
        let step = continuation
            .find("ready_update_step(")
            .expect("plan evaluated");
        let begin = continuation
            .find("model.begin_primary_action()")
            .expect("disconnect start");
        assert!(
            step < begin,
            "begin_primary_action must be guarded by the step"
        );
        assert_eq!(
            continuation.matches("model.begin_primary_action()").count(),
            1
        );
        assert!(continuation.contains("run_disconnect_for_update("));
        assert!(continuation.contains(
            "ReadyUpdateStep::Disconnect,\n            Some((PrimaryAction::Disconnect | PrimaryAction::Cleanup, mutation)),"
        ));
        assert!(continuation.contains("apply_ready_update("));

        let apply = body("apply_ready_update");
        assert!(apply.contains("updater::start_apply(&release, &staged)"));
        assert!(apply.contains(".update_via_service("));
        assert!(!apply.contains("download_and_verify"));
        assert!(!apply.contains("stage_update_via_service"));
    }

    #[test]
    fn a_ready_update_applies_when_idle_and_disconnects_only_once() {
        assert_eq!(
            ready_update_step(UpdateConfirmPlan::InstallNow, false, 0),
            ReadyUpdateStep::Apply
        );
        // The user disconnected by hand during the download: nothing left to stop.
        assert_eq!(
            ready_update_step(UpdateConfirmPlan::InstallNow, true, 3),
            ReadyUpdateStep::Apply
        );
        assert_eq!(
            ready_update_step(UpdateConfirmPlan::DisconnectFirst, false, 0),
            ReadyUpdateStep::Disconnect
        );
        // Still connected after our disconnect (it failed, or the user connected again):
        // stay connected and report it instead of fighting the user.
        assert_eq!(
            ready_update_step(UpdateConfirmPlan::DisconnectFirst, true, 0),
            ReadyUpdateStep::GiveUp
        );
        // Connecting, switching or reconciling: wait for it to settle, but not forever.
        assert_eq!(
            ready_update_step(UpdateConfirmPlan::Wait, false, 0),
            ReadyUpdateStep::WaitAgain
        );
        assert_eq!(
            ready_update_step(UpdateConfirmPlan::Wait, true, MAX_READY_UPDATE_WAITS - 1),
            ReadyUpdateStep::WaitAgain
        );
        assert_eq!(
            ready_update_step(UpdateConfirmPlan::Wait, false, MAX_READY_UPDATE_WAITS),
            ReadyUpdateStep::GiveUp
        );
    }

    #[test]
    fn a_second_confirm_never_starts_a_second_download() {
        use crate::updater::UpdateState;
        use multicore_release::{GitHubRelease, WindowsArch};
        let release = GitHubRelease::parse_for_arch(br#"{
            "tag_name":"v9.0.0",
            "html_url":"https://github.com/Fatyzzz/multi-core/releases/tag/v9.0.0",
            "assets":[{
                "name":"multicore-windows-x64.zip",
                "browser_download_url":"https://github.com/Fatyzzz/multi-core/releases/download/v9.0.0/multicore-windows-x64.zip",
                "size":1234,
                "digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            }]
        }"#, WindowsArch::X64)
        .unwrap();
        let state = Mutex::new(UpdateState::Available(release.clone()));
        assert!(begin_update_download(&state).is_some());
        assert!(matches!(*state.lock().unwrap(), UpdateState::Updating(..)));
        assert!(
            begin_update_download(&state).is_none(),
            "already downloading"
        );
        // A failed attempt can be retried.
        *state.lock().unwrap() = UpdateState::InstallFailed(release, "x".into());
        assert!(begin_update_download(&state).is_some());
        *state.lock().unwrap() = UpdateState::Current;
        assert!(begin_update_download(&state).is_none());
    }

    #[test]
    fn connected_update_uses_confirmation_and_managed_disconnect() {
        assert_eq!(
            update_install_intent(false),
            UpdateInstallIntent::ConfirmRestart
        );
        assert_eq!(update_install_intent(true), UpdateInstallIntent::InstallNow);

        let rust = include_str!("main.rs");
        let updater = rust
            .split("fn wire_updater(")
            .nth(1)
            .and_then(|source| source.split("fn run_update_check").next())
            .expect("updater wiring");
        for required in [
            "ui.on_confirm_update_restart",
            "ui.on_cancel_update_restart",
            "run_disconnect_for_update",
            "start_update_install",
        ] {
            assert!(
                updater.contains(required),
                "missing updater flow: {required}"
            );
        }

        let ui = include_str!("../ui/app.slint");
        for required in [
            "in-out property <bool> update-confirm-open",
            "callback confirm-update-restart()",
            "callback cancel-update-restart()",
            "Скачаем обновление, не отключая VPN.",
            "Туннель остановится только после загрузки.",
            "label: @tr(\"Обновить и перезапустить\")",
            "label: @tr(\"Отмена\")",
            "update-modal-focus := FocusScope",
            "focus-epoch: root.update-confirm-focus-serial;",
        ] {
            assert!(
                ui.contains(required),
                "missing update confirmation UI: {required}"
            );
        }
    }

    #[test]
    fn final_flush_can_replace_an_earlier_tray_flush_after_writer_shutdown() {
        let root = tempfile::tempdir().unwrap();
        let store = PreferenceStore::at(root.path().join("preferences.json"));
        let mut writer = PreferenceWriter::start(Some(store.clone()));
        let tray_snapshot = AppPreferences::default();
        assert!(writer.stop_and_flush(&tray_snapshot).is_ok());

        let final_snapshot = AppPreferences {
            ambient_background: false,
            visible_page: VisiblePage::Settings,
            ..tray_snapshot
        };
        assert!(writer.stop_and_flush(&final_snapshot).is_ok());
        assert_eq!(store.load().unwrap(), final_snapshot);
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
    #[cfg(windows)]
    fn windows_creates_the_tray_popup_on_demand_and_drops_it_when_left_closed() {
        let source = include_str!("main.rs");
        let main_entry = source
            .split("fn main()")
            .nth(1)
            .and_then(|source| source.split("fn activation_for_launch_arguments(").next())
            .expect("desktop main entrypoint");
        let compact_main = main_entry.split_whitespace().collect::<String>();
        assert!(compact_main.contains("#[cfg(not(windows))]tray_popup_slot.get_or_create()?;"));
        assert!(compact_main.contains(
            "#[cfg(windows)]let_tray_popup_release_timer=wire_tray_popup_release(tray_popup_slot.clone());"
        ));
        let release = source
            .split("fn wire_tray_popup_release(")
            .nth(1)
            .and_then(|source| source.split("/// Size and resize state").next())
            .expect("popup release timer");
        assert!(release.contains("renderer_release::RELEASE_AFTER"));
        assert!(release.contains("slot.popup.borrow_mut().take()"));
        let toggle = source
            .split("fn toggle_tray_popup(")
            .nth(1)
            .and_then(|source| source.split("fn dispatch_tray_popup_command(").next())
            .expect("popup toggle");
        // A new popup is put in place before its window exists, then placed natively.
        let preplace = toggle
            .find("preplace_new_tray_popup(")
            .expect("pre-placement");
        let show = toggle.find("popup.show()").expect("show");
        assert!(preplace < show);
        assert!(toggle.contains("winit_window().await"));
        let renderer = include_str!("renderer_fallback.rs");
        assert_eq!(
            renderer
                .matches("with_winit_window_attributes_hook(crate::tray_popup_window_attributes)")
                .count(),
            2
        );
    }

    #[test]
    #[cfg(windows)]
    fn tray_hide_keeps_slint_window_registered_for_tray_events() {
        let source = include_str!("main.rs");
        let helper = source
            .split("fn hide_window_to_tray(")
            .nth(1)
            .and_then(|source| source.split("fn apply_tray_popup_model").next())
            .expect("tray hide helper");
        let helper = helper
            .split("#[cfg(not(target_os = \"linux\"))]")
            .nth(1)
            .expect("native-only hide branch");
        assert!(helper.contains("with_winit_window"));
        assert!(helper.contains("window.set_visible(false)"));
        assert!(!helper.contains("ui.hide()"));
    }

    #[test]
    fn page_names_round_trip_and_unknown_names_are_rejected() {
        for page in [
            VisiblePage::Home,
            VisiblePage::Status,
            VisiblePage::Subscriptions,
            VisiblePage::Settings,
        ] {
            assert_eq!(visible_page_from_name(visible_page_name(&page)), Some(page));
        }
        assert_eq!(visible_page_from_name("routes"), None);
    }

    #[test]
    fn subscriptions_are_a_persisted_top_level_page() {
        let ui = include_str!("../ui/app.slint");
        let rust = include_str!("main.rs");
        let preferences = include_str!("preferences.rs");

        for required in [
            "label: @tr(\"Подписки\"); badge: root.subscription-profiles.length > 1 ? \"\" + root.subscription-profiles.length : \"\"; active: root.visible-page == \"subscriptions\"",
            "if root.visible-page == \"subscriptions\"",
            "VisiblePage::Subscriptions => \"subscriptions\"",
            "\"subscriptions\" => Some(VisiblePage::Subscriptions)",
            "Subscriptions,",
            "if root.visible-page == \"settings\": Rectangle {",
            "if (root.settings-section == \"updates\") {",
        ] {
            assert!(
                ui.contains(required) || rust.contains(required) || preferences.contains(required),
                "missing top-level subscriptions contract: {required}"
            );
        }
        assert!(!ui.contains("SettingsNavItem { width: parent.width; label: \"Подписки\""));
        assert!(!rust.contains("set_settings_section(\"subscriptions\""));
    }

    #[test]
    fn subscription_rows_expose_metadata_and_actions_without_generic_expansion() {
        let ui = include_str!("../ui/app.slint");

        assert!(ui.contains("in-out property <string> profile-edit-id;"));
        assert!(ui.contains("in-out property <string> profile-edit-draft;"));
        assert!(ui.contains("root.replace-profile-source(profile.id, root.profile-edit-draft);"));
        // Expiry first: at the minimum width the elide must eat the usage, not the expiry.
        assert!(ui.contains("profile.expiry + \" · \" + profile.usage"));
        assert!(ui.contains("accessible-label: @tr(\"Изменить URL {}\", profile.title)"));
        assert!(ui.contains("@image-url(\"../assets/edit.svg\")"));
        let edit_control = ui
            .split("edit-profile-focus := FocusScope")
            .nth(1)
            .and_then(|source| source.split("delete-profile-focus := FocusScope").next())
            .expect("profile edit control source");
        assert!(!edit_control.contains("@image-url(\"../assets/link.svg\")"));
        assert!(!ui.contains("expanded-profile-id"));
        assert!(!ui.contains("Локальный источник"));
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
    fn transient_preference_failures_stay_silent_until_retries_are_exhausted() {
        assert!(!preference_failure_is_terminal(1, false));
        assert!(!preference_failure_is_terminal(2, false));
        assert!(preference_failure_is_terminal(3, false));
        assert!(preference_failure_is_terminal(1, true));
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
        let failure = PreferenceWriteFailure::from_error(
            &std::io::Error::from_raw_os_error(5),
            PREFERENCE_RETRY_DELAYS.len() + 1,
        );
        assert_eq!(
            preference_error_for_result(&WriterResult::Failed(failure)),
            preference_save_error()
        );
        assert_eq!(
            preference_error_for_result(&WriterResult::Saved(AppPreferences::default())),
            ""
        );
        assert!(!preference_save_error().is_ascii());
        assert!(preference_save_error().chars().count() < 64);
        assert!(!preference_save_error().contains("http"));
        assert!(!preference_save_error().contains(":\\"));
    }

    /// A core host that briefly holds the data directory sharing reads only (the service
    /// reading a staged generation) must not cost the exit flush its save; a hold that
    /// outlasts the retries is reported with the failed step and OS code for the log.
    #[cfg(windows)]
    #[test]
    fn exit_flush_rides_out_a_brief_directory_hold_and_reports_a_lasting_one() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ,
        };
        let hold = |path: &std::path::Path| {
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                .open(path)
                .unwrap()
        };
        let root = tempfile::tempdir().unwrap();
        let store = PreferenceStore::at(root.path().join("preferences.json"));
        store.save(&AppPreferences::default()).unwrap();
        let settings = AppPreferences {
            visible_page: VisiblePage::Settings,
            ..AppPreferences::default()
        };

        let held = hold(root.path());
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(held);
        });
        let mut writer = PreferenceWriter::start(Some(store.clone()));
        assert_eq!(writer.stop_and_flush(&settings), Ok(()));
        release.join().unwrap();
        assert_eq!(store.load().unwrap(), settings);

        let _held = hold(root.path());
        let mut writer = PreferenceWriter::start(Some(store.clone()));
        let failure = writer
            .stop_and_flush(&AppPreferences::default())
            .unwrap_err();
        assert_eq!(failure.raw_os_error, Some(32));
        assert_eq!(
            failure.attempts,
            super::PREFERENCE_FLUSH_RETRY_DELAYS.len() + 1
        );
        let line = failure.log_line();
        assert!(line.contains("replace preferences: "), "{line}");
        assert!(line.contains("os=Some(32)"), "{line}");
        assert!(!line.contains(&*root.path().to_string_lossy()), "{line}");
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
        // Wayland: one compositor move per press, then Slint's stuck grab is released.
        assert!(slint.contains("if (self.pressed && !self.drag-issued)"));
        assert!(rust.contains("WindowEvent::PointerExited"));
        assert!(
            rust.split("fn native_resize_and_persistence_source_contract")
                .next()
                .unwrap()
                .matches("release_pointer_grab_after_compositor_move(&ui);")
                .count()
                == 2,
            "both the move and the resize release the grab"
        );
        assert!(rust.contains("PreferenceStore::from_user_data_root()"));
        assert!(rust.contains("TimerMode::Repeated, Duration::from_millis(500)"));
        assert!(rust.contains("stop_and_flush"));
        let exit_branch = rust
            .split("TrayCommand::Exit => {")
            .nth(1)
            .and_then(|source| source.split("}").next())
            .expect("tray Exit branch");
        assert!(
            exit_branch.find("flush_preferences").unwrap()
                < exit_branch.find("request_quit").unwrap()
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
        // No tunnel is up in an error state; updating from it must work
        // directly (regression: it used to route through the restart
        // confirmation, whose "disconnect first" path did nothing here).
        assert!(runtime_allows_update(
            &UiState::Error {
                message: "x".into()
            }
            .presentation()
        ));
        assert!(runtime_allows_update(
            &UiState::ServiceUnreachable {
                message: "x".into()
            }
            .presentation()
        ));
        assert!(!runtime_allows_update(
            &UiState::DaemonDegraded {
                message: "x".into()
            }
            .presentation()
        ));
    }

    #[test]
    fn update_confirmation_only_ever_starts_an_install_or_a_disconnect() {
        let ready = UiState::Ready {
            profile: "profile".into(),
            node: "auto".into(),
        };
        let connected = UiState::Connected {
            profile: "profile".into(),
            node: "node".into(),
        };
        let connecting = UiState::Connecting {
            profile: "profile".into(),
            node: "node".into(),
            step: "x".into(),
        };
        let disconnecting = UiState::Disconnecting {
            profile: "profile".into(),
            node: "node".into(),
        };
        let error = UiState::Error {
            message: "x".into(),
        };
        let degraded = UiState::DaemonDegraded {
            message: "x".into(),
        };
        assert_eq!(
            update_confirm_plan(&ready.presentation()),
            UpdateConfirmPlan::InstallNow
        );
        assert_eq!(
            update_confirm_plan(&error.presentation()),
            UpdateConfirmPlan::InstallNow
        );
        assert_eq!(
            update_confirm_plan(&connected.presentation()),
            UpdateConfirmPlan::DisconnectFirst
        );
        assert_eq!(
            update_confirm_plan(&degraded.presentation()),
            UpdateConfirmPlan::DisconnectFirst
        );
        assert_eq!(
            update_confirm_plan(&connecting.presentation()),
            UpdateConfirmPlan::Wait
        );
        assert_eq!(
            update_confirm_plan(&disconnecting.presentation()),
            UpdateConfirmPlan::Wait
        );

        // A Retry/Connect primary action must never be started for an update (nothing
        // completes it, which used to leave the main button dead): the only
        // begin_primary_action of the update flow is guarded by `ready_update_step`, see
        // `a_connected_update_downloads_before_anything_is_disconnected`.
        let rust = include_str!("main.rs");

        let ui = include_str!("../ui/app.slint");
        assert!(ui.contains("in property <bool> update-runtime-busy"));
        assert!(ui.contains("enabled: !root.update-runtime-busy;"));
        assert!(ui.contains("(root.update-action-label == \"\" || !root.update-runtime-busy)"));
        assert!(ui.contains("!(root.update-runtime-busy && root.update-available)"));
        assert!(rust.contains("ui.set_update_runtime_busy(presentation.is_busy);"));
    }

    #[test]
    fn install_failures_keep_the_release_and_startup_reads_the_helper_result() {
        let rust = include_str!("main.rs");
        assert!(rust.contains("*current = UpdateState::InstallFailed(release, error);"));
        assert!(rust.contains("updater::with_pending_install_failure("));
        let startup = rust
            .split("fn main()")
            .nth(1)
            .and_then(|source| source.split("run_refresh(").next())
            .expect("startup");
        let load = startup
            .find("updater::load_install_result();")
            .expect("startup reads helper result");
        let check = startup.find("run_update_check(").expect("startup check");
        assert!(
            load < check,
            "the helper result must be loaded before the first check"
        );
    }

    #[test]
    fn background_launch_and_restore_source_contract() {
        let source = include_str!("main.rs");
        let launch = source
            .split("fn main()")
            .nth(1)
            .and_then(|source| source.split("fn activation_for_launch_arguments").next())
            .expect("desktop startup body");
        assert!(source.contains("--background"));
        assert!(source.contains("LaunchMode::Background"));
        assert!(launch.contains("event_loop_guard::run_until_quit("));
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
    fn installer_uri_accepts_windows_shell_root_path_and_scheme_host_case() {
        for route in [
            "multicore://install-sub",
            "multicore://install-sub/",
            "MULTICORE://INSTALL-SUB/",
        ] {
            let uri =
                format!("{route}?url=https%3A%2F%2Fexample.com%2Fsub%3Ftoken%3Da%252Fb%26next%3D1");
            let parsed = parse_launch_arguments([uri.into()]).unwrap();
            assert_eq!(parsed.mode, LaunchMode::Foreground);
            assert_eq!(
                parsed.subscription_url.as_deref(),
                Some("https://example.com/sub?token=a%2Fb&next=1"),
                "{route}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn installer_uri_received_from_real_windows_shell_preserves_subscription() {
        use std::os::windows::process::CommandExt;

        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/fixtures/capture-windows-shell-uri.ps1");
        let output = std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(fixture)
            .creation_flags(0x08000000)
            .output()
            .expect("run Windows ShellExecute fixture");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        // The fixture owns a unique scheme so concurrent tests never replace the user's handler.
        // Only its scheme name differs from the production ShellExecute invocation.
        let received = receipt["received"].as_str().unwrap();
        let scheme = receipt["scheme"].as_str().unwrap();
        let suffix = received.strip_prefix(scheme).unwrap();
        let parsed = parse_launch_arguments([format!("multicore{suffix}").into()])
            .expect("the URI actually delivered by Windows must be accepted");
        assert_eq!(
            parsed.subscription_url.as_deref(),
            Some("https://example.com/sub?token=a%2Fb&next=1")
        );
    }

    #[test]
    fn add_uri_takes_a_raw_subscription_link_like_panel_pages_send() {
        for (argument, expected) in [
            (
                "multicore://add/https://sub.example.com/api/sub/AbC123",
                "https://sub.example.com/api/sub/AbC123",
            ),
            (
                "MULTICORE://ADD/https://sub.example.com/sub?token=a%2Fb&next=1",
                "https://sub.example.com/sub?token=a%2Fb&next=1",
            ),
            (
                "multicore://add/https%3A%2F%2Fsub.example.com%2Fsub",
                "https://sub.example.com/sub",
            ),
        ] {
            let parsed = parse_launch_arguments([argument.into()]).unwrap();
            assert_eq!(parsed.mode, LaunchMode::Foreground);
            assert_eq!(
                parsed.subscription_url.as_deref(),
                Some(expected),
                "{argument}"
            );
        }
    }

    #[test]
    fn add_uri_rejects_non_http_fragments_and_blank_targets() {
        for argument in [
            "multicore://add/",
            "multicore://add/ftp://example.com/sub",
            "multicore://add/https://example.com/sub#frag",
            "multicore://add/https://",
            "multicore://add/https://example.com/a b",
            "multicore://add/javascript:alert(1)",
        ] {
            assert!(
                parse_launch_arguments([argument.into()]).is_err(),
                "{argument}"
            );
        }
    }

    #[test]
    fn installer_uri_rejects_extra_route_components_in_shell_form() {
        for argument in [
            "multicore://install-sub//?url=https%3A%2F%2Fexample.com",
            "multicore://install-sub/other?url=https%3A%2F%2Fexample.com",
            "multicore://user@install-sub/?url=https%3A%2F%2Fexample.com",
            "multicore://install-sub:80/?url=https%3A%2F%2Fexample.com",
            "multicore://install-sub/?url=https%3A%2F%2Fexample.com#fragment",
            "multicore://install-sub/?url=https%3A%2F%2Fexample.com&url=https%3A%2F%2Fother.example",
            "multicore://install-sub/?url=https://example.com/sub",
        ] {
            assert!(
                parse_launch_arguments([argument.into()]).is_err(),
                "{argument}"
            );
        }
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

    /// A press of the switch that background work holds is queued, not dropped, and a timer
    /// starts it through the same dispatch as a free press once the work ends.
    #[test]
    fn a_held_switch_press_is_queued_and_started_once_the_work_ends() {
        let source = include_str!("main.rs").replace("\r\n", "\n");
        let press = source
            .split("fn wire_primary_action(")
            .nth(1)
            .and_then(|source| source.split("fn run_queued_primary_action(").next())
            .expect("primary action wiring");
        assert!(press.contains(
            "let waiting = action.is_none() && model.queue_primary_action(Instant::now());"
        ));
        assert!(press.contains(
            "run_queued_primary_action(&queued_press, &weak, &model, &profile_preferences);"
        ));
        assert!(press.contains("dispatch_primary_action(&weak, &model, &profile_preferences"));
        let queued = source
            .split("fn run_queued_primary_action(")
            .nth(1)
            .and_then(|source| source.split("fn dispatch_primary_action(").next())
            .expect("queued primary action wiring");
        assert!(queued.contains("model.begin_queued_primary_action(Instant::now())"));
        assert!(queued.contains("timer.stop();"));
        assert!(queued.contains("dispatch_primary_action(&weak, &model, &profile_preferences"));
        assert!(source.contains("ui.set_primary_queueable(presentation.primary_queueable);"));
    }

    #[test]
    fn startup_auto_connect_requires_background_opt_in_ready_profile_and_network() {
        assert!(!startup_should_connect(
            LaunchMode::Foreground,
            true,
            true,
            true
        ));
        assert!(!startup_should_connect(
            LaunchMode::Background,
            false,
            true,
            true
        ));
        assert!(!startup_should_connect(
            LaunchMode::Background,
            true,
            false,
            true
        ));
        assert!(!startup_should_connect(
            LaunchMode::Background,
            true,
            true,
            false
        ));
        assert!(startup_should_connect(
            LaunchMode::Background,
            true,
            true,
            true
        ));
    }

    #[test]
    fn update_check_is_due_hourly_plus_jitter_but_not_during_an_update_operation() {
        let none = Duration::ZERO;
        let jitter = Duration::from_secs(7 * 60);
        assert!(!update_check_due(Duration::from_secs(3_599), none, false));
        assert!(update_check_due(Duration::from_secs(3_600), none, false));
        assert!(!update_check_due(Duration::from_secs(3_600), none, true));
        assert!(!update_check_due(
            Duration::from_secs(3_600 + 419),
            jitter,
            false
        ));
        assert!(update_check_due(
            Duration::from_secs(3_600 + 420),
            jitter,
            false
        ));
        // A jitter never pushes the check past 70 minutes.
        assert!(update_check_due(
            Duration::from_secs(4_200),
            Duration::from_secs(9_999),
            false
        ));
        for _ in 0..64 {
            assert!(super::update_check_jitter() <= super::UPDATE_CHECK_MAX_JITTER);
        }
    }

    #[test]
    fn home_update_banner_only_shows_available_releases_and_has_actions() {
        let ui = include_str!("../ui/app.slint").replace("\r\n", "\n");
        assert!(ui.contains(
            "root.update-available && root.update-toast-key != root.update-banner-dismissed-for"
        ));
        assert!(ui.contains("root.update-banner-dismissed-for = root.update-toast-key"));
        // A running update is only hidden (it goes on, and shows in Settings), never offered
        // again; after an update the same place confirms the new version once.
        assert!(ui.contains(
            "if update-toast.running: BannerAction {\n                                label: @tr(\"Скрыть\");"
        ));
        assert!(ui.contains("if root.update-in-progress: VerticalLayout {"));
        assert!(ui.contains("@tr(\"Обновлено до {}\", root.update-done-version)"));
        assert!(ui.contains("activated => { root.update-done-dismissed = true; }"));
        assert!(ui.contains("root.update-action();"));
    }

    #[test]
    fn installer_uri_rejects_controls_fragments_and_oversized_subscription_urls() {
        for argument in [
            "multicore://install-sub?url=https%3A%2F%2Fexample.com%2Fsub%0Apath".to_owned(),
            "multicore://install-sub?url=https%3A%2F%2Fexample.com%2Fsub%00path".to_owned(),
            "multicore://install-sub?url=https%3A%2F%2Fexample.com%2Fsub%23fragment".to_owned(),
            format!(
                "multicore://install-sub?url=https%3A%2F%2Fexample.com%2F{}",
                "a".repeat(crate::view_model::MAX_SUBSCRIPTION_URL_BYTES)
            ),
        ] {
            assert!(parse_launch_arguments([argument.into()]).is_err());
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

    #[cfg(windows)]
    #[test]
    fn installed_windows_protocol_is_repaired_before_single_instance_forwarding() {
        let source = include_str!("main.rs");
        let repair = source
            .find("windows_settings::ensure_url_protocol_registration()")
            .expect("installed URL protocol repair must run on startup");
        let forwarding = source
            .find("single_instance::claim_or_forward(&activation)")
            .expect("single-instance forwarding");
        assert!(repair < forwarding);
    }

    #[cfg(windows)]
    #[test]
    fn only_the_primary_instance_renames_the_legacy_brand() {
        let source = include_str!("main.rs");
        let forwarding = source
            .find("single_instance::claim_or_forward(&activation)")
            .expect("single-instance forwarding");
        let migration = source
            .find("windows_settings::migrate_legacy_brand();")
            .expect("installed copies rename the 1.1.1 entry and shortcuts on startup");
        assert!(forwarding < migration);
    }

    /// The expanded rail's label keeps everything left of its count: an English
    /// "Subscriptions" next to "3" used to be clipped by a fixed 24px reservation.
    #[test]
    fn rail_labels_take_the_width_left_of_their_count() {
        let ui = include_str!("../ui/app.slint");
        let item = ui
            .split("component NavItem inherits Rectangle {")
            .nth(1)
            .and_then(|body| body.split("\ncomponent ").next())
            .expect("NavItem component");
        assert!(item.contains("(root.badge == \"\" ? 0px : nav-badge.width + 8px)"));
        assert!(item.contains("width: root.badge == \"\" ? 0px : self.preferred-width;"));
        assert!(item.contains("overflow: elide;"));
        assert!(!item.contains("24px));"));
    }

    /// After an in-app update the primary instance puts the user's autostart value back in
    /// the plain spelling before the settings page first reads it.
    #[cfg(windows)]
    #[test]
    fn the_primary_instance_repairs_autostart_before_showing_it() {
        let source = include_str!("main.rs");
        let forwarding = source
            .find("single_instance::claim_or_forward(&activation)")
            .expect("single-instance forwarding");
        let repair = source
            .find("windows_settings::repair_launch_at_sign_in()")
            .expect("startup repairs the autostart value");
        let first_query = source
            .find("apply_launch_at_sign_in(&ui, windows_settings::query_launch_at_sign_in())")
            .expect("the settings page reads autostart on startup");
        assert!(forwarding < repair && repair < first_query);
    }

    #[test]
    fn a_subscription_switch_opens_the_new_notice_once() {
        let start = Instant::now();
        let mut notice = super::SwitchNotice::default();
        // Launch: the first profile is not a switch, even with a notice.
        assert!(!notice.observe(Some("a"), true, start));
        assert!(!notice.observe(Some("a"), true, start));
        // A switch to a subscription with a notice opens it once.
        assert!(notice.observe(Some("b"), true, start));
        assert!(!notice.observe(Some("b"), true, start));
        // A notice that arrives with a later snapshot (a refresh) still counts...
        assert!(!notice.observe(Some("a"), false, start));
        assert!(!notice.observe(None, false, start));
        assert!(notice.observe(Some("a"), true, start + Duration::from_secs(5)));
        // ...but not long after the switch.
        assert!(!notice.observe(Some("b"), false, start));
        assert!(!notice.observe(Some("b"), true, start + Duration::from_secs(31)));
        assert!(!notice.observe(Some("b"), true, start + Duration::from_secs(32)));
    }

    #[test]
    fn announcement_fingerprint_is_stable_and_ignores_blank_notices() {
        assert_eq!(super::announcement_fingerprint("   "), None);
        assert_eq!(super::announcement_fingerprint(""), None);
        // FNV-1a 64 of "a" (reference value).
        assert_eq!(
            super::announcement_fingerprint("a").as_deref(),
            Some("af63dc4c8601ec8c")
        );
        assert_eq!(
            super::announcement_fingerprint(" Работы в 23:00 "),
            super::announcement_fingerprint("Работы в 23:00")
        );
        assert_ne!(
            super::announcement_fingerprint("Работы в 23:00"),
            super::announcement_fingerprint("Работы в 22:00")
        );
    }

    #[test]
    fn home_states_use_ios_cards_and_a_catalog_retry() {
        let ui = include_str!("../ui/app.slint").replace("\r\n", "\n");
        let rust = include_str!("main.rs");
        const ENTER: &str = "cubic-bezier(0.22, 1, 0.36, 1)";
        // The conflict banner is a card with quiet text actions and a one-shot enter.
        let card = ui
            .split("conflict-card := Rectangle {")
            .nth(1)
            .expect("conflict card");
        let card = card.chars().take(900).collect::<String>();
        assert!(card.contains("background: Theme.surface;"));
        assert!(card.contains("border-radius: 14px;"));
        assert!(card.contains(&format!(
            "animate opacity {{ duration: 240ms; easing: {ENTER}; }}"
        )));
        // The update notice is a bottom-right toast that never pushes the routes.
        assert!(!ui.contains("update-banner := Rectangle"));
        let toast = ui
            .split("update-toast := Rectangle {")
            .nth(1)
            .expect("update toast");
        let toast = toast.chars().take(9000).collect::<String>();
        for required in [
            "width: Math.min(356px, parent.width - 32px);",
            "x: parent.width - self.width - 16px;",
            "y: parent.height - self.height - 16px + self.motion;",
            "property <length> motion: root.reduced-motion ? 0px",
            ": root.update-banner-entering ? 16px",
            "visible: self.opacity > 0;",
            "border-color: update-toast-hover.has-hover ? #3a414b : Theme.line;",
            "filled: true;",
            "label: root.update-action-label != \"\" ? root.update-action-label : @tr(\"Подробнее\");",
            "root.update-banner-dismissed-for = root.update-toast-key;",
            "UpdateProgressBar {",
        ] {
            assert!(toast.contains(required), "update toast missing: {required}");
        }
        assert!(toast.contains(&format!(
            "animate opacity {{ duration: self.shown ? 320ms : 200ms; easing: {ENTER}; }}"
        )));
        // Below the popovers in z-order: declared before the profile menu.
        assert!(
            ui.find("update-toast := Rectangle").unwrap()
                < ui.find("profile-menu := Rectangle").unwrap()
        );
        // The connect knob slides with a soft overshoot.
        assert!(
            ui.contains(
                "animate x { duration: 260ms; easing: cubic-bezier(0.34, 1.56, 0.64, 1); }"
            )
        );
        assert!(ui.contains("banner-enter := Timer {\n        interval: 16ms;"));
        assert!(ui.contains("if (root.update-toast-shown && !root.reduced-motion) {"));
        assert!(ui.contains("if (root.conflicting-apps-banner-visible && !root.reduced-motion) {"));
        assert!(ui.contains("component TextAction inherits Rectangle {"));
        // The conflict card grows with its message instead of a fixed worst case.
        assert!(ui.contains(
            "height: Math.max(72px, conflict-title.preferred-height + conflict-copy.preferred-height + 30px);"
        ));
        // A failed catalog is a card with a retry wired to the regular load.
        assert!(ui.contains("if root.catalog-error != \"\": catalog-error-card := Rectangle {"));
        assert!(ui.contains("activated => { root.retry-catalog(); }"));
        assert!(ui.contains("visible: root.catalog-error == \"\";"));
        assert!(rust.contains("ui.on_retry_catalog(move || {"));
        // Connection module tones: busy is blue, a failed connect reads its reason in red.
        assert!(ui.contains(": root.shown-busy ? #4c9dff59\n                            : root.shown-tone == \"success\" ? #3ccb7f59"));
        assert!(ui.contains("source: root.shown-busy ? @image-url(\"../assets/shield.svg\")"));
        assert!(
            ui.contains("color: root.shown-tone == \"danger\" ? #f0a3a3 : Theme.text-secondary;")
        );
        // A switch held by background work still takes a press (`primary-queueable`).
        assert!(ui.contains("opacity: root.primary-pressable || root.shown-busy ? 1 : 0.4;"));
        assert!(ui.contains(
            "private property <bool> primary-pressable: root.primary-enabled || root.primary-queueable;"
        ));
    }

    #[test]
    fn exit_line_leads_with_the_real_node_behind_an_automatic_group() {
        let app = include_str!("../ui/app.slint").replace("\r\n", "\n");
        let main = include_str!("main.rs").replace("\r\n", "\n");
        for property in [
            "in property <string> exit-now-name;",
            "in property <string> exit-now-icon;",
            "in property <image> exit-now-icon-image;",
        ] {
            assert!(app.contains(property), "{property}");
        }
        // "Group → real node": the selected group leads (muted when a real node
        // follows), an arrow points at the node the group is actually using.
        let group = app
            .find("text: root.node-name; color: root.exit-now-name != \"\" ? Theme.text-muted")
            .expect("group");
        let arrow = app
            .find("if root.exit-now-name != \"\": Text { text: \"→\";")
            .expect("arrow");
        let node = app
            .find("if root.exit-now-name != \"\": Text { horizontal-stretch: 0; max-width: 160px; text: root.exit-now-name;")
            .expect("real node");
        assert!(group < arrow && arrow < node);
        assert!(!app.contains("text: \"Выход\";"));
        // The name crossfades in on change via a one-frame flag, gated by reduced motion.
        assert!(app.contains("exit-now-enter := Timer {\n        interval: 16ms;\n        running: root.exit-now-entering;"));
        assert!(app.contains("if (root.exit-now-name != \"\" && !root.reduced-motion) {"));
        assert!(main.contains("exit_now_display(&line.exit_now, ui.get_node_name().as_str())"));
    }

    #[test]
    fn chip_avatar_uses_the_first_letter_of_the_subscription() {
        assert_eq!(super::subscription_initial("Fjord VPN"), "F");
        assert_eq!(super::subscription_initial("🌍 мой сервер"), "М");
        assert_eq!(super::subscription_initial("  "), "П");
    }

    #[test]
    fn mapping_flags_are_drawn_as_images_not_letters() {
        let (icon, name) = super::split_leading_emoji_image(" 🇦🇱 Albania");
        assert_eq!(name, "Albania");
        assert!(icon.size().width > 0);
        let (icon, name) = super::split_leading_emoji_image("Plain name");
        assert_eq!(name, "Plain name");
        assert_eq!(icon.size().width, 0);
    }

    #[test]
    fn toggle_reflects_every_connection_state() {
        use crate::view_model::UiState;

        let cases = [
            (
                UiState::Ready {
                    profile: "p".into(),
                    node: "n".into(),
                },
                "Защита выключена",
                false,
                false,
                true,
            ),
            (
                UiState::Connecting {
                    profile: "p".into(),
                    node: "n".into(),
                    step: "s".into(),
                },
                "Подключение…",
                true,
                true,
                true,
            ),
            (
                UiState::Connected {
                    profile: "p".into(),
                    node: "n".into(),
                },
                "Защита включена",
                true,
                false,
                true,
            ),
            (
                UiState::Disconnecting {
                    profile: "p".into(),
                    node: "n".into(),
                },
                "Отключение…",
                false,
                true,
                true,
            ),
            (
                UiState::Error {
                    message: "boom".into(),
                },
                "Не удалось подключиться",
                false,
                false,
                false,
            ),
            (
                UiState::ServiceUnreachable {
                    message: "m".into(),
                },
                "Нет связи с сервисом",
                false,
                false,
                false,
            ),
            (
                UiState::DaemonDegraded {
                    message: "m".into(),
                },
                "Сервис недоступен",
                false,
                false,
                false,
            ),
        ];
        for (state, title, on, busy, detail_is_exit) in cases {
            let toggle = super::toggle_presentation(&state.presentation());
            assert_eq!(
                toggle,
                super::TogglePresentation {
                    title: title.into(),
                    on,
                    busy,
                    detail_is_exit
                },
                "{state:?}"
            );
        }
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
    fn queued_installer_urls_wait_for_import_and_keep_arrival_order() {
        let mut pending = PendingSubscriptionImports::default();
        let now = Instant::now();
        assert!(pending.enqueue("https://example.com/first".into()));
        assert!(pending.enqueue("https://example.com/second".into()));
        assert!(!pending.dispatch_next(now, |_| false));
        assert_eq!(pending.len(), 2);

        let mut imported = Vec::new();
        assert!(pending.dispatch_next(now, |url| {
            imported.push(url.to_owned());
            true
        }));
        assert!(!pending.dispatch_next(now, |_| true));
        assert_eq!(pending.len(), 2);
        pending.finish_attempt("https://example.com/first", true, now);
        assert!(pending.dispatch_next(now, |url| {
            imported.push(url.to_owned());
            true
        }));
        pending.finish_attempt("https://example.com/second", true, now);
        assert_eq!(
            imported,
            ["https://example.com/first", "https://example.com/second"]
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn failed_installer_import_retries_without_losing_the_url() {
        let mut pending = PendingSubscriptionImports::default();
        let now = Instant::now();
        let url = "https://example.com/sub";
        assert!(pending.enqueue(url.into()));
        assert!(pending.dispatch_next(now, |_| true));
        pending.finish_attempt(url, false, now);
        assert_eq!(pending.len(), 1);
        assert!(!pending.dispatch_next(now, |_| true));
        assert!(pending.dispatch_next(now + Duration::from_secs(2), |_| true));
        pending.finish_attempt(url, true, now + Duration::from_secs(2));
        assert!(pending.is_empty());
    }

    #[test]
    fn approved_deep_link_failure_exposes_manual_retry_only_after_final_attempt() {
        let mut pending = PendingSubscriptionImports::default();
        let now = Instant::now();
        let url = "https://example.com/sub";
        assert!(pending.enqueue(url.into()));
        assert!(pending.dispatch_next(now, |_| true));
        assert_eq!(
            pending.finish_attempt(url, false, now),
            PendingImportOutcome::RetryScheduled
        );
        assert!(pending.dispatch_next(now + Duration::from_secs(2), |_| true));
        assert_eq!(
            pending.finish_attempt(url, false, now + Duration::from_secs(2)),
            PendingImportOutcome::RetryScheduled
        );
        assert!(pending.dispatch_next(now + Duration::from_secs(7), |_| true));
        assert_eq!(
            pending.finish_attempt(url, false, now + Duration::from_secs(7)),
            PendingImportOutcome::Failed
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn deep_link_consent_never_imports_before_yes_and_decline_discards_only_one() {
        let mut consent = PendingSubscriptionConsents::default();
        let mut imports = PendingSubscriptionImports::default();
        assert!(consent.enqueue("https://user:pass@example.com/private?token=secret".into()));
        assert!(consent.enqueue("https://second.example/sub".into()));
        assert_eq!(consent.current_host(), Some("example.com"));
        assert!(imports.is_empty());
        assert!(consent.reject_next());
        assert_eq!(consent.current_host(), Some("second.example"));
        assert!(imports.is_empty());
        assert!(consent.approve_next(&mut imports));
        assert!(consent.current_host().is_none());
        let mut imported = None;
        assert!(imports.dispatch_next(Instant::now(), |url| {
            imported = Some(url.to_owned());
            true
        }));
        assert_eq!(imported.as_deref(), Some("https://second.example/sub"));
    }

    #[test]
    fn deep_link_consent_queue_rejects_unparseable_and_oversized_input() {
        let mut consent = PendingSubscriptionConsents::default();
        assert!(!consent.enqueue("not a URL".into()));
        for index in 0..MAX_PENDING_SUBSCRIPTION_IMPORTS {
            assert!(consent.enqueue(format!("https://host{index}.example/sub")));
        }
        assert!(!consent.enqueue("https://overflow.example/sub".into()));
        assert_eq!(consent.current_host(), Some("host0.example"));
    }

    #[test]
    fn deep_link_consent_is_a_domain_only_modal_before_any_import() {
        let ui = include_str!("../ui/app.slint");
        assert!(ui.contains("in-out property <bool> deep-link-confirm-open"));
        assert!(ui.contains("in property <string> deep-link-confirm-host"));
        assert!(ui.contains("callback confirm-deep-link();"));
        assert!(ui.contains("callback cancel-deep-link();"));
        let modal = ui
            .split("if root.deep-link-confirm-open: Rectangle")
            .nth(1)
            .and_then(|block| block.split("// A taskbar restore").next())
            .expect("deep-link confirmation overlay");
        assert!(modal.contains("background: #05070bc7;"));
        assert!(modal.contains("text: root.deep-link-confirm-host;"));
        assert!(modal.contains("label: @tr(\"Отмена\");"));
        assert!(modal.contains("label: @tr(\"Добавить\");"));
        assert!(!modal.contains("root.subscription-draft"));
        assert!(modal.contains("key-pressed(event)"));
        assert!(modal.contains("root.cancel-deep-link();"));
        assert!(ui.contains(
            "enabled: !root.deep-link-confirm-open && !root.update-confirm-open && !root.log-range-open;"
        ));

        let rust = include_str!("main.rs");
        let queue = rust
            .split("fn queue_subscription_import(")
            .nth(1)
            .and_then(|block| block.split("fn wire_pending_subscription_imports(").next())
            .expect("deep-link queue handler");
        assert!(queue.contains("consent_guard.enqueue(subscription_url)"));
        assert!(queue.contains("let was_empty = consent_guard.current_host().is_none()"));
        assert!(queue.contains("if was_empty {"));
        assert!(!queue.contains("prefill_subscription("));
    }

    #[test]
    fn deep_link_modal_recaptures_keys_after_async_background_focus_restore() {
        let ui = include_str!("../ui/app.slint");
        let background = ui
            .split("visual-frame := FocusScope {")
            .nth(1)
            .and_then(|block| block.split("VerticalLayout {").next())
            .expect("background frame keyboard handlers");
        assert!(background.contains("capture-key-pressed(event)"));
        assert!(background.contains("capture-key-released(event)"));
        assert!(background.contains("if (root.deep-link-confirm-open)"));
        assert!(background.contains("root.deep-link-focus-serial += 1;"));
        assert!(
            background.contains(
                "if (event.text == Key.Escape) { root.cancel-deep-link(); return accept; }"
            )
        );
        assert!(
            ui.contains("if root.update-confirm-open && !root.deep-link-confirm-open: Rectangle")
        );

        let worker = include_str!("main.rs");
        assert!(worker.contains("apply_delete_focus_completion(&ui"));
    }

    #[test]
    fn deep_link_queue_waits_for_ui_completion_before_advancing() {
        let source = include_str!("main.rs");
        let worker = source
            .split("fn run_import(")
            .nth(1)
            .and_then(|block| block.split("fn run_connect(").next())
            .expect("import worker");
        assert!(
            worker.find("slint::invoke_from_event_loop").unwrap()
                < worker.find(".finish_attempt(&url").unwrap()
        );
    }

    #[test]
    fn installer_uri_requests_consent_on_initial_and_secondary_launch() {
        let lf_source = include_str!("main.rs").replace("\r\n", "\n");
        for source in [lf_source.clone(), lf_source.replace('\n', "\r\n")] {
            // include_str! retains Git's checkout line endings on Windows runners.
            let source = source.replace("\r\n", "\n");
            let startup = source
                .split("wire_import(\n        &ui,")
                .nth(1)
                .and_then(|source| {
                    source
                        .split("let result = event_loop_guard::run_until_quit(")
                        .next()
                })
                .expect("startup after import callback wiring");
            assert!(startup.contains("&pending_subscription_consents,\n            &pending_subscription_imports,\n            subscription_url"));

            let dispatch = source
                .split("fn wire_pending_subscription_imports(")
                .nth(1)
                .and_then(|source| source.split("fn wire_external_activations(").next())
                .expect("queued installer URL dispatch");
            assert!(dispatch.contains("ui.invoke_import_url(url.into())"));
            assert!(dispatch.contains(".dispatch_next(Instant::now(), |url|"));

            let secondary = source
                .split("single_instance::Activation::InstallSubscription(url) => {")
                .nth(1)
                .and_then(|source| source.split(".is_err()").next())
                .expect("secondary installer activation");
            assert!(secondary.contains("model.open_panel(Panel::None)"));
            assert!(
                secondary.find("model.open_panel(Panel::None)").unwrap()
                    < secondary
                        .find("queue_subscription_import(&ui, &consent, &pending, url)")
                        .unwrap()
            );

            let import_worker = source
                .split("fn run_import(")
                .nth(1)
                .and_then(|source| source.split("fn run_connect(").next())
                .expect("import worker");
            assert!(import_worker.contains(
                "advance_profile_reconciliation(weak, model, profile_preferences, import_succeeded)"
            ));
        }
    }

    #[test]
    fn import_and_reconciliation_callbacks_use_current_model_state() {
        let source = include_str!("main.rs");
        for (start, end) in [
            ("fn run_status(", "fn run_import("),
            ("fn run_import(", "fn run_connect("),
            (
                "fn run_profile_status_reconciliation(",
                "fn run_subscription_refresh(",
            ),
            (
                "fn load_profile_list(",
                "fn advance_profile_reconciliation(",
            ),
            ("fn load_catalog_impl(", "fn run_latency_check("),
            ("fn apply_snapshot_from_worker(", "fn load_events("),
        ] {
            let worker = source
                .split(start)
                .nth(1)
                .and_then(|source| source.split(end).next())
                .expect("reconciliation worker");
            assert!(
                worker.contains("apply_latest_snapshot(&ui, &model)"),
                "{start}"
            );
        }
        let status = source
            .split("fn run_status(")
            .nth(1)
            .and_then(|source| source.split("fn run_import(").next())
            .expect("startup status worker");
        assert!(status.contains("if refresh_catalog && !current.import_pending"));
        assert!(status.contains(".defer_profile_list_reload()"));
        let disconnect = source
            .split("fn apply_snapshot_from_worker(")
            .nth(1)
            .and_then(|source| source.split("fn load_events(").next())
            .expect("disconnect and connect UI completion");
        assert!(disconnect.contains(".defer_profile_catalog_reload()"));
    }

    #[test]
    fn first_run_clipboard_action_reuses_the_import_pipeline() {
        let source = include_str!("main.rs");
        assert!(source.contains("wire_clipboard_import(&ui, model.clone());"));

        let wiring = source
            .split("fn wire_clipboard_import(")
            .nth(1)
            .and_then(|source| source.split("fn wire_subscription_refresh(").next())
            .expect("clipboard import wiring");
        for required in [
            "ui.on_paste_subscription",
            "ui.on_dismiss_first_run_import",
            "clipboard_text::read_subscription_text(ui.window())",
            "ui.set_subscription_draft",
            "ui.invoke_import_url",
            "model.set_import_error_message(error.user_message())",
            "apply_snapshot(&ui, &current)",
            "model.dismiss_import_error()",
        ] {
            assert!(
                wiring.contains(required),
                "missing clipboard wiring: {required}"
            );
        }
    }
}

#[cfg(test)]
mod english_tests {
    use crate::view_model::UiState;
    use multicore_i18n::{Lang, ThreadLanguage};

    #[test]
    fn the_connection_toggle_reads_in_english() {
        let _en = ThreadLanguage::set(Lang::En);
        let ready = UiState::Ready {
            profile: "p".into(),
            node: "n".into(),
        };
        assert_eq!(
            super::toggle_presentation(&ready.presentation()).title,
            "Not protected"
        );
        let connected = UiState::Connected {
            profile: "p".into(),
            node: "n".into(),
        };
        assert_eq!(
            super::toggle_presentation(&connected.presentation()).title,
            "Protected"
        );
        assert_eq!(super::preference_save_error(), "Couldn't save settings.");
        assert_eq!(super::subscription_initial("  "), "S");
    }
}
