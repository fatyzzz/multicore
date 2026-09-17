#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app_icon;
mod bootstrap;
mod daemon;
mod single_instance;
mod tray;
mod updater;
mod view_model;
mod windows_settings;

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use daemon::{DaemonClient, DaemonError, UnavailableDaemonClient};
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

    let model = Arc::new(Mutex::new(DesktopViewModel::new(client)));
    let update_state = Arc::new(Mutex::new(UpdateState::initial()));

    let ui = AppWindow::new()?;
    apply_snapshot(&ui, &snapshot(&model.lock().expect("view model lock")));
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
    let tray_runtime = tray::start(
        tray_model,
        move || tray_menu_model(&tray_model_source.lock().expect("view model lock")),
        move |command| handle_tray_command(&tray_action_ui, &tray_action_model, command),
    )
    .map_err(|error| eprintln!("tray unavailable: {error}"))
    .ok();
    wire_window_controls(&ui, tray_runtime.is_some());
    wire_updater(&ui, model.clone(), update_state.clone());
    if updater::configured_repository().ok().flatten().is_some() {
        run_update_check(ui.as_weak(), model.clone(), update_state);
    }
    run_refresh(ui.as_weak(), model);
    if initial_window_visible(launch_mode, tray_runtime.is_some()) {
        ui.show()?;
    }
    let result = slint::run_event_loop();
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CloseDisposition {
    HideToTray,
    Exit,
}

fn close_disposition(tray_available: bool, smoke_mode: bool) -> CloseDisposition {
    if tray_available && !smoke_mode {
        CloseDisposition::HideToTray
    } else {
        CloseDisposition::Exit
    }
}

fn smoke_close_requested() -> bool {
    std::env::var_os("MULTICORE_SMOKE_EXIT_ON_CLOSE").as_deref() == Some(std::ffi::OsStr::new("1"))
}

fn next_maximized(current: bool) -> bool {
    !current
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
    ui.on_window_minimize(move || {
        if let Some(ui) = weak.upgrade() {
            ui.window().set_minimized(true);
        }
    });

    let weak = ui.as_weak();
    ui.on_window_toggle_maximize(move || {
        if let Some(ui) = weak.upgrade() {
            let window = ui.window();
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
) {
    let Some(ui) = weak.upgrade() else {
        return;
    };
    match command {
        TrayCommand::ShowWindow => {
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
        CloseDisposition, LaunchArguments, LaunchMode, activation_for_launch_arguments,
        close_disposition, initial_window_visible, next_maximized, parse_launch_arguments,
        runtime_allows_update,
    };
    use crate::single_instance::Activation;
    use crate::view_model::UiState;

    #[test]
    fn maximize_toggle_inverts_current_window_state() {
        assert!(next_maximized(false));
        assert!(!next_maximized(true));
    }

    #[test]
    fn close_hides_only_when_a_tray_is_available_outside_smoke_mode() {
        assert_eq!(close_disposition(true, false), CloseDisposition::HideToTray);
        assert_eq!(close_disposition(false, false), CloseDisposition::Exit);
        assert_eq!(close_disposition(true, true), CloseDisposition::Exit);
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
