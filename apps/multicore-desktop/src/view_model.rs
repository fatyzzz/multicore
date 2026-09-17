use std::{
    path::Path,
    sync::{Arc, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use regex::Regex;

use crate::daemon::{
    DaemonCatalog, DaemonClient, DaemonDiagnostics, DaemonEvent, DaemonEventBatch, DaemonLatencies,
    DaemonStatus, DaemonSubscriptionInfo,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiState {
    Empty,
    Importing,
    Ready {
        profile: String,
        node: String,
    },
    Connecting {
        profile: String,
        node: String,
        step: String,
    },
    Disconnecting {
        profile: String,
        node: String,
    },
    Connected {
        profile: String,
        node: String,
    },
    Error {
        message: String,
    },
    DaemonDegraded {
        message: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrimaryAction {
    None,
    Connect,
    Disconnect,
    Retry,
    Cleanup,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Panel {
    #[default]
    None,
    Subscription,
    Routes,
    Events,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticTone {
    Neutral,
    Accent,
    Success,
    Danger,
    Warning,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MutationToken(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MutationKind {
    Refresh,
    Import,
    Connect,
    Disconnect,
    DisconnectReconciliation,
    SubscriptionRefresh,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ActiveMutation {
    token: MutationToken,
    kind: MutationKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EventRequestToken(u64);

pub(crate) struct EventRequest {
    pub(crate) client: Arc<dyn DaemonClient>,
    pub(crate) after: u64,
    pub(crate) known_epoch: Option<String>,
    pub(crate) token: EventRequestToken,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DiagnosticsRequestToken(u64);

pub(crate) struct DiagnosticsRequest {
    pub(crate) client: Arc<dyn DaemonClient>,
    pub(crate) token: DiagnosticsRequestToken,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CatalogRequestToken(u64);

pub(crate) struct CatalogRequest {
    pub(crate) client: Arc<dyn DaemonClient>,
    pub(crate) token: CatalogRequestToken,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LatencyRequestToken(u64);

pub(crate) struct LatencyRequest {
    pub(crate) client: Arc<dyn DaemonClient>,
    pub(crate) token: LatencyRequestToken,
    pub(crate) catalog_revision: Option<u64>,
    pub(crate) group_ids: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LatencyPresentation {
    pub(crate) loading: bool,
    pub(crate) queued: bool,
    pub(crate) error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LatencyOutcome {
    pub(crate) refresh_catalog: bool,
    pub(crate) rerun: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SelectionRequestToken(u64);

pub(crate) struct NodeSelectionRequest {
    pub(crate) client: Arc<dyn DaemonClient>,
    pub(crate) token: SelectionRequestToken,
    pub(crate) group_id: String,
    pub(crate) revision: u64,
    pub(crate) node_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct QueuedNodeSelection {
    pub(crate) group_id: String,
    pub(crate) revision: u64,
    pub(crate) node_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SelectionOutcome {
    pub(crate) refresh_catalog: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ActiveSelection {
    token: SelectionRequestToken,
    group_id: String,
    previously_selected_node_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CatalogGroupPresentation {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) selected: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CatalogNodePresentation {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) selected: bool,
    pub(crate) delay_ms: Option<u64>,
    pub(crate) latency_text: String,
    pub(crate) latency_tone: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CatalogPresentation {
    pub(crate) revision: Option<u64>,
    pub(crate) groups: Vec<CatalogGroupPresentation>,
    pub(crate) selected_group_id: Option<String>,
    pub(crate) nodes: Vec<CatalogNodePresentation>,
    pub(crate) loading: bool,
    pub(crate) group_navigation_enabled: bool,
    pub(crate) selection_enabled: bool,
    pub(crate) selection_pending: bool,
    pub(crate) selection_queued: bool,
    pub(crate) error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SubscriptionPresentation {
    pub(crate) display_name: String,
    pub(crate) usage: String,
    pub(crate) expiry: String,
    pub(crate) announcement_text: String,
    pub(crate) announcement_tone: String,
    pub(crate) service_logo_path: Option<String>,
    pub(crate) refresh_available: bool,
    pub(crate) refresh_pending: bool,
    pub(crate) error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CatalogGroup {
    id: String,
    label: String,
    is_primary: bool,
    nodes: Vec<CatalogNodePresentation>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum EventFilter {
    #[default]
    All,
    System,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatePresentation {
    pub eyebrow: &'static str,
    pub headline: String,
    pub supporting: String,
    pub profile: String,
    pub node: String,
    pub primary_label: &'static str,
    pub primary_action: PrimaryAction,
    pub primary_enabled: bool,
    pub pill_label: &'static str,
    pub semantic_tone: SemanticTone,
    pub has_profile: bool,
    pub is_connected: bool,
    pub is_busy: bool,
    pub is_error: bool,
    pub is_degraded: bool,
}

impl UiState {
    pub fn presentation(&self) -> StatePresentation {
        match self {
            Self::Empty => StatePresentation {
                eyebrow: "Профиль не добавлен",
                headline: "Добавьте подписку".into(),
                supporting: "Вставьте URL — остальное настроим автоматически.".into(),
                profile: "Нет профиля".into(),
                node: "Маршрут не выбран".into(),
                primary_label: "Подключиться",
                primary_action: PrimaryAction::None,
                primary_enabled: false,
                pill_label: "ГОТОВО",
                semantic_tone: SemanticTone::Neutral,
                has_profile: false,
                is_connected: false,
                is_busy: false,
                is_error: false,
                is_degraded: false,
            },
            Self::Importing => StatePresentation {
                eyebrow: "Импорт",
                headline: "Добавляем профиль…".into(),
                supporting: "Проверяем подписку и доступные маршруты.".into(),
                profile: "Новый профиль".into(),
                node: "Маршрут не выбран".into(),
                primary_label: "Добавляем…",
                primary_action: PrimaryAction::None,
                primary_enabled: false,
                pill_label: "ПОДКЛЮЧЕНИЕ",
                semantic_tone: SemanticTone::Warning,
                has_profile: false,
                is_connected: false,
                is_busy: true,
                is_error: false,
                is_degraded: false,
            },
            Self::Ready { profile, node } => StatePresentation {
                eyebrow: "Готово",
                headline: "Можно подключаться".into(),
                supporting: String::new(),
                profile: profile.clone(),
                node: node.clone(),
                primary_label: "Подключиться",
                primary_action: PrimaryAction::Connect,
                primary_enabled: true,
                pill_label: "ГОТОВО",
                semantic_tone: SemanticTone::Accent,
                has_profile: true,
                is_connected: false,
                is_busy: false,
                is_error: false,
                is_degraded: false,
            },
            Self::Connecting {
                profile,
                node,
                step,
            } => StatePresentation {
                eyebrow: "Подключение",
                headline: "Устанавливаем соединение…".into(),
                supporting: step.clone(),
                profile: profile.clone(),
                node: node.clone(),
                primary_label: "Подключаем…",
                primary_action: PrimaryAction::None,
                primary_enabled: false,
                pill_label: "ПОДКЛЮЧЕНИЕ",
                semantic_tone: SemanticTone::Warning,
                has_profile: true,
                is_connected: false,
                is_busy: true,
                is_error: false,
                is_degraded: false,
            },
            Self::Connected { profile, node } => StatePresentation {
                eyebrow: "Подключено",
                headline: "Соединение защищено".into(),
                supporting: "Текущий маршрут".into(),
                profile: profile.clone(),
                node: node.clone(),
                primary_label: "Отключиться",
                primary_action: PrimaryAction::Disconnect,
                primary_enabled: true,
                pill_label: "В СЕТИ",
                semantic_tone: SemanticTone::Success,
                has_profile: true,
                is_connected: true,
                is_busy: false,
                is_error: false,
                is_degraded: false,
            },
            Self::Disconnecting { profile, node } => StatePresentation {
                eyebrow: "Отключение",
                headline: "Завершаем соединение…".into(),
                supporting: "Отключаем локальный маршрут.".into(),
                profile: profile.clone(),
                node: node.clone(),
                primary_label: "Отключаем…",
                primary_action: PrimaryAction::None,
                primary_enabled: false,
                pill_label: "ОТКЛЮЧЕНИЕ",
                semantic_tone: SemanticTone::Warning,
                has_profile: true,
                is_connected: true,
                is_busy: true,
                is_error: false,
                is_degraded: false,
            },
            Self::Error { message } => StatePresentation {
                eyebrow: "Не удалось подключиться",
                headline: "Что-то пошло не так".into(),
                supporting: message.clone(),
                profile: "—".into(),
                node: "Авто".into(),
                primary_label: "Повторить",
                primary_action: PrimaryAction::Retry,
                primary_enabled: true,
                pill_label: "ОШИБКА",
                semantic_tone: SemanticTone::Danger,
                has_profile: true,
                is_connected: false,
                is_busy: false,
                is_error: true,
                is_degraded: false,
            },
            Self::DaemonDegraded { message } => StatePresentation {
                eyebrow: "Сервис недоступен",
                headline: "Ограниченный режим".into(),
                supporting: message.clone(),
                profile: "Состояние неизвестно".into(),
                node: "—".into(),
                primary_label: "Очистить",
                primary_action: PrimaryAction::Cleanup,
                primary_enabled: true,
                pill_label: "СЕРВИС",
                semantic_tone: SemanticTone::Warning,
                has_profile: true,
                is_connected: false,
                is_busy: false,
                is_error: false,
                is_degraded: true,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresentedEvent {
    pub id: u64,
    pub timestamp: String,
    pub component: String,
    pub level: String,
    pub message: String,
}

impl From<DaemonEvent> for PresentedEvent {
    fn from(event: DaemonEvent) -> Self {
        Self {
            id: event.id,
            timestamp: event.timestamp,
            component: "system".into(),
            level: event.level,
            message: redact_event_text(&event.message),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeCheckPresentation {
    pub value: String,
    pub tone: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticsPresentation {
    pub xray: RuntimeCheckPresentation,
    pub mihomo: RuntimeCheckPresentation,
    pub tun: RuntimeCheckPresentation,
    pub mappings: Vec<ProxyMappingPresentation>,
    pub loading: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyMappingPresentation {
    pub proxy_name: String,
    pub address: String,
    pub xray_label: String,
}

pub fn redact_event_text(input: &str) -> String {
    static AUTHORIZATION: OnceLock<Regex> = OnceLock::new();
    static SENSITIVE_VALUE: OnceLock<Regex> = OnceLock::new();
    static URL: OnceLock<Regex> = OnceLock::new();
    static UUID: OnceLock<Regex> = OnceLock::new();

    let authorization = AUTHORIZATION.get_or_init(|| {
        Regex::new(r"(?im)\b(proxy-authorization|authorization)\s*:\s*[^\r\n]+")
            .expect("authorization redaction regex must compile")
    });
    let sensitive = SENSITIVE_VALUE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(token|password|secret|api[_-]?key|subscription(?:_url)?|url)\s*([:=])\s*([^\s,;]+)",
        )
        .expect("redaction regex must compile")
    });
    let url = URL.get_or_init(|| {
        Regex::new(r"(?i)\bhttps?://[^\s,;]+").expect("URL redaction regex must compile")
    });
    let uuid = UUID.get_or_init(|| {
        Regex::new(r"(?i)\b[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}\b")
            .expect("UUID redaction regex must compile")
    });

    let without_authorization = authorization.replace_all(input, "$1: [REDACTED]");
    let without_values = sensitive.replace_all(&without_authorization, "$1$2[REDACTED]");
    let without_urls = url.replace_all(&without_values, "[REDACTED_URL]");
    uuid.replace_all(&without_urls, "[REDACTED_UUID]")
        .into_owned()
}

pub fn state_from_status(status: DaemonStatus) -> UiState {
    if status.degraded {
        return UiState::DaemonDegraded {
            message: status
                .message
                .unwrap_or_else(|| "Фоновый сервис отвечает не полностью.".into()),
        };
    }

    let profile = status.profile.unwrap_or_else(|| "Основной".into());
    let node = status.current_node.unwrap_or_else(|| "Авто".into());
    match status.state.as_str() {
        "empty" => UiState::Empty,
        "importing" => UiState::Importing,
        "connecting" => UiState::Connecting {
            profile,
            node,
            step: status
                .message
                .unwrap_or_else(|| "Проверяем доступность маршрута…".into()),
        },
        "connected" => UiState::Connected { profile, node },
        "ready" | "disconnected" => UiState::Ready { profile, node },
        "error" => UiState::Error {
            message: status
                .message
                .unwrap_or_else(|| "Повторите попытку через несколько секунд.".into()),
        },
        _ => UiState::Error {
            message: "Фоновый сервис вернул неизвестное состояние.".into(),
        },
    }
}

pub struct DesktopViewModel {
    client: Arc<dyn DaemonClient>,
    state: UiState,
    panel: Panel,
    import_pending: bool,
    import_error: Option<String>,
    import_previous_state: Option<UiState>,
    connection_notice: Option<String>,
    subscription: Option<DaemonSubscriptionInfo>,
    subscription_refresh_error: Option<String>,
    next_mutation_generation: u64,
    active_mutation: Option<ActiveMutation>,
    event_epoch: Option<String>,
    event_cursor: u64,
    event_cache: Vec<PresentedEvent>,
    event_load_error: Option<PresentedEvent>,
    event_filter: EventFilter,
    next_event_generation: u64,
    event_request_pending: Option<EventRequestToken>,
    diagnostics: Option<DaemonDiagnostics>,
    diagnostics_error: Option<PresentedEvent>,
    next_diagnostics_generation: u64,
    diagnostics_request_pending: Option<DiagnosticsRequestToken>,
    catalog_revision: Option<u64>,
    catalog_groups: Vec<CatalogGroup>,
    selected_catalog_group: Option<String>,
    catalog_error: Option<String>,
    next_catalog_generation: u64,
    catalog_request_pending: Option<CatalogRequestToken>,
    catalog_request_deferred: bool,
    next_selection_generation: u64,
    active_selection: Option<ActiveSelection>,
    queued_selections: Vec<QueuedNodeSelection>,
    catalog_reconciliation_required: bool,
    catalog_notice_after_refresh: Option<String>,
    next_latency_generation: u64,
    latency_request_pending: Option<LatencyRequestToken>,
    latency_request_revision: Option<u64>,
    latency_request_group_ids: Vec<String>,
    latency_queued: bool,
    latency_error: Option<String>,
}

impl DesktopViewModel {
    pub fn new(client: Arc<dyn DaemonClient>) -> Self {
        Self {
            client,
            state: UiState::DaemonDegraded {
                message: "Проверяем фоновый сервис…".into(),
            },
            panel: Panel::None,
            import_pending: false,
            import_error: None,
            import_previous_state: None,
            connection_notice: None,
            subscription: None,
            subscription_refresh_error: None,
            next_mutation_generation: 0,
            active_mutation: None,
            event_epoch: None,
            event_cursor: 0,
            event_cache: Vec::new(),
            event_load_error: None,
            event_filter: EventFilter::All,
            next_event_generation: 0,
            event_request_pending: None,
            diagnostics: None,
            diagnostics_error: None,
            next_diagnostics_generation: 0,
            diagnostics_request_pending: None,
            catalog_revision: None,
            catalog_groups: Vec::new(),
            selected_catalog_group: None,
            catalog_error: None,
            next_catalog_generation: 0,
            catalog_request_pending: None,
            catalog_request_deferred: false,
            next_selection_generation: 0,
            active_selection: None,
            queued_selections: Vec::new(),
            catalog_reconciliation_required: false,
            catalog_notice_after_refresh: None,
            next_latency_generation: 0,
            latency_request_pending: None,
            latency_request_revision: None,
            latency_request_group_ids: Vec::new(),
            latency_queued: false,
            latency_error: None,
        }
    }

    pub fn presentation(&self) -> StatePresentation {
        let mut presentation = self.state.presentation();
        if let Some(notice) = &self.connection_notice {
            presentation.supporting = notice.clone();
            presentation.is_error = true;
        }
        if self
            .active_mutation
            .is_some_and(|active| active.kind == MutationKind::DisconnectReconciliation)
        {
            presentation.eyebrow = "Проверка";
            presentation.headline = "Проверяем состояние…".into();
            presentation.primary_label = "Проверяем…";
            presentation.pill_label = "ПРОВЕРКА";
            presentation.semantic_tone = SemanticTone::Warning;
            presentation.is_connected = false;
            presentation.is_busy = true;
        }
        if self.active_mutation.is_some()
            || self.import_pending
            || self.active_selection.is_some()
            || self.catalog_reconciliation_required
        {
            presentation.primary_enabled = false;
        }
        presentation
    }

    pub(crate) fn panel(&self) -> Panel {
        self.panel
    }

    pub(crate) fn import_pending(&self) -> bool {
        self.import_pending
    }

    pub(crate) fn import_error(&self) -> Option<&str> {
        self.import_error.as_deref()
    }

    pub(crate) fn daemon_client(&self) -> Arc<dyn DaemonClient> {
        self.client.clone()
    }

    fn apply_daemon_status(&mut self, status: DaemonStatus) -> UiState {
        self.subscription = status.subscription.clone();
        state_from_status(status)
    }

    pub(crate) fn subscription_presentation(&self) -> SubscriptionPresentation {
        self.subscription_presentation_at(current_unix_time())
    }

    pub(crate) fn subscription_presentation_at(&self, now_unix: u64) -> SubscriptionPresentation {
        let Some(info) = &self.subscription else {
            return SubscriptionPresentation {
                display_name: "Подписка".into(),
                usage: "Нет данных".into(),
                expiry: "Добавьте ссылку".into(),
                announcement_text: String::new(),
                announcement_tone: "info".into(),
                service_logo_path: None,
                refresh_available: false,
                refresh_pending: false,
                error: self.subscription_refresh_error.clone(),
            };
        };
        let used_bytes =
            info.used_bytes
                .or_else(|| match (info.uploaded_bytes, info.downloaded_bytes) {
                    (Some(uploaded), Some(downloaded)) => Some(uploaded.saturating_add(downloaded)),
                    (Some(uploaded), None) => Some(uploaded),
                    (None, Some(downloaded)) => Some(downloaded),
                    (None, None) => None,
                });
        let usage = match (used_bytes, info.total_bytes) {
            (Some(used), Some(total)) => format!("{} / {}", format_gib(used), format_gib(total)),
            (Some(used), None) => format_gib(used),
            (None, Some(total)) => format!("— / {}", format_gib(total)),
            _ => "Нет данных".into(),
        };
        let expiry = info.expires_at_unix.map_or_else(
            || "Срок неизвестен".into(),
            |expires| {
                if expires <= now_unix {
                    "Срок истёк".into()
                } else {
                    let days = (expires - now_unix).div_ceil(86_400);
                    format!("Осталось {days} {}", russian_days(days))
                }
            },
        );
        let display_name = bounded_plain_text(&info.display_name, 128)
            .filter(|name| !name.is_empty())
            .or_else(|| bounded_plain_text(&info.source_name, 128))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "Подписка".into());
        let announcement_text = info
            .announcement_text
            .as_deref()
            .and_then(|text| bounded_plain_text(text, 512))
            .unwrap_or_default();
        let announcement_tone = match info.announcement_tone.as_deref() {
            Some("green" | "success") => "success",
            Some("red" | "danger") => "danger",
            _ => "info",
        };
        SubscriptionPresentation {
            display_name,
            usage,
            expiry,
            announcement_text,
            announcement_tone: announcement_tone.into(),
            service_logo_path: info.service_logo_path.as_deref().and_then(local_logo_path),
            refresh_available: info.refresh_available,
            refresh_pending: self
                .active_mutation
                .is_some_and(|active| active.kind == MutationKind::SubscriptionRefresh),
            error: self.subscription_refresh_error.clone(),
        }
    }

    pub(crate) fn begin_subscription_refresh(&mut self) -> Option<MutationToken> {
        if self.active_mutation.is_some()
            || self.import_pending
            || self.active_selection.is_some()
            || self.catalog_reconciliation_required
            || matches!(
                self.state,
                UiState::Connected { .. } | UiState::Connecting { .. }
            )
            || !self
                .subscription
                .as_ref()
                .is_some_and(|info| info.refresh_available)
        {
            return None;
        }
        self.subscription_refresh_error = None;
        Some(self.next_mutation(MutationKind::SubscriptionRefresh))
    }

    pub(crate) fn finish_subscription_refresh(
        &mut self,
        token: MutationToken,
        result: Result<DaemonStatus, crate::daemon::DaemonError>,
    ) -> bool {
        if !self.is_current_mutation(token, MutationKind::SubscriptionRefresh) {
            return false;
        }
        let succeeded = result.is_ok();
        match result {
            Ok(status) => {
                self.state = self.apply_daemon_status(status);
                self.subscription_refresh_error = None;
                self.catalog_revision = None;
                self.queued_selections.clear();
                self.latency_queued = true;
            }
            Err(_) => {
                self.subscription_refresh_error =
                    Some("Не удалось обновить. Рабочий профиль сохранён.".into());
            }
        }
        self.active_mutation = None;
        succeeded
    }

    fn next_mutation(&mut self, kind: MutationKind) -> MutationToken {
        self.next_mutation_generation = self.next_mutation_generation.wrapping_add(1);
        let token = MutationToken(self.next_mutation_generation);
        self.active_mutation = Some(ActiveMutation { token, kind });
        token
    }

    fn is_current_mutation(&self, token: MutationToken, kind: MutationKind) -> bool {
        self.active_mutation == Some(ActiveMutation { token, kind })
    }

    pub(crate) fn begin_refresh(&mut self) -> MutationToken {
        self.connection_notice = None;
        self.next_mutation(MutationKind::Refresh)
    }

    pub(crate) fn begin_disconnect_reconciliation(&mut self) -> MutationToken {
        self.next_mutation(MutationKind::DisconnectReconciliation)
    }

    pub fn open_panel(&mut self, panel: Panel) {
        self.panel = panel;
    }

    pub fn close_panel_on_escape(&mut self) -> bool {
        if self.panel == Panel::None {
            return false;
        }
        self.panel = Panel::None;
        true
    }

    pub fn begin_primary_action(&mut self) -> Option<(PrimaryAction, MutationToken)> {
        if self.active_mutation.is_some()
            || self.import_pending
            || self.active_selection.is_some()
            || self.catalog_reconciliation_required
        {
            return None;
        }
        let action = self.state.presentation().primary_action;
        if action == PrimaryAction::None {
            return None;
        }
        let kind = match action {
            PrimaryAction::Connect => MutationKind::Connect,
            PrimaryAction::Disconnect | PrimaryAction::Cleanup => MutationKind::Disconnect,
            PrimaryAction::Retry => MutationKind::Refresh,
            PrimaryAction::None => return None,
        };
        let token = self.next_mutation(kind);
        self.connection_notice = None;
        if action == PrimaryAction::Connect {
            let current = self.state.presentation();
            self.state = UiState::Connecting {
                profile: current.profile,
                node: current.node,
                step: "Запускаем локальный маршрут…".into(),
            };
        } else if action == PrimaryAction::Disconnect {
            let current = self.state.presentation();
            self.state = UiState::Disconnecting {
                profile: current.profile,
                node: current.node,
            };
        }
        Some((action, token))
    }

    pub fn begin_import(&mut self, url: &str) -> Option<MutationToken> {
        if self.import_pending
            || self.active_selection.is_some()
            || self.catalog_reconciliation_required
        {
            return None;
        }
        if self
            .active_mutation
            .is_some_and(|active| active.kind != MutationKind::Refresh)
        {
            self.import_error = Some("Дождитесь завершения операции подключения.".into());
            return None;
        }
        let valid = reqwest::Url::parse(url.trim())
            .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some());
        if !valid {
            self.import_error = Some("Введите корректный HTTP(S) URL.".into());
            return None;
        }
        self.import_error = None;
        self.import_previous_state = Some(self.state.clone());
        self.import_pending = true;
        Some(self.next_mutation(MutationKind::Import))
    }

    pub(crate) fn finish_refresh(
        &mut self,
        token: MutationToken,
        result: Result<DaemonStatus, crate::daemon::DaemonError>,
    ) {
        if !self.is_current_mutation(token, MutationKind::Refresh) {
            return;
        }
        self.state = match result {
            Ok(status) => self.apply_daemon_status(status),
            Err(error) => UiState::Error {
                message: error.to_string(),
            },
        };
        self.active_mutation = None;
    }

    pub(crate) fn finish_import(
        &mut self,
        token: MutationToken,
        result: Result<DaemonStatus, crate::daemon::DaemonError>,
    ) {
        if !self.is_current_mutation(token, MutationKind::Import) {
            return;
        }
        let previous_state = self
            .import_previous_state
            .take()
            .unwrap_or_else(|| self.state.clone());
        match result {
            Ok(status) => {
                self.state = self.apply_daemon_status(status);
                self.import_error = None;
                self.catalog_revision = None;
            }
            Err(_) => {
                self.state = previous_state;
                self.import_error =
                    Some("Не удалось добавить подписку. Проверьте URL и повторите попытку.".into());
            }
        }
        self.import_pending = false;
        self.active_mutation = None;
    }

    pub(crate) fn finish_connect(
        &mut self,
        token: MutationToken,
        result: Result<DaemonStatus, crate::daemon::DaemonError>,
    ) {
        if !self.is_current_mutation(token, MutationKind::Connect) {
            return;
        }
        let succeeded = result.is_ok();
        self.state = match result {
            Ok(status) => self.apply_daemon_status(status),
            Err(error) => UiState::Error {
                message: error.to_string(),
            },
        };
        if succeeded {
            self.catalog_revision = None;
        }
        self.active_mutation = None;
    }

    pub(crate) fn finish_disconnect(
        &mut self,
        token: MutationToken,
        result: Result<DaemonStatus, crate::daemon::DaemonError>,
    ) -> bool {
        if !self.is_current_mutation(token, MutationKind::Disconnect) {
            return false;
        }
        let rollback = match &self.state {
            UiState::Disconnecting { profile, node } => Some(UiState::Connected {
                profile: profile.clone(),
                node: node.clone(),
            }),
            _ => None,
        };
        let (state, reconcile_status, succeeded) = match result {
            Ok(status) => (self.apply_daemon_status(status), false, true),
            Err(error) if error.requires_reconciliation() => (
                UiState::DaemonDegraded {
                    message: "Не удалось подтвердить завершение соединения. Проверяем состояние…"
                        .into(),
                },
                true,
                false,
            ),
            Err(_) => {
                self.connection_notice =
                    Some("Не удалось отключиться. Соединение остаётся активным.".into());
                (
                    rollback.unwrap_or_else(|| UiState::Error {
                        message: "Не удалось отключиться. Повторите попытку.".into(),
                    }),
                    false,
                    false,
                )
            }
        };
        self.state = state;
        if succeeded || reconcile_status {
            self.catalog_revision = None;
        }
        self.active_mutation = None;
        reconcile_status
    }

    pub(crate) fn finish_disconnect_reconciliation(
        &mut self,
        token: MutationToken,
        result: Result<DaemonStatus, crate::daemon::DaemonError>,
    ) -> bool {
        if !self.is_current_mutation(token, MutationKind::DisconnectReconciliation) {
            return false;
        }
        self.connection_notice = None;
        self.state = match result {
            Ok(status) => self.apply_daemon_status(status),
            Err(_) => UiState::DaemonDegraded {
                message: "Не удалось подтвердить состояние соединения. Повторите проверку.".into(),
            },
        };
        self.active_mutation = None;
        true
    }

    pub(crate) fn set_event_filter(&mut self, level: Option<&str>) -> Vec<PresentedEvent> {
        self.event_filter = match level {
            Some(level) if level.eq_ignore_ascii_case("system") => EventFilter::System,
            Some(level) if level.eq_ignore_ascii_case("error") => EventFilter::Error,
            _ => EventFilter::All,
        };
        self.filtered_events()
    }

    pub(crate) fn begin_events_request(&mut self) -> Option<EventRequest> {
        if self.event_request_pending.is_some() {
            return None;
        }
        self.next_event_generation = self.next_event_generation.wrapping_add(1);
        let token = EventRequestToken(self.next_event_generation);
        self.event_request_pending = Some(token);
        Some(EventRequest {
            client: self.client.clone(),
            after: self.event_cursor,
            known_epoch: self.event_epoch.clone(),
            token,
        })
    }

    pub(crate) fn begin_diagnostics_request(&mut self) -> Option<DiagnosticsRequest> {
        if self.diagnostics_request_pending.is_some() {
            return None;
        }
        self.next_diagnostics_generation = self.next_diagnostics_generation.wrapping_add(1);
        let token = DiagnosticsRequestToken(self.next_diagnostics_generation);
        self.diagnostics_request_pending = Some(token);
        self.diagnostics_error = None;
        Some(DiagnosticsRequest {
            client: self.client.clone(),
            token,
        })
    }

    pub(crate) fn finish_diagnostics(
        &mut self,
        token: DiagnosticsRequestToken,
        result: Result<DaemonDiagnostics, crate::daemon::DaemonError>,
    ) -> Option<Vec<PresentedEvent>> {
        if self.diagnostics_request_pending != Some(token) {
            return None;
        }
        self.diagnostics_request_pending = None;
        match result {
            Ok(mut diagnostics) => {
                diagnostics.logs.truncate(256);
                self.diagnostics = Some(diagnostics);
                self.diagnostics_error = None;
            }
            Err(_) => {
                self.diagnostics_error = Some(PresentedEvent {
                    id: 0,
                    timestamp: String::new(),
                    component: "system".into(),
                    level: "error".into(),
                    message: "Не удалось обновить диагностику. Повторно откройте экран.".into(),
                });
            }
        }
        Some(self.filtered_events())
    }

    pub fn diagnostics_presentation(&self) -> DiagnosticsPresentation {
        let loading = self.diagnostics_request_pending.is_some();
        let checks = self.diagnostics.as_ref();
        DiagnosticsPresentation {
            xray: present_runtime_check(checks.map(|value| value.xray.as_str()), loading),
            mihomo: present_runtime_check(checks.map(|value| value.mihomo.as_str()), loading),
            tun: present_runtime_check(checks.map(|value| value.tun.as_str()), loading),
            mappings: checks
                .map(|value| {
                    value
                        .mappings
                        .iter()
                        .map(|mapping| ProxyMappingPresentation {
                            proxy_name: mapping.proxy_name.clone(),
                            address: mapping.address.clone(),
                            xray_label: mapping.xray_label.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            loading,
        }
    }

    pub(crate) fn finish_events(
        &mut self,
        token: EventRequestToken,
        result: Result<DaemonEventBatch, crate::daemon::DaemonError>,
    ) -> Option<Vec<PresentedEvent>> {
        if self.event_request_pending != Some(token) {
            return None;
        }
        self.event_request_pending = None;
        match result {
            Ok(batch) => {
                self.event_load_error = None;
                if self.event_epoch.as_deref() != Some(batch.epoch.as_str()) {
                    self.event_epoch = Some(batch.epoch);
                    self.event_cursor = 0;
                    self.event_cache.clear();
                }
                let next_cursor = batch
                    .events
                    .iter()
                    .map(|event| event.id)
                    .max()
                    .unwrap_or(self.event_cursor)
                    .max(self.event_cursor);
                self.event_cache.extend(
                    batch
                        .events
                        .into_iter()
                        .filter(|event| event.id > self.event_cursor)
                        .map(PresentedEvent::from),
                );
                self.event_cursor = next_cursor;
                if self.event_cache.len() > 500 {
                    let remove = self.event_cache.len() - 500;
                    self.event_cache.drain(..remove);
                }
            }
            Err(_) => {
                self.event_load_error = Some(PresentedEvent {
                    id: 0,
                    timestamp: String::new(),
                    component: "system".into(),
                    level: "error".into(),
                    message: "Не удалось загрузить события. Повторите попытку.".into(),
                });
            }
        }
        Some(self.filtered_events())
    }

    pub(crate) fn begin_catalog_request(&mut self) -> Option<CatalogRequest> {
        if self.catalog_request_pending.is_some() {
            return None;
        }
        if self.active_selection.is_some() {
            self.catalog_request_deferred = true;
            return None;
        }
        self.catalog_request_deferred = false;
        self.next_catalog_generation = self.next_catalog_generation.wrapping_add(1);
        let token = CatalogRequestToken(self.next_catalog_generation);
        self.catalog_request_pending = Some(token);
        if self.catalog_notice_after_refresh.is_none() {
            self.catalog_error = None;
        }
        Some(CatalogRequest {
            client: self.client.clone(),
            token,
        })
    }

    pub(crate) fn latency_presentation(&self) -> LatencyPresentation {
        LatencyPresentation {
            loading: self.latency_request_pending.is_some(),
            queued: self.latency_queued,
            error: self.latency_error.clone(),
        }
    }

    pub(crate) fn begin_latency_request(&mut self) -> Option<LatencyRequest> {
        self.latency_error = None;
        if self.latency_request_pending.is_some() {
            self.latency_queued = true;
            return None;
        }
        if !matches!(self.state, UiState::Connected { .. })
            || self.catalog_revision.is_none()
            || self.selected_catalog_group.is_none()
        {
            self.latency_queued = true;
            return None;
        }
        let group_ids = vec![self.selected_catalog_group.clone()?];
        self.next_latency_generation = self.next_latency_generation.wrapping_add(1);
        let token = LatencyRequestToken(self.next_latency_generation);
        self.latency_request_pending = Some(token);
        self.latency_request_revision = self.catalog_revision;
        self.latency_request_group_ids.clone_from(&group_ids);
        self.latency_queued = false;
        Some(LatencyRequest {
            client: self.client.clone(),
            token,
            catalog_revision: self.catalog_revision,
            group_ids,
        })
    }

    pub(crate) fn finish_latencies(
        &mut self,
        token: LatencyRequestToken,
        result: Result<DaemonLatencies, crate::daemon::DaemonError>,
    ) -> Option<LatencyOutcome> {
        if self.latency_request_pending != Some(token) {
            return None;
        }
        self.latency_request_pending = None;
        let targeted_group_ids = std::mem::take(&mut self.latency_request_group_ids);
        if self.latency_request_revision.take() != self.catalog_revision {
            self.latency_queued = true;
            return Some(LatencyOutcome {
                refresh_catalog: false,
                rerun: true,
            });
        }
        let mut refresh_catalog = false;
        match result {
            Ok(latencies) => {
                for entry in &latencies.entries {
                    if !targeted_group_ids.iter().any(|id| id == &entry.group_id) {
                        continue;
                    }
                    if let Some(node) = self
                        .catalog_groups
                        .iter_mut()
                        .find(|group| group.id == entry.group_id)
                        .and_then(|group| {
                            group.nodes.iter_mut().find(|node| node.id == entry.node_id)
                        })
                    {
                        let (text, tone) = latency_display(&entry.status, entry.latency_ms);
                        node.delay_ms = if entry.status == "ok" {
                            entry.latency_ms
                        } else {
                            None
                        };
                        node.latency_text = text;
                        node.latency_tone = tone;
                    }
                }
                self.latency_error = None;
            }
            Err(error) if error.is_stale_revision() => {
                self.catalog_revision = None;
                self.latency_queued = true;
                self.latency_error =
                    Some("Каталог маршрутов изменился. Обновляем задержку…".into());
                refresh_catalog = true;
            }
            Err(_) => {
                self.latency_error =
                    Some("Не удалось проверить задержку. Повторите попытку.".into());
            }
        }
        Some(LatencyOutcome {
            refresh_catalog,
            rerun: self.latency_queued && !refresh_catalog,
        })
    }

    pub(crate) fn finish_catalog(
        &mut self,
        token: CatalogRequestToken,
        result: Result<DaemonCatalog, crate::daemon::DaemonError>,
    ) -> Option<CatalogPresentation> {
        if self.catalog_request_pending != Some(token) {
            return None;
        }
        self.catalog_request_pending = None;
        match result {
            Ok(catalog) => {
                let reconciliation = self.catalog_reconciliation_required;
                self.replace_catalog(catalog);
                if reconciliation {
                    self.sync_connected_route_from_catalog();
                }
                self.catalog_reconciliation_required = false;
                self.catalog_error = self.catalog_notice_after_refresh.take();
            }
            Err(_) => {
                self.catalog_revision = None;
                self.catalog_reconciliation_required = false;
                self.catalog_error = self
                    .catalog_notice_after_refresh
                    .take()
                    .or_else(|| Some("Не удалось загрузить маршруты. Повторите попытку.".into()));
            }
        }
        Some(self.catalog_presentation())
    }

    pub(crate) fn select_catalog_group(&mut self, id: &str) -> bool {
        if self.catalog_request_pending.is_some()
            || !self.catalog_groups.iter().any(|group| group.id == id)
        {
            return false;
        }
        self.selected_catalog_group = Some(id.to_owned());
        self.latency_queued = true;
        true
    }

    pub(crate) fn begin_node_selection(&mut self, node_id: &str) -> Option<NodeSelectionRequest> {
        if self.active_selection.is_some()
            || self.catalog_request_pending.is_some()
            || self.active_mutation.is_some()
            || self.import_pending
            || self.catalog_reconciliation_required
            || !matches!(
                self.state,
                UiState::Connected { .. } | UiState::Ready { .. }
            )
        {
            return None;
        }
        let revision = self.catalog_revision?;
        let group_id = self.selected_catalog_group.clone()?;
        let group = self
            .catalog_groups
            .iter_mut()
            .find(|group| group.id == group_id)?;
        let target_index = group.nodes.iter().position(|node| node.id == node_id)?;
        if group.nodes[target_index].selected {
            return None;
        }
        let previously_selected_node_ids = group
            .nodes
            .iter()
            .filter(|node| node.selected)
            .map(|node| node.id.clone())
            .collect();
        for node in &mut group.nodes {
            node.selected = false;
        }
        group.nodes[target_index].selected = true;
        let selected_label = group.nodes[target_index].label.clone();
        let updates_connection_summary = group.is_primary;
        if matches!(self.state, UiState::Ready { .. }) {
            let queued = QueuedNodeSelection {
                group_id,
                revision,
                node_id: node_id.to_owned(),
            };
            if let Some(existing) = self
                .queued_selections
                .iter_mut()
                .find(|existing| existing.group_id == queued.group_id)
            {
                *existing = queued;
            } else {
                self.queued_selections.push(queued);
            }
            if updates_connection_summary && let UiState::Ready { node, .. } = &mut self.state {
                *node = selected_label;
            }
            self.catalog_error = None;
            return None;
        }
        self.queued_selections.clear();
        self.next_selection_generation = self.next_selection_generation.wrapping_add(1);
        let token = SelectionRequestToken(self.next_selection_generation);
        self.active_selection = Some(ActiveSelection {
            token,
            group_id: group_id.clone(),
            previously_selected_node_ids,
        });
        self.catalog_error = None;
        Some(NodeSelectionRequest {
            client: self.client.clone(),
            token,
            group_id,
            revision,
            node_id: node_id.to_owned(),
        })
    }

    pub(crate) fn queued_node_selections(&self) -> Vec<QueuedNodeSelection> {
        self.queued_selections.clone()
    }

    pub(crate) fn finish_queued_node_selections(
        &mut self,
        queued: &[QueuedNodeSelection],
        results: Vec<Result<DaemonCatalog, crate::daemon::DaemonError>>,
    ) -> bool {
        if self.queued_selections != queued {
            return false;
        }
        if results.len() != queued.len() {
            return false;
        }
        self.queued_selections.clear();
        let mut failed = false;
        for result in results {
            match result {
                Ok(catalog) => {
                    self.replace_catalog(catalog);
                    self.sync_connected_route_from_catalog();
                }
                Err(_) => failed = true,
            }
        }
        if failed {
            self.catalog_revision = None;
            self.catalog_error = Some(
                "Подключено, но часть маршрутов не применилась. Проверьте выбор ещё раз.".into(),
            );
        } else {
            self.catalog_error = None;
        }
        true
    }

    pub(crate) fn finish_node_selection(
        &mut self,
        token: SelectionRequestToken,
        result: Result<DaemonCatalog, crate::daemon::DaemonError>,
    ) -> Option<SelectionOutcome> {
        if self
            .active_selection
            .as_ref()
            .map(|selection| selection.token)
            != Some(token)
        {
            return None;
        }
        let active = self
            .active_selection
            .take()
            .expect("checked active selection");
        match result {
            Ok(catalog) => {
                self.replace_catalog(catalog);
                if let Some(label) = self
                    .catalog_groups
                    .iter()
                    .find(|group| group.id == active.group_id && group.is_primary)
                    .and_then(|group| group.nodes.iter().find(|node| node.selected))
                    .map(|node| node.label.clone())
                    && let UiState::Connected { node, .. } = &mut self.state
                {
                    *node = label;
                }
                self.catalog_error = None;
                self.catalog_reconciliation_required = true;
                Some(SelectionOutcome {
                    refresh_catalog: true,
                })
            }
            Err(error) => {
                self.restore_selection(&active);
                let stale_revision = error.is_stale_revision();
                let ambiguous = error.requires_reconciliation();
                let refresh_catalog = stale_revision || ambiguous || self.catalog_request_deferred;
                if stale_revision {
                    self.catalog_revision = None;
                    self.catalog_error =
                        Some("Каталог маршрутов изменился. Обновляем его; повторите выбор.".into());
                    self.catalog_notice_after_refresh =
                        Some("Каталог маршрутов обновлён. Повторите выбор.".into());
                } else if ambiguous {
                    self.catalog_revision = None;
                    let message = "Не удалось подтвердить изменение маршрута. Каталог обновляется; повторите попытку.".to_owned();
                    self.catalog_error = Some(message.clone());
                    self.catalog_notice_after_refresh = Some(message);
                } else {
                    let message = "Не удалось изменить маршрут. Повторите попытку.".to_owned();
                    self.catalog_error = Some(message.clone());
                    if refresh_catalog {
                        self.catalog_revision = None;
                        self.catalog_notice_after_refresh = Some(message);
                    }
                }
                self.catalog_reconciliation_required = refresh_catalog;
                Some(SelectionOutcome { refresh_catalog })
            }
        }
    }

    pub(crate) fn catalog_presentation(&self) -> CatalogPresentation {
        let selected = self.selected_catalog_group.as_deref();
        let groups = self
            .catalog_groups
            .iter()
            .map(|group| CatalogGroupPresentation {
                id: group.id.clone(),
                label: group.label.clone(),
                selected: selected == Some(group.id.as_str()),
            })
            .collect();
        let nodes = selected
            .and_then(|id| self.catalog_groups.iter().find(|group| group.id == id))
            .map(|group| group.nodes.clone())
            .unwrap_or_default();
        CatalogPresentation {
            revision: self.catalog_revision,
            groups,
            selected_group_id: self.selected_catalog_group.clone(),
            nodes,
            loading: self.catalog_request_pending.is_some(),
            group_navigation_enabled: !self.catalog_groups.is_empty()
                && self.catalog_request_pending.is_none(),
            selection_enabled: matches!(
                self.state,
                UiState::Connected { .. } | UiState::Ready { .. }
            ) && self.catalog_revision.is_some()
                && self.catalog_request_pending.is_none()
                && self.active_selection.is_none()
                && self.active_mutation.is_none()
                && !self.import_pending
                && !self.catalog_reconciliation_required,
            selection_pending: self.active_selection.is_some(),
            selection_queued: !self.queued_selections.is_empty(),
            error: self.catalog_error.clone(),
        }
    }

    fn replace_catalog(&mut self, catalog: DaemonCatalog) {
        let previous_groups =
            (self.catalog_revision == Some(catalog.revision)).then(|| self.catalog_groups.clone());
        let preserve_queued = self
            .queued_selections
            .iter()
            .all(|queued| queued.revision == catalog.revision);
        if !preserve_queued {
            self.queued_selections.clear();
        }
        self.catalog_revision = Some(catalog.revision);
        let primary_group_id = catalog
            .groups
            .iter()
            .find(|group| group.selected)
            .or_else(|| catalog.groups.first())
            .map(|group| group.id.clone());
        self.selected_catalog_group = self
            .selected_catalog_group
            .as_deref()
            .and_then(|selected| catalog.groups.iter().find(|group| group.id == selected))
            .or_else(|| catalog.groups.iter().find(|group| group.selected))
            .or_else(|| catalog.groups.first())
            .map(|group| group.id.clone());
        self.catalog_groups = catalog
            .groups
            .into_iter()
            .map(|group| {
                let old_group = previous_groups
                    .as_ref()
                    .and_then(|groups| groups.iter().find(|old| old.id == group.id));
                CatalogGroup {
                    is_primary: primary_group_id.as_deref() == Some(group.id.as_str()),
                    id: group.id,
                    label: safe_catalog_label(&group.label),
                    nodes: group
                        .nodes
                        .into_iter()
                        .map(|node| {
                            let old_node = old_group.and_then(|old| {
                                old.nodes.iter().find(|old_node| old_node.id == node.id)
                            });
                            // A real delay from the catalog is authoritative. When a
                            // same-revision reload omits it, preserve the complete last-known
                            // display tuple so internal data and visible text cannot diverge.
                            let (delay_ms, latency_text, latency_tone) =
                                if let Some(delay) = node.delay_ms {
                                    let (text, tone) = latency_display("ok", Some(delay));
                                    (Some(delay), text, tone)
                                } else if let Some(old) = old_node {
                                    (
                                        old.delay_ms,
                                        old.latency_text.clone(),
                                        old.latency_tone.clone(),
                                    )
                                } else {
                                    (None, String::new(), "neutral".into())
                                };
                            CatalogNodePresentation {
                                id: node.id,
                                label: safe_catalog_label(&node.label),
                                selected: node.selected,
                                delay_ms,
                                latency_text,
                                latency_tone,
                            }
                        })
                        .collect(),
                }
            })
            .collect();
        if !self.catalog_groups.is_empty() {
            self.latency_queued = true;
        }
        if preserve_queued {
            for queued in &self.queued_selections {
                if let Some(group) = self
                    .catalog_groups
                    .iter_mut()
                    .find(|group| group.id == queued.group_id)
                    && let Some(target_index) = group
                        .nodes
                        .iter()
                        .position(|node| node.id == queued.node_id)
                {
                    for node in &mut group.nodes {
                        node.selected = false;
                    }
                    group.nodes[target_index].selected = true;
                    if group.is_primary
                        && let UiState::Ready { node, .. } = &mut self.state
                    {
                        *node = group.nodes[target_index].label.clone();
                    }
                }
            }
        }
    }

    fn restore_selection(&mut self, active: &ActiveSelection) {
        if let Some(group) = self
            .catalog_groups
            .iter_mut()
            .find(|group| group.id == active.group_id)
        {
            for node in &mut group.nodes {
                node.selected = active
                    .previously_selected_node_ids
                    .iter()
                    .any(|id| id == &node.id);
            }
        }
    }

    fn sync_connected_route_from_catalog(&mut self) {
        let label = self
            .catalog_groups
            .iter()
            .find(|group| group.is_primary)
            .and_then(|group| group.nodes.iter().find(|node| node.selected))
            .map(|node| node.label.clone());
        if let (Some(label), UiState::Connected { node, .. }) = (label, &mut self.state) {
            *node = label;
        }
    }

    fn filtered_events(&self) -> Vec<PresentedEvent> {
        let mut events = self
            .diagnostics
            .as_ref()
            .map(|diagnostics| {
                diagnostics
                    .logs
                    .iter()
                    .map(|record| PresentedEvent {
                        id: record.id,
                        timestamp: record.timestamp.clone(),
                        component: match record.component.as_str() {
                            "xray" => "xray",
                            "mihomo" => "mihomo",
                            _ => "core",
                        }
                        .to_owned(),
                        level: match record.level.as_str() {
                            "error" => "error",
                            "warning" => "warning",
                            _ => "info",
                        }
                        .to_owned(),
                        message: redact_event_text(&record.message),
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        events.extend(self.event_cache.clone());
        if let Some(error) = &self.diagnostics_error {
            events.insert(0, error.clone());
        }
        if let Some(error) = &self.event_load_error {
            events.insert(0, error.clone());
        }
        match self.event_filter {
            EventFilter::All => {}
            EventFilter::System => {
                events.retain(|event| !event.level.eq_ignore_ascii_case("error"));
            }
            EventFilter::Error => {
                events.retain(|event| event.level.eq_ignore_ascii_case("error"));
            }
        }
        events
    }
}

fn present_runtime_check(value: Option<&str>, loading: bool) -> RuntimeCheckPresentation {
    if loading && value.is_none() {
        return RuntimeCheckPresentation {
            value: "Проверка".into(),
            tone: "accent".into(),
        };
    }
    let (value, tone) = match value {
        Some("ready") => ("Готов", "success"),
        Some("starting") => ("Запуск", "warning"),
        Some("failed") => ("Сбой", "danger"),
        Some("unsupported") => ("Без проверки", "neutral"),
        Some("stopped") => ("Выкл", "neutral"),
        Some(_) => ("Неизвестно", "danger"),
        None => ("Нет данных", "neutral"),
    };
    RuntimeCheckPresentation {
        value: value.into(),
        tone: tone.into(),
    }
}

fn latency_display(status: &str, latency_ms: Option<u64>) -> (String, String) {
    match (status, latency_ms) {
        ("ok", Some(value)) => {
            let tone = if value < 80 {
                "success"
            } else if value < 150 {
                "warning"
            } else {
                "danger"
            };
            (format!("{value} ms"), tone.into())
        }
        ("timeout", _) => ("таймаут".into(), "danger".into()),
        ("error" | "unavailable", _) => ("нет связи".into(), "danger".into()),
        _ => ("нет данных".into(), "neutral".into()),
    }
}

fn safe_catalog_label(label: &str) -> String {
    let label = redact_event_text(label)
        .replace(['\r', '\n'], " ")
        .trim()
        .to_owned();
    if label.is_empty() {
        "Без названия".into()
    } else {
        label
    }
}

fn current_unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn format_gib(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1024_f64.powi(3))
}

fn bounded_plain_text(value: &str, max_chars: usize) -> Option<String> {
    let filtered = value
        .chars()
        .filter(|character| {
            !character.is_control()
                && !matches!(
                    character,
                    '\u{061c}'
                        | '\u{200e}'
                        | '\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                )
        })
        .collect::<String>();
    let bounded = filtered
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect::<String>();
    (!bounded.is_empty()).then_some(bounded)
}

fn is_non_file_reference(value: &str) -> bool {
    let lowercase = value.to_ascii_lowercase();
    value.contains("://") || lowercase.starts_with("data:")
}

#[cfg(windows)]
fn local_logo_path(value: &str) -> Option<String> {
    use std::path::{Component, Prefix};

    let value = value.trim();
    if value.is_empty() || is_non_file_reference(value) {
        return None;
    }
    let path = Path::new(value);
    let is_local_drive = matches!(
        path.components().next(),
        Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_))
    );
    (is_local_drive && path.is_absolute()).then(|| value.to_owned())
}

#[cfg(not(windows))]
fn local_logo_path(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || is_non_file_reference(value) {
        return None;
    }
    Path::new(value).is_absolute().then(|| value.to_owned())
}

fn russian_days(days: u64) -> &'static str {
    let last_two = days % 100;
    let last = days % 10;
    if (11..=14).contains(&last_two) {
        "дней"
    } else if last == 1 {
        "день"
    } else if (2..=4).contains(&last) {
        "дня"
    } else {
        "дней"
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CatalogDisplayKind {
    Group,
    Node,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CatalogDisplay {
    pub(crate) icon: String,
    pub(crate) label: String,
}

pub(crate) fn derive_catalog_display(label: &str, kind: CatalogDisplayKind) -> CatalogDisplay {
    let value = label.trim();
    let mut characters = value.char_indices();
    if let (Some((_, first)), Some((second_index, second))) = (characters.next(), characters.next())
        && is_regional_indicator(first)
        && is_regional_indicator(second)
    {
        let end = second_index + second.len_utf8();
        return CatalogDisplay {
            icon: value[..end].to_owned(),
            label: trim_catalog_decorators(&value[end..]).to_owned(),
        };
    }

    for prefix in [
        "🌍", "🌎", "🌏", "🌐", "🎮", "🔄", "↻", "▶️", "▶", "📺", "🎬", "⛔", "🚫", "↘", "⚙️", "⚙",
    ] {
        if let Some(rest) = value.strip_prefix(prefix) {
            return CatalogDisplay {
                icon: prefix.to_owned(),
                label: trim_catalog_decorators(rest).to_owned(),
            };
        }
    }

    let lowercase = value.to_lowercase();
    let icon = if lowercase.contains("minecraft")
        || lowercase.contains("game")
        || lowercase.contains("игр")
    {
        "🎮"
    } else if lowercase.contains("youtube")
        || lowercase.contains("video")
        || lowercase.contains("media")
        || lowercase.contains("стрим")
    {
        "▶"
    } else if lowercase.contains("без vpn")
        || lowercase.contains("direct")
        || lowercase.contains("прям")
    {
        "↗"
    } else if lowercase.contains("auto") || lowercase.contains("авто") {
        "↻"
    } else {
        match kind {
            CatalogDisplayKind::Group => "◎",
            CatalogDisplayKind::Node => "⚙",
        }
    };

    CatalogDisplay {
        icon: icon.to_owned(),
        label: value.to_owned(),
    }
}

fn is_regional_indicator(character: char) -> bool {
    ('\u{1f1e6}'..='\u{1f1ff}').contains(&character)
}

fn trim_catalog_decorators(value: &str) -> &str {
    let trimmed = value.trim_start_matches(|character: char| {
        character.is_whitespace()
            || matches!(
                character,
                '-' | '–' | '—' | ':' | '|' | '·' | '_' | '/' | '.' | ','
            )
    });
    if trimmed.is_empty() {
        value.trim()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Mutex, mpsc},
        thread,
    };

    use super::*;
    use crate::daemon::{
        DaemonCatalog, DaemonCatalogGroup, DaemonCatalogNode, DaemonDiagnosticLog, DaemonError,
        DaemonLatencies, DaemonLatencyEntry, DaemonProxyMapping, DaemonSubscriptionInfo,
        test_support::MockDaemonClient,
    };

    #[test]
    fn latency_check_queues_while_disconnected_and_allows_only_one_request_when_connected() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let refresh = view_model.begin_refresh();
        view_model.finish_refresh(refresh, Ok(MockDaemonClient::ready().status));

        assert!(view_model.begin_latency_request().is_none());
        assert!(view_model.latency_presentation().queued);

        let connect = view_model.begin_primary_action().expect("connect").1;
        view_model.finish_connect(
            connect,
            Ok(DaemonStatus {
                state: "connected".into(),
                profile: Some("Тестовый".into()),
                current_node: Some("Авто".into()),
                ..DaemonStatus::default()
            }),
        );
        let catalog = view_model.begin_catalog_request().expect("catalog request");
        view_model
            .finish_catalog(catalog.token, Ok(catalog_fixture()))
            .expect("catalog completion");
        let request = view_model
            .begin_latency_request()
            .expect("queued latency request after connect");
        assert_eq!(request.group_ids, vec!["opaque:selected"]);
        assert!(view_model.begin_latency_request().is_none());
        assert!(view_model.latency_presentation().loading);
        assert!(view_model.latency_presentation().queued);

        let outcome = view_model
            .finish_latencies(request.token, Ok(DaemonLatencies::default()))
            .expect("first completion");
        assert!(outcome.rerun);
        assert!(view_model.latency_presentation().queued);

        let rerun = view_model
            .begin_latency_request()
            .expect("one coalesced rerun");
        let outcome = view_model
            .finish_latencies(rerun.token, Ok(DaemonLatencies::default()))
            .expect("rerun completion");
        assert!(!outcome.rerun);
        assert!(!view_model.latency_presentation().queued);
    }

    #[test]
    fn first_successful_catalog_load_queues_selected_group_latency() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let refresh = view_model.begin_refresh();
        view_model.finish_refresh(
            refresh,
            Ok(DaemonStatus {
                state: "connected".into(),
                profile: Some("Тестовый".into()),
                current_node: Some("Авто".into()),
                ..DaemonStatus::default()
            }),
        );
        assert!(!view_model.latency_presentation().queued);

        let catalog = view_model
            .begin_catalog_request()
            .expect("first catalog request");
        view_model
            .finish_catalog(catalog.token, Ok(catalog_fixture()))
            .expect("first catalog completion");

        assert!(view_model.latency_presentation().queued);
        let request = view_model
            .begin_latency_request()
            .expect("automatic selected-group latency");
        assert_eq!(request.catalog_revision, Some(91));
        assert_eq!(request.group_ids, vec!["opaque:selected"]);
    }

    #[test]
    fn latency_tone_boundaries_are_stable() {
        assert_eq!(
            latency_display("ok", Some(79)),
            ("79 ms".into(), "success".into())
        );
        assert_eq!(
            latency_display("ok", Some(80)),
            ("80 ms".into(), "warning".into())
        );
        assert_eq!(
            latency_display("ok", Some(149)),
            ("149 ms".into(), "warning".into())
        );
        assert_eq!(
            latency_display("ok", Some(150)),
            ("150 ms".into(), "danger".into())
        );
        assert_eq!(latency_display("timeout", None).1, "danger");
        assert_eq!(latency_display("unavailable", None).1, "danger");
        assert_eq!(latency_display("unknown", None).1, "neutral");
    }

    #[test]
    fn targeted_latency_updates_preserve_other_groups_and_missing_entries() {
        let mut view_model = connected_view_model_with_catalog();
        let first = view_model
            .begin_latency_request()
            .expect("selected group request");
        view_model
            .finish_latencies(
                first.token,
                Ok(DaemonLatencies {
                    entries: vec![DaemonLatencyEntry {
                        group_id: "opaque:selected".into(),
                        node_id: "opaque:node-b".into(),
                        latency_ms: Some(79),
                        status: "ok".into(),
                    }],
                }),
            )
            .expect("selected group completion");
        let selected = view_model.catalog_presentation();
        assert_eq!(selected.nodes[0].latency_text, "79 ms");
        assert_eq!(selected.nodes[0].latency_tone, "success");
        assert_eq!(selected.nodes[0].delay_ms, Some(79));
        assert_eq!(selected.nodes[1].latency_text, "51 ms");
        assert_eq!(selected.nodes[1].delay_ms, Some(51));

        assert!(view_model.select_catalog_group("opaque:first"));
        let second = view_model
            .begin_latency_request()
            .expect("new group request");
        assert_eq!(second.group_ids, vec!["opaque:first"]);
        view_model
            .finish_latencies(second.token, Ok(DaemonLatencies::default()))
            .expect("empty targeted completion");
        assert_eq!(view_model.catalog_presentation().nodes[0].latency_text, "");

        assert!(view_model.select_catalog_group("opaque:selected"));
        let selected = view_model.catalog_presentation();
        assert_eq!(selected.nodes[0].latency_text, "79 ms");
        assert_eq!(selected.nodes[0].delay_ms, Some(79));
        assert_eq!(selected.nodes[1].latency_text, "51 ms");
    }

    #[test]
    fn same_revision_reload_preserves_omitted_latency_and_changed_revision_uses_fresh_data() {
        let mut view_model = connected_view_model_with_catalog();
        let request = view_model.begin_latency_request().expect("latency request");
        view_model
            .finish_latencies(
                request.token,
                Ok(DaemonLatencies {
                    entries: vec![DaemonLatencyEntry {
                        group_id: "opaque:selected".into(),
                        node_id: "opaque:node-b".into(),
                        latency_ms: Some(42),
                        status: "ok".into(),
                    }],
                }),
            )
            .expect("latency completion");

        let same = view_model.begin_catalog_request().expect("same revision");
        let mut same_fixture = catalog_fixture();
        same_fixture.groups[1].nodes[0].delay_ms = None;
        view_model
            .finish_catalog(same.token, Ok(same_fixture))
            .expect("same catalog");
        assert_eq!(
            view_model.catalog_presentation().nodes[0].latency_text,
            "42 ms"
        );
        assert_eq!(
            view_model.catalog_presentation().nodes[0].delay_ms,
            Some(42)
        );

        let changed = view_model
            .begin_catalog_request()
            .expect("changed revision");
        let mut fixture = catalog_fixture();
        fixture.revision += 1;
        view_model
            .finish_catalog(changed.token, Ok(fixture))
            .expect("changed catalog");
        assert_eq!(
            view_model.catalog_presentation().nodes[0].latency_text,
            "37 ms"
        );
        assert_eq!(
            view_model.catalog_presentation().nodes[0].delay_ms,
            Some(37)
        );
        assert_eq!(
            view_model.catalog_presentation().nodes[0].latency_tone,
            "success"
        );

        let cleared = view_model
            .begin_catalog_request()
            .expect("cleared revision");
        let mut cleared_fixture = catalog_fixture();
        cleared_fixture.revision += 2;
        cleared_fixture.groups[1].nodes[0].delay_ms = None;
        view_model
            .finish_catalog(cleared.token, Ok(cleared_fixture))
            .expect("cleared catalog");
        assert_eq!(view_model.catalog_presentation().nodes[0].latency_text, "");
        assert_eq!(view_model.catalog_presentation().nodes[0].delay_ms, None);
        assert_eq!(
            view_model.catalog_presentation().nodes[0].latency_tone,
            "neutral"
        );
    }

    #[test]
    fn successful_subscription_refresh_queues_automatic_latency_check() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let refresh = view_model.begin_refresh();
        view_model.finish_refresh(
            refresh,
            Ok(DaemonStatus {
                state: "ready".into(),
                profile: Some("Тестовый".into()),
                current_node: Some("Авто".into()),
                subscription: Some(DaemonSubscriptionInfo {
                    refresh_available: true,
                    ..DaemonSubscriptionInfo::default()
                }),
                ..DaemonStatus::default()
            }),
        );
        let refresh = view_model
            .begin_subscription_refresh()
            .expect("subscription refresh");
        assert!(view_model.finish_subscription_refresh(
            refresh,
            Ok(DaemonStatus {
                state: "ready".into(),
                profile: Some("Тестовый".into()),
                current_node: Some("Авто".into()),
                ..DaemonStatus::default()
            })
        ));
        assert!(view_model.latency_presentation().queued);
    }

    #[test]
    fn latency_results_are_compact_safe_and_stale_completions_are_ignored() {
        let mut view_model = connected_view_model_with_catalog();
        let first = view_model.begin_latency_request().expect("first request");
        view_model
            .finish_latencies(
                first.token,
                Ok(DaemonLatencies {
                    entries: vec![
                        DaemonLatencyEntry {
                            group_id: "opaque:selected".into(),
                            node_id: "opaque:node-b".into(),
                            latency_ms: Some(42),
                            status: "ok".into(),
                        },
                        DaemonLatencyEntry {
                            group_id: "opaque:selected".into(),
                            node_id: "opaque:node-c".into(),
                            latency_ms: None,
                            status: "timeout".into(),
                        },
                    ],
                }),
            )
            .expect("current completion");
        let shown = view_model.catalog_presentation();
        assert_eq!(shown.nodes[0].latency_text, "42 ms");
        assert_eq!(shown.nodes[1].latency_text, "таймаут");
        assert_eq!(shown.nodes[0].delay_ms, Some(42));
        assert_eq!(shown.nodes[1].delay_ms, None);

        let newer = view_model.begin_latency_request().expect("new request");
        assert!(
            view_model
                .finish_latencies(
                    first.token,
                    Err(DaemonError::new("private upstream detail"))
                )
                .is_none()
        );
        assert!(view_model.latency_presentation().loading);
        view_model
            .finish_latencies(
                newer.token,
                Err(DaemonError::new("private upstream detail")),
            )
            .expect("current failure");
        assert_eq!(
            view_model.latency_presentation().error.as_deref(),
            Some("Не удалось проверить задержку. Повторите попытку.")
        );
        assert!(!format!("{:?}", view_model.latency_presentation()).contains("private"));
    }

    #[test]
    fn latency_completion_for_an_older_catalog_revision_is_suppressed() {
        let mut view_model = connected_view_model_with_catalog();
        let latency = view_model.begin_latency_request().expect("latency request");
        let catalog = view_model.begin_catalog_request().expect("catalog refresh");
        let mut replacement = catalog_fixture();
        replacement.revision += 1;
        view_model
            .finish_catalog(catalog.token, Ok(replacement))
            .expect("new catalog");

        view_model
            .finish_latencies(
                latency.token,
                Ok(DaemonLatencies {
                    entries: vec![DaemonLatencyEntry {
                        group_id: "opaque:selected".into(),
                        node_id: "opaque:node-b".into(),
                        latency_ms: Some(999),
                        status: "ok".into(),
                    }],
                }),
            )
            .expect("stale completion is consumed without applying it");

        assert_eq!(
            view_model.catalog_presentation().nodes[0].latency_text,
            "37 ms"
        );
        assert!(!view_model.latency_presentation().loading);
    }

    #[test]
    fn stale_latency_revision_invalidates_catalog_and_queues_safe_retry() {
        let mut view_model = connected_view_model_with_catalog();
        let latency = view_model.begin_latency_request().expect("latency request");

        let outcome = view_model
            .finish_latencies(latency.token, Err(DaemonError::stale_revision()))
            .expect("current stale completion");

        assert!(outcome.refresh_catalog);
        assert!(view_model.catalog_presentation().revision.is_none());
        assert!(view_model.latency_presentation().queued);
        assert_eq!(
            view_model.latency_presentation().error.as_deref(),
            Some("Каталог маршрутов изменился. Обновляем задержку…")
        );
    }

    #[test]
    fn same_revision_catalog_reload_preserves_and_reapplies_queued_choice_for_connect() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let refresh = view_model.begin_refresh();
        view_model.finish_refresh(refresh, Ok(MockDaemonClient::ready().status));
        let catalog = view_model.begin_catalog_request().expect("catalog request");
        view_model.finish_catalog(catalog.token, Ok(catalog_fixture()));
        assert!(view_model.select_catalog_group("opaque:selected"));
        assert!(view_model.begin_node_selection("opaque:node-c").is_none());

        let reload = view_model
            .begin_catalog_request()
            .expect("same revision reload");
        view_model
            .finish_catalog(reload.token, Ok(catalog_fixture()))
            .expect("catalog completion");

        let presentation = view_model.catalog_presentation();
        assert!(presentation.selection_queued);
        assert!(presentation.nodes[1].selected);
        assert!(!presentation.nodes[0].selected);
        let queued = view_model.queued_node_selections();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].revision, 91);
        assert_eq!(queued[0].node_id, "opaque:node-c");
        assert_eq!(
            view_model
                .begin_primary_action()
                .expect("connect queued choice")
                .0,
            PrimaryAction::Connect
        );
    }

    #[test]
    fn changed_catalog_revision_discards_queued_choice() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let refresh = view_model.begin_refresh();
        view_model.finish_refresh(refresh, Ok(MockDaemonClient::ready().status));
        let catalog = view_model.begin_catalog_request().expect("catalog request");
        view_model.finish_catalog(catalog.token, Ok(catalog_fixture()));
        assert!(view_model.begin_node_selection("opaque:node-c").is_none());

        let reload = view_model
            .begin_catalog_request()
            .expect("changed revision reload");
        let mut changed = catalog_fixture();
        changed.revision += 1;
        view_model
            .finish_catalog(reload.token, Ok(changed))
            .expect("changed catalog completion");

        assert!(view_model.queued_node_selections().is_empty());
        assert!(!view_model.catalog_presentation().selection_queued);
        assert!(view_model.catalog_presentation().nodes[0].selected);
    }

    #[test]
    fn compact_latency_ui_source_contract() {
        let app = include_str!("../ui/app.slint");
        let main = include_str!("main.rs");
        assert!(!app.contains("callback check-latencies();"));
        assert!(!app.contains("latency-check-enabled"));
        assert!(app.contains("Задержка обновляется автоматически"));
        assert!(app.contains("trailing-text: node.latency-text;"));
        assert!(!main.contains("ui.on_check_latencies"));
        assert!(main.contains("TimerMode::Repeated"));
        assert!(main.contains("Duration::from_secs(60)"));
        assert!(main.contains("ui.window().is_visible()"));
        assert!(main.contains("if current.latency.queued"));
        assert!(app.contains("trailing-tone: node.latency-tone;"));
        assert!(app.matches("background: #ffffff05;").count() >= 4);
        assert!(app.contains("height: 54px;"));
        assert!(app.contains("border-color: root.status-tone == \"success\""));
        assert!(include_str!("../ui/components.slint").contains("width: 76px;"));
        let route_row = include_str!("../ui/components.slint")
            .split("export component RouteNodeRow")
            .nth(1)
            .expect("route row source");
        assert!(route_row.contains("x: parent.width - self.width - 40px;"));
        assert!(route_row.contains("parent.width - self.x - trailing-label.width - 48px"));
        assert!(!route_row.contains("trailing-label.width - (root.selected"));
        assert!(!route_row.contains("self.width - (root.selected"));
        assert!(route_row.contains("root.pending ? \"…\" : root.selected ? \"✓\" : \"\""));
        assert!(!route_row.contains("root.selected ? \"✓\" : \"…\""));
        assert!(main.contains("run_latency_check"));
        assert!(main.contains("if outcome.refresh_catalog"));
    }

    #[test]
    fn one_page_home_shell_avoids_routes_blank_and_layout_regressions() {
        let app = include_str!("../ui/app.slint");
        let components = include_str!("../ui/components.slint");
        let main = include_str!("main.rs");

        assert!(app.contains(
            "root.active-panel == \"routes\" ? \"home\" : root.active-panel == \"events\" ? \"status\" : root.local-page"
        ));
        assert!(!app.contains("root.active-panel == \"routes\" ? \"routes\""));
        assert!(!app.contains("if root.visible-page == \"routes\""));
        assert!(main.contains("if panel == Panel::Routes"));
        assert!(main.contains("ui.set_local_page(\"home\".into());"));
        assert!(main.contains("load_catalog(weak.clone(), open_model.clone());"));

        assert!(!app.contains("Text { text: \"Главная\";"));
        assert!(!app.contains("profile-name"));
        assert!(!main.contains("set_profile_name"));
        assert!(
            app.contains("height: 60px;\n                                horizontal-stretch: 1;")
        );
        assert!(app.contains("if root.has-service-logo: Rectangle"));
        assert!(app.contains("source: @image-url(\"../assets/power.svg\")"));

        let shelf = app
            .split("height: 54px;")
            .nth(1)
            .and_then(|source| {
                source
                    .split("if root.subscription-announcement-text")
                    .next()
            })
            .expect("subscription shelf source");
        assert!(shelf.contains("border-radius: 16px;"));
        assert!(shelf.contains("root.subscription-refresh-error != \"\""));
        assert!(!shelf.contains("if root.subscription-refresh-error != \"\": Text"));

        assert!(!components.contains("#0091ff"));
        assert!(components.contains("#4c9dff1f"));
        assert!(components.contains("#4c9dff66"));
    }

    #[test]
    fn state_eyebrows_are_sentence_case_and_ready_has_one_route_label() {
        let states = [
            UiState::Empty,
            UiState::Importing,
            UiState::Ready {
                profile: "Работа".into(),
                node: "Авто".into(),
            },
            UiState::Connecting {
                profile: "Работа".into(),
                node: "Авто".into(),
                step: "Запуск".into(),
            },
            UiState::Connected {
                profile: "Работа".into(),
                node: "nl-01".into(),
            },
            UiState::Disconnecting {
                profile: "Работа".into(),
                node: "nl-01".into(),
            },
            UiState::Error {
                message: "Ошибка".into(),
            },
            UiState::DaemonDegraded {
                message: "Недоступно".into(),
            },
        ];
        for state in states {
            let eyebrow = state.presentation().eyebrow;
            assert_ne!(
                eyebrow,
                eyebrow.to_uppercase(),
                "all-caps eyebrow: {eyebrow}"
            );
        }
        let ready = UiState::Ready {
            profile: "Работа".into(),
            node: "Авто".into(),
        }
        .presentation();
        assert_eq!(ready.eyebrow, "Готово");
        assert_eq!(ready.node, "Авто");
        assert_eq!(ready.supporting, "");

        for state in [UiState::Empty, UiState::Importing] {
            assert_eq!(state.presentation().node, "Маршрут не выбран");
        }
    }

    #[test]
    fn subscription_presentation_uses_real_usage_expiry_and_safe_source_name() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let refresh = view_model.begin_refresh();
        view_model.finish_refresh(
            refresh,
            Ok(DaemonStatus {
                state: "ready".into(),
                profile: Some("Основной".into()),
                current_node: Some("Auto Sweden".into()),
                subscription: Some(DaemonSubscriptionInfo {
                    source_name: "subscription.example".into(),
                    display_name: "Provider One".into(),
                    uploaded_bytes: Some(u64::MAX),
                    downloaded_bytes: Some(938_375_741_110),
                    used_bytes: Some(u64::MAX),
                    total_bytes: None,
                    expires_at_unix: Some(1_792_851_157),
                    updated_at_unix: 1_789_405_200,
                    refresh_available: true,
                    ..DaemonSubscriptionInfo::default()
                }),
                ..DaemonStatus::default()
            }),
        );

        let subscription = view_model.subscription_presentation_at(1_789_405_200);
        assert_eq!(subscription.display_name, "Provider One");
        assert_eq!(subscription.usage, "17179869184.0 GB");
        assert_eq!(subscription.expiry, "Осталось 40 дней");
        assert!(subscription.refresh_available);
        assert!(!format!("{subscription:?}").contains("https://"));
    }

    #[test]
    fn subscription_presentation_does_not_invent_unlimited_usage_or_expiry() {
        let mut client = MockDaemonClient::ready();
        client.status.subscription = Some(DaemonSubscriptionInfo::default());
        let mut view_model = DesktopViewModel::new(Arc::new(client));
        let refresh = view_model.begin_refresh();
        let status = view_model.daemon_client().status().expect("status");
        view_model.finish_refresh(refresh, Ok(status));

        let subscription = view_model.subscription_presentation_at(0);
        assert_eq!(subscription.usage, "Нет данных");
        assert_eq!(subscription.expiry, "Срок неизвестен");
    }

    #[test]
    fn subscription_presentation_preserves_legitimate_total_only_metadata() {
        let mut client = MockDaemonClient::ready();
        client.status.subscription = Some(DaemonSubscriptionInfo {
            total_bytes: Some(10 * 1024 * 1024 * 1024),
            ..DaemonSubscriptionInfo::default()
        });
        let mut view_model = DesktopViewModel::new(Arc::new(client));
        let refresh = view_model.begin_refresh();
        let status = view_model.daemon_client().status().expect("status");
        view_model.finish_refresh(refresh, Ok(status));

        let subscription = view_model.subscription_presentation_at(0);
        assert_eq!(subscription.usage, "— / 10.0 GB");
        assert_eq!(subscription.expiry, "Срок неизвестен");
    }

    #[cfg(windows)]
    #[test]
    fn local_logo_path_accepts_only_fully_qualified_local_drive_paths_on_windows() {
        assert_eq!(
            local_logo_path(r"C:\cache\provider\logo.png").as_deref(),
            Some(r"C:\cache\provider\logo.png")
        );
        for rejected in [
            r"\\server\share\logo.png",
            r"\\?\UNC\server\share\logo.png",
            r"\\.\device\logo.png",
            r"\rooted\logo.png",
            r"C:relative\logo.png",
            "https://provider.invalid/logo.png",
            "HTTP://provider.invalid/logo.png",
            "data:image/png;base64,AA==",
        ] {
            assert_eq!(local_logo_path(rejected), None, "accepted {rejected}");
        }
    }

    #[test]
    fn subscription_presentation_bounds_metadata_and_rejects_remote_logo_urls() {
        let mut client = MockDaemonClient::ready();
        client.status.subscription = Some(DaemonSubscriptionInfo {
            source_name: "fallback.example".into(),
            display_name: format!("  {}  ", "P".repeat(200)),
            announcement_text: Some(format!("  Notice\n{}  ", "x".repeat(600))),
            announcement_tone: Some("provider-purple".into()),
            service_logo_path: Some("https://provider.invalid/logo.png".into()),
            ..DaemonSubscriptionInfo::default()
        });
        let mut view_model = DesktopViewModel::new(Arc::new(client));
        let refresh = view_model.begin_refresh();
        let status = view_model.daemon_client().status().expect("status");
        view_model.finish_refresh(refresh, Ok(status));

        let subscription = view_model.subscription_presentation_at(0);
        assert_eq!(subscription.display_name.chars().count(), 128);
        assert_eq!(subscription.announcement_text.chars().count(), 512);
        assert_eq!(subscription.announcement_tone, "info");
        assert_eq!(subscription.service_logo_path, None);
    }

    #[test]
    fn subscription_presentation_falls_back_to_host_and_keeps_local_logo_path() {
        let logo = tempfile::tempdir()
            .expect("logo temp directory")
            .path()
            .join("provider.png");
        let mut client = MockDaemonClient::ready();
        client.status.subscription = Some(DaemonSubscriptionInfo {
            source_name: "fallback.example".into(),
            display_name: " \n ".into(),
            uploaded_bytes: Some(u64::MAX),
            downloaded_bytes: Some(42),
            total_bytes: Some(u64::MAX),
            announcement_tone: Some("red".into()),
            service_logo_path: Some(logo.to_string_lossy().into_owned()),
            ..DaemonSubscriptionInfo::default()
        });
        let mut view_model = DesktopViewModel::new(Arc::new(client));
        let refresh = view_model.begin_refresh();
        let status = view_model.daemon_client().status().expect("status");
        view_model.finish_refresh(refresh, Ok(status));

        let subscription = view_model.subscription_presentation_at(0);
        assert_eq!(subscription.display_name, "fallback.example");
        assert_eq!(subscription.usage, "17179869184.0 GB / 17179869184.0 GB");
        assert_eq!(subscription.announcement_tone, "danger");
        assert_eq!(subscription.service_logo_path.as_deref(), logo.to_str());
    }

    #[test]
    fn failed_subscription_refresh_stays_ready_and_reports_inline() {
        let mut client = MockDaemonClient::ready();
        client.status.subscription = Some(DaemonSubscriptionInfo {
            source_name: "subscription.example".into(),
            refresh_available: true,
            ..DaemonSubscriptionInfo::default()
        });
        let mut view_model = DesktopViewModel::new(Arc::new(client));
        let startup = view_model.begin_refresh();
        view_model.finish_refresh(
            startup,
            Ok(DaemonStatus {
                state: "ready".into(),
                profile: Some("Основной".into()),
                subscription: Some(DaemonSubscriptionInfo {
                    source_name: "subscription.example".into(),
                    refresh_available: true,
                    ..DaemonSubscriptionInfo::default()
                }),
                ..DaemonStatus::default()
            }),
        );
        let refresh = view_model
            .begin_subscription_refresh()
            .expect("refresh from ready state");
        assert!(view_model.subscription_presentation().refresh_pending);
        assert!(!view_model.finish_subscription_refresh(
            refresh,
            Err(DaemonError::new("unsafe url=https://private.invalid")),
        ));
        assert!(matches!(view_model.state, UiState::Ready { .. }));
        let subscription = view_model.subscription_presentation();
        assert_eq!(
            subscription.error.as_deref(),
            Some("Не удалось обновить. Рабочий профиль сохранён.")
        );
        assert!(!format!("{subscription:?}").contains("private.invalid"));
    }

    struct DelayedDaemonClient {
        import_started: Mutex<Option<mpsc::Sender<()>>>,
        import_release: Mutex<mpsc::Receiver<()>>,
        events_started: Mutex<Option<mpsc::Sender<()>>>,
        events_release: Mutex<mpsc::Receiver<()>>,
    }

    type DelayedClientHarness = (
        Arc<DelayedDaemonClient>,
        mpsc::Receiver<()>,
        mpsc::Sender<()>,
        mpsc::Receiver<()>,
        mpsc::Sender<()>,
    );

    impl DelayedDaemonClient {
        fn harness() -> DelayedClientHarness {
            let (import_started_tx, import_started_rx) = mpsc::channel();
            let (import_release_tx, import_release_rx) = mpsc::channel();
            let (events_started_tx, events_started_rx) = mpsc::channel();
            let (events_release_tx, events_release_rx) = mpsc::channel();
            (
                Arc::new(Self {
                    import_started: Mutex::new(Some(import_started_tx)),
                    import_release: Mutex::new(import_release_rx),
                    events_started: Mutex::new(Some(events_started_tx)),
                    events_release: Mutex::new(events_release_rx),
                }),
                import_started_rx,
                import_release_tx,
                events_started_rx,
                events_release_tx,
            )
        }

        fn ready_status() -> DaemonStatus {
            DaemonStatus {
                state: "ready".into(),
                profile: Some("Тестовый".into()),
                current_node: Some("Авто".into()),
                ..DaemonStatus::default()
            }
        }
    }

    impl DaemonClient for DelayedDaemonClient {
        fn status(&self) -> Result<DaemonStatus, DaemonError> {
            Ok(Self::ready_status())
        }

        fn import_subscription(&self, _url: &str) -> Result<DaemonStatus, DaemonError> {
            if let Some(started) = self
                .import_started
                .lock()
                .expect("import started lock")
                .take()
            {
                started.send(()).expect("announce import start");
            }
            self.import_release
                .lock()
                .expect("import release lock")
                .recv()
                .expect("release delayed import");
            Ok(Self::ready_status())
        }

        fn connect(&self) -> Result<DaemonStatus, DaemonError> {
            Ok(Self::ready_status())
        }

        fn disconnect(&self) -> Result<DaemonStatus, DaemonError> {
            Ok(Self::ready_status())
        }

        fn events(
            &self,
            _after: u64,
            _known_epoch: Option<&str>,
        ) -> Result<DaemonEventBatch, DaemonError> {
            if let Some(started) = self
                .events_started
                .lock()
                .expect("events started lock")
                .take()
            {
                started.send(()).expect("announce events start");
            }
            self.events_release
                .lock()
                .expect("events release lock")
                .recv()
                .expect("release delayed events");
            Ok(DaemonEventBatch {
                epoch: "delayed".into(),
                events: Vec::new(),
            })
        }

        fn catalog(&self) -> Result<DaemonCatalog, DaemonError> {
            Ok(DaemonCatalog::default())
        }

        fn select_node(
            &self,
            _group_id: &str,
            _revision: u64,
            _node_id: &str,
        ) -> Result<DaemonCatalog, DaemonError> {
            Ok(DaemonCatalog::default())
        }
    }

    struct DelayedCatalogClient {
        started: Mutex<Option<mpsc::Sender<()>>>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    struct DelayedSelectionClient {
        started: Mutex<Option<mpsc::Sender<()>>>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl DaemonClient for DelayedCatalogClient {
        fn status(&self) -> Result<DaemonStatus, DaemonError> {
            Ok(DaemonStatus::default())
        }

        fn import_subscription(&self, _url: &str) -> Result<DaemonStatus, DaemonError> {
            Ok(DaemonStatus::default())
        }

        fn connect(&self) -> Result<DaemonStatus, DaemonError> {
            Ok(DaemonStatus::default())
        }

        fn disconnect(&self) -> Result<DaemonStatus, DaemonError> {
            Ok(DaemonStatus::default())
        }

        fn events(
            &self,
            _after: u64,
            _known_epoch: Option<&str>,
        ) -> Result<DaemonEventBatch, DaemonError> {
            Ok(DaemonEventBatch::default())
        }

        fn catalog(&self) -> Result<DaemonCatalog, DaemonError> {
            if let Some(started) = self.started.lock().expect("catalog started lock").take() {
                started.send(()).expect("announce catalog start");
            }
            self.release
                .lock()
                .expect("catalog release lock")
                .recv()
                .expect("release delayed catalog");
            Ok(DaemonCatalog::default())
        }

        fn select_node(
            &self,
            _group_id: &str,
            _revision: u64,
            _node_id: &str,
        ) -> Result<DaemonCatalog, DaemonError> {
            Ok(DaemonCatalog::default())
        }
    }

    impl DaemonClient for DelayedSelectionClient {
        fn status(&self) -> Result<DaemonStatus, DaemonError> {
            Ok(DaemonStatus::default())
        }

        fn import_subscription(&self, _url: &str) -> Result<DaemonStatus, DaemonError> {
            Ok(DaemonStatus::default())
        }

        fn connect(&self) -> Result<DaemonStatus, DaemonError> {
            Ok(DaemonStatus::default())
        }

        fn disconnect(&self) -> Result<DaemonStatus, DaemonError> {
            Ok(DaemonStatus::default())
        }

        fn events(
            &self,
            _after: u64,
            _known_epoch: Option<&str>,
        ) -> Result<DaemonEventBatch, DaemonError> {
            Ok(DaemonEventBatch::default())
        }

        fn catalog(&self) -> Result<DaemonCatalog, DaemonError> {
            Ok(catalog_fixture())
        }

        fn select_node(
            &self,
            _group_id: &str,
            _revision: u64,
            _node_id: &str,
        ) -> Result<DaemonCatalog, DaemonError> {
            if let Some(started) = self.started.lock().expect("selection started lock").take() {
                started.send(()).expect("announce selection start");
            }
            self.release
                .lock()
                .expect("selection release lock")
                .recv()
                .expect("release delayed selection");
            let mut catalog = catalog_fixture();
            catalog.groups[1].nodes[0].selected = false;
            catalog.groups[1].nodes[1].selected = true;
            Ok(catalog)
        }
    }

    fn refresh(view_model: &mut DesktopViewModel) {
        let token = view_model.begin_refresh();
        let result = view_model.daemon_client().status();
        view_model.finish_refresh(token, result);
    }

    fn import_subscription(view_model: &mut DesktopViewModel, url: &str) {
        let token = view_model
            .active_mutation
            .filter(|active| active.kind == MutationKind::Import)
            .map(|active| active.token)
            .or_else(|| view_model.begin_import(url))
            .expect("start import mutation");
        let result = view_model.daemon_client().import_subscription(url);
        view_model.finish_import(token, result);
    }

    fn connect(view_model: &mut DesktopViewModel) {
        let token = view_model.next_mutation(MutationKind::Connect);
        let result = view_model.daemon_client().connect();
        view_model.finish_connect(token, result);
    }

    fn disconnect(view_model: &mut DesktopViewModel) {
        let token = view_model.next_mutation(MutationKind::Disconnect);
        let result = view_model.daemon_client().disconnect();
        view_model.finish_disconnect(token, result);
    }

    fn events(view_model: &mut DesktopViewModel, level: Option<&str>) -> Vec<PresentedEvent> {
        let filtered = view_model.set_event_filter(level);
        let Some(request) = view_model.begin_events_request() else {
            return filtered;
        };
        let result = request
            .client
            .events(request.after, request.known_epoch.as_deref());
        view_model
            .finish_events(request.token, result)
            .expect("current event request")
    }

    #[test]
    fn delayed_import_does_not_hold_the_model_lock_and_escape_stays_immediate() {
        let (client, started, release, _events_started, _events_release) =
            DelayedDaemonClient::harness();
        let model = Arc::new(Mutex::new(DesktopViewModel::new(client)));
        let (daemon, mutation) = {
            let mut model = model.lock().expect("view model lock");
            model.open_panel(Panel::Subscription);
            let mutation = model
                .begin_import("https://subscription.invalid/private")
                .expect("start import");
            (model.daemon_client(), mutation)
        };
        let worker_model = model.clone();
        let worker = thread::spawn(move || {
            let result = daemon.import_subscription("https://subscription.invalid/private");
            worker_model
                .lock()
                .expect("view model lock")
                .finish_import(mutation, result);
        });

        started.recv().expect("delayed import started");
        {
            let mut model = model
                .try_lock()
                .expect("model lock must stay available during import I/O");
            assert!(model.close_panel_on_escape());
        }
        release.send(()).expect("release import");
        worker.join().expect("join delayed import");
        assert_eq!(model.lock().expect("view model lock").panel, Panel::None);
    }

    #[test]
    fn delayed_events_do_not_block_panel_changes_or_filter_completion() {
        let (client, _import_started, _import_release, started, release) =
            DelayedDaemonClient::harness();
        let model = Arc::new(Mutex::new(DesktopViewModel::new(client)));
        let request = model
            .lock()
            .expect("view model lock")
            .begin_events_request()
            .expect("start events request");
        let worker_model = model.clone();
        let worker = thread::spawn(move || {
            let result = request
                .client
                .events(request.after, request.known_epoch.as_deref());
            worker_model
                .lock()
                .expect("view model lock")
                .finish_events(request.token, result)
        });

        started.recv().expect("delayed events started");
        {
            let mut model = model
                .try_lock()
                .expect("model lock must stay available during events I/O");
            model.open_panel(Panel::Routes);
            assert_eq!(model.panel, Panel::Routes);
        }
        release.send(()).expect("release events");
        assert!(
            worker
                .join()
                .expect("join delayed events")
                .expect("current event request")
                .is_empty()
        );
    }

    #[test]
    fn opening_a_panel_replaces_the_previous_panel_and_escape_closes_it() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));

        assert_eq!(view_model.panel, Panel::None);
        view_model.open_panel(Panel::Subscription);
        assert_eq!(view_model.panel, Panel::Subscription);
        view_model.open_panel(Panel::Routes);
        assert_eq!(view_model.panel, Panel::Routes);
        view_model.open_panel(Panel::Events);
        assert_eq!(view_model.panel, Panel::Events);

        assert!(view_model.close_panel_on_escape());
        assert_eq!(view_model.panel, Panel::None);
        assert!(!view_model.close_panel_on_escape());
    }

    #[test]
    fn a_pending_connection_action_suppresses_repeated_activation() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        refresh(&mut view_model);

        assert_eq!(
            view_model.begin_primary_action().map(|(action, _)| action),
            Some(PrimaryAction::Connect)
        );
        assert!(!view_model.presentation().primary_enabled);
        assert_eq!(view_model.begin_primary_action(), None);
    }

    #[test]
    fn stale_startup_refresh_cannot_overwrite_a_newer_import() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let refresh = view_model.begin_refresh();
        view_model.open_panel(Panel::Subscription);
        let import = view_model
            .begin_import("https://subscription.invalid/private")
            .expect("import supersedes startup refresh");

        view_model.finish_import(
            import,
            Ok(DaemonStatus {
                state: "ready".into(),
                profile: Some("Новый".into()),
                current_node: Some("Авто".into()),
                ..DaemonStatus::default()
            }),
        );
        view_model.finish_refresh(
            refresh,
            Ok(DaemonStatus {
                state: "empty".into(),
                ..DaemonStatus::default()
            }),
        );

        assert_eq!(
            view_model.state,
            UiState::Ready {
                profile: "Новый".into(),
                node: "Авто".into(),
            }
        );
    }

    #[test]
    fn import_and_connection_mutations_are_mutually_serialized() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        view_model.state = UiState::Ready {
            profile: "Работа".into(),
            node: "Авто".into(),
        };
        let import = view_model
            .begin_import("https://subscription.invalid/private")
            .expect("start import");
        assert!(!view_model.presentation().primary_enabled);
        assert_eq!(view_model.begin_primary_action(), None);
        view_model.finish_import(import, Ok(MockDaemonClient::ready().status));

        let (action, mutation) = view_model
            .begin_primary_action()
            .expect("start connection mutation");
        assert_eq!(action, PrimaryAction::Connect);
        assert_eq!(
            view_model.begin_import("https://subscription.invalid/second"),
            None
        );
        assert_eq!(
            view_model.import_error.as_deref(),
            Some("Дождитесь завершения операции подключения.")
        );
        view_model.finish_connect(mutation, Ok(MockDaemonClient::ready().status));
    }

    #[test]
    fn event_epoch_change_resets_cache_cursor_and_cache_is_bounded() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let first = view_model
            .begin_events_request()
            .expect("first event request");
        assert_eq!(first.after, 0);
        assert_eq!(first.known_epoch, None);
        let events = (1..=505)
            .map(|id| DaemonEvent {
                id,
                timestamp: id.to_string(),
                level: "info".into(),
                message: format!("event {id}"),
            })
            .collect();
        let first_page = view_model
            .finish_events(
                first.token,
                Ok(DaemonEventBatch {
                    epoch: "epoch-a".into(),
                    events,
                }),
            )
            .expect("current completion");
        assert_eq!(first_page.len(), 500);
        assert_eq!(first_page.first().map(|event| event.id), Some(6));

        let second = view_model
            .begin_events_request()
            .expect("second event request");
        assert_eq!(second.after, 505);
        assert_eq!(second.known_epoch.as_deref(), Some("epoch-a"));
        let second_page = view_model
            .finish_events(
                second.token,
                Ok(DaemonEventBatch {
                    epoch: "epoch-b".into(),
                    events: vec![DaemonEvent {
                        id: 1,
                        timestamp: "new".into(),
                        level: "warning".into(),
                        message: "new epoch".into(),
                    }],
                }),
            )
            .expect("epoch completion");
        assert_eq!(
            second_page.iter().map(|event| event.id).collect::<Vec<_>>(),
            [1]
        );
        let request = view_model
            .begin_events_request()
            .expect("request after epoch reset");
        assert_eq!(request.after, 1);
        assert_eq!(request.known_epoch.as_deref(), Some("epoch-b"));
    }

    #[test]
    fn event_requests_coalesce_and_completion_uses_the_current_local_filter() {
        let client = Arc::new(MockDaemonClient::ready());
        let mut view_model = DesktopViewModel::new(client.clone());
        let request = view_model.begin_events_request().expect("event request");
        assert!(view_model.begin_events_request().is_none());
        let local = view_model.set_event_filter(Some("error"));
        assert!(local.is_empty());
        assert!(client.calls.lock().expect("mock call lock").is_empty());

        let filtered = view_model
            .finish_events(
                request.token,
                Ok(DaemonEventBatch {
                    epoch: "epoch-a".into(),
                    events: vec![
                        DaemonEvent {
                            id: 1,
                            timestamp: "1".into(),
                            level: "info".into(),
                            message: "info".into(),
                        },
                        DaemonEvent {
                            id: 2,
                            timestamp: "2".into(),
                            level: "error".into(),
                            message: "error".into(),
                        },
                    ],
                }),
            )
            .expect("current completion");
        assert_eq!(
            filtered.iter().map(|event| event.id).collect::<Vec<_>>(),
            [2]
        );

        let newer = view_model.begin_events_request().expect("newer request");
        assert!(
            view_model
                .finish_events(
                    request.token,
                    Ok(DaemonEventBatch {
                        epoch: "epoch-a".into(),
                        events: Vec::new(),
                    }),
                )
                .is_none()
        );
        assert!(
            view_model
                .finish_events(
                    newer.token,
                    Ok(DaemonEventBatch {
                        epoch: "epoch-a".into(),
                        events: Vec::new(),
                    }),
                )
                .is_some()
        );
    }

    fn catalog_fixture() -> DaemonCatalog {
        DaemonCatalog {
            revision: 91,
            groups: vec![
                DaemonCatalogGroup {
                    id: "opaque:first".into(),
                    label: "Резерв https://private.invalid/path".into(),
                    selected: false,
                    nodes: vec![DaemonCatalogNode {
                        id: "opaque:node-a".into(),
                        label: "Север".into(),
                        selected: false,
                        delay_ms: None,
                    }],
                },
                DaemonCatalogGroup {
                    id: "opaque:selected".into(),
                    label: "Основной".into(),
                    selected: true,
                    nodes: vec![
                        DaemonCatalogNode {
                            id: "opaque:node-b".into(),
                            label: "Восток".into(),
                            selected: true,
                            delay_ms: Some(37),
                        },
                        DaemonCatalogNode {
                            id: "opaque:node-c".into(),
                            label: "Запад".into(),
                            selected: false,
                            delay_ms: Some(51),
                        },
                    ],
                },
            ],
        }
    }

    #[test]
    fn catalog_preserves_revision_and_opaque_ids_and_uses_selected_group() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let request = view_model.begin_catalog_request().expect("catalog request");
        assert!(view_model.begin_catalog_request().is_none());

        let catalog = view_model
            .finish_catalog(request.token, Ok(catalog_fixture()))
            .expect("current catalog completion");

        assert_eq!(catalog.revision, Some(91));
        assert_eq!(catalog.groups[0].id, "opaque:first");
        assert_eq!(catalog.groups[1].id, "opaque:selected");
        assert_eq!(
            catalog.selected_group_id.as_deref(),
            Some("opaque:selected")
        );
        assert_eq!(catalog.nodes[0].id, "opaque:node-b");
        assert_eq!(catalog.nodes[0].delay_ms, Some(37));
        assert!(!catalog.groups[0].label.contains("private.invalid"));
        assert!(catalog.error.is_none());
    }

    #[test]
    fn delayed_catalog_network_call_does_not_hold_the_model_lock() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let client = Arc::new(DelayedCatalogClient {
            started: Mutex::new(Some(started_tx)),
            release: Mutex::new(release_rx),
        });
        let model = Arc::new(Mutex::new(DesktopViewModel::new(client)));
        let request = model
            .lock()
            .expect("view model lock")
            .begin_catalog_request()
            .expect("catalog request");
        let worker_model = model.clone();
        let worker = thread::spawn(move || {
            let result = request.client.catalog();
            worker_model
                .lock()
                .expect("view model lock")
                .finish_catalog(request.token, result)
        });

        started_rx.recv().expect("catalog request started");
        {
            let mut model = model
                .try_lock()
                .expect("model lock must remain available during catalog I/O");
            model.open_panel(Panel::Subscription);
            assert_eq!(model.panel(), Panel::Subscription);
        }
        release_tx.send(()).expect("release catalog request");
        assert!(worker.join().expect("join catalog worker").is_some());
    }

    #[test]
    fn catalog_uses_first_group_when_none_selected_and_changes_group_locally_by_id() {
        let client = Arc::new(MockDaemonClient::ready());
        let mut view_model = DesktopViewModel::new(client.clone());
        let mut fixture = catalog_fixture();
        for group in &mut fixture.groups {
            group.selected = false;
        }
        let request = view_model.begin_catalog_request().expect("catalog request");
        view_model
            .finish_catalog(request.token, Ok(fixture))
            .expect("catalog completion");

        assert_eq!(
            view_model
                .catalog_presentation()
                .selected_group_id
                .as_deref(),
            Some("opaque:first")
        );
        assert!(view_model.select_catalog_group("opaque:selected"));
        let selected = view_model.catalog_presentation();
        assert_eq!(
            selected.selected_group_id.as_deref(),
            Some("opaque:selected")
        );
        assert_eq!(selected.nodes[0].id, "opaque:node-b");
        assert!(client.calls.lock().expect("mock calls").is_empty());
        assert!(!view_model.select_catalog_group("unknown"));
    }

    #[test]
    fn empty_and_failed_catalogs_have_safe_visible_states_and_stale_results_are_ignored() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let empty = view_model
            .begin_catalog_request()
            .expect("empty catalog request");
        let presentation = view_model
            .finish_catalog(
                empty.token,
                Ok(DaemonCatalog {
                    revision: 5,
                    groups: Vec::new(),
                }),
            )
            .expect("empty catalog completion");
        assert_eq!(presentation.revision, Some(5));
        assert!(presentation.groups.is_empty());
        assert!(presentation.nodes.is_empty());
        assert!(presentation.error.is_none());

        let failed = view_model
            .begin_catalog_request()
            .expect("failed catalog request");
        let failure = view_model
            .finish_catalog(
                failed.token,
                Err(DaemonError::new(
                    "unsafe https://private.invalid token=secret",
                )),
            )
            .expect("current failed completion");
        assert_eq!(
            failure.error.as_deref(),
            Some("Не удалось загрузить маршруты. Повторите попытку.")
        );
        assert!(
            !failure
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("private")
        );

        let newer = view_model
            .begin_catalog_request()
            .expect("new catalog request");
        assert!(
            view_model
                .finish_catalog(failed.token, Ok(catalog_fixture()))
                .is_none()
        );
        assert!(
            view_model
                .finish_catalog(newer.token, Ok(catalog_fixture()))
                .is_some()
        );
    }

    fn connected_view_model_with_catalog() -> DesktopViewModel {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let refresh = view_model.begin_refresh();
        view_model.finish_refresh(
            refresh,
            Ok(DaemonStatus {
                state: "connected".into(),
                profile: Some("Тестовый".into()),
                current_node: Some("Первый".into()),
                ..DaemonStatus::default()
            }),
        );
        let request = view_model.begin_catalog_request().expect("catalog request");
        view_model
            .finish_catalog(request.token, Ok(catalog_fixture()))
            .expect("catalog completion");
        view_model
    }

    #[test]
    fn selection_is_allowed_only_for_connected_fresh_catalog_and_suppresses_repeated_clicks() {
        let mut view_model = connected_view_model_with_catalog();
        assert!(view_model.catalog_presentation().selection_enabled);

        let request = view_model
            .begin_node_selection("opaque:node-c")
            .expect("valid selection");
        assert_eq!(request.revision, 91);
        assert!(!view_model.catalog_presentation().selection_enabled);
        assert!(view_model.begin_node_selection("opaque:node-c").is_none());
        assert!(
            view_model
                .catalog_presentation()
                .nodes
                .iter()
                .find(|node| node.id == "opaque:node-c")
                .expect("selected node")
                .selected
        );

        let mut disconnected = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let refresh = disconnected.begin_refresh();
        disconnected.finish_refresh(refresh, Ok(MockDaemonClient::ready().status));
        let catalog = disconnected
            .begin_catalog_request()
            .expect("catalog request while disconnected");
        disconnected.finish_catalog(catalog.token, Ok(catalog_fixture()));
        assert!(disconnected.begin_node_selection("opaque:node-c").is_none());
        let queued = disconnected.queued_node_selections();
        assert_eq!(queued.len(), 1);
        let queued = &queued[0];
        assert_eq!(queued.group_id, "opaque:selected");
        assert_eq!(queued.node_id, "opaque:node-c");
        assert_eq!(queued.revision, 91);
        assert!(disconnected.catalog_presentation().selection_queued);
    }

    #[test]
    fn ready_state_queues_one_selection_per_group_for_connect() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let refresh = view_model.begin_refresh();
        view_model.finish_refresh(refresh, Ok(MockDaemonClient::ready().status));
        let catalog = view_model
            .begin_catalog_request()
            .expect("catalog request while ready");
        view_model.finish_catalog(catalog.token, Ok(catalog_fixture()));

        assert!(view_model.begin_node_selection("opaque:node-c").is_none());
        assert!(view_model.select_catalog_group("opaque:first"));
        assert!(view_model.begin_node_selection("opaque:node-a").is_none());

        let queued = view_model.queued_node_selections();
        assert_eq!(queued.len(), 2);
        assert_eq!(queued[0].group_id, "opaque:selected");
        assert_eq!(queued[0].node_id, "opaque:node-c");
        assert_eq!(queued[1].group_id, "opaque:first");
        assert_eq!(queued[1].node_id, "opaque:node-a");
    }

    #[test]
    fn secondary_group_selection_never_overrides_primary_connection_summary() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let refresh = view_model.begin_refresh();
        view_model.finish_refresh(refresh, Ok(MockDaemonClient::ready().status));
        let catalog = view_model.begin_catalog_request().expect("catalog request");
        view_model.finish_catalog(catalog.token, Ok(catalog_fixture()));

        assert!(view_model.begin_node_selection("opaque:node-c").is_none());
        let primary_summary = view_model.presentation().node;

        assert!(view_model.select_catalog_group("opaque:first"));
        assert!(view_model.begin_node_selection("opaque:node-a").is_none());
        assert_eq!(
            view_model.presentation().node,
            primary_summary,
            "a secondary selector must not replace the primary server summary"
        );
    }

    #[test]
    fn incomplete_queued_selection_batch_is_preserved_for_connect_retry() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let refresh = view_model.begin_refresh();
        view_model.finish_refresh(refresh, Ok(MockDaemonClient::ready().status));
        let catalog = view_model.begin_catalog_request().expect("catalog request");
        view_model.finish_catalog(catalog.token, Ok(catalog_fixture()));
        assert!(view_model.begin_node_selection("opaque:node-c").is_none());
        let queued = view_model.queued_node_selections();

        assert!(!view_model.finish_queued_node_selections(&queued, Vec::new()));
        assert_eq!(view_model.queued_node_selections(), queued);
    }

    #[test]
    fn failed_connect_preserves_queued_selections_for_retry() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let refresh = view_model.begin_refresh();
        view_model.finish_refresh(refresh, Ok(MockDaemonClient::ready().status));
        let catalog = view_model.begin_catalog_request().expect("catalog request");
        view_model.finish_catalog(catalog.token, Ok(catalog_fixture()));
        assert!(view_model.begin_node_selection("opaque:node-c").is_none());
        let queued = view_model.queued_node_selections();
        let (action, mutation) = view_model.begin_primary_action().expect("connect mutation");
        assert_eq!(action, PrimaryAction::Connect);

        view_model.finish_connect(mutation, Err(DaemonError::new("connect failed")));

        assert_eq!(view_model.queued_node_selections(), queued);
        assert!(view_model.catalog_presentation().selection_queued);
    }

    #[test]
    fn group_navigation_remains_enabled_for_loaded_read_only_catalogs_but_locks_while_loading() {
        for daemon_state in ["ready", "disconnected"] {
            let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
            let refresh = view_model.begin_refresh();
            view_model.finish_refresh(
                refresh,
                Ok(DaemonStatus {
                    state: daemon_state.into(),
                    profile: Some("Тестовый".into()),
                    current_node: Some("Восток".into()),
                    ..DaemonStatus::default()
                }),
            );
            let catalog = view_model.begin_catalog_request().expect("catalog request");
            view_model
                .finish_catalog(catalog.token, Ok(catalog_fixture()))
                .expect("loaded catalog");

            let read_only = view_model.catalog_presentation();
            assert!(read_only.group_navigation_enabled, "state: {daemon_state}");
            assert!(read_only.selection_enabled, "state: {daemon_state}");
            assert!(view_model.select_catalog_group("opaque:first"));

            let loading = view_model
                .begin_catalog_request()
                .expect("refresh loaded catalog");
            assert!(!view_model.catalog_presentation().group_navigation_enabled);
            assert!(!view_model.select_catalog_group("opaque:selected"));
            view_model.finish_catalog(loading.token, Ok(catalog_fixture()));
        }
    }

    #[test]
    fn slint_group_chips_use_local_navigation_gate_not_node_selection_gate() {
        let source = include_str!("../ui/app.slint");

        assert!(source.contains(
            "private property <bool> catalog-group-navigation-enabled: !root.catalog-loading && root.catalog-groups.length > 0;"
        ));
        assert!(source.contains("enabled: root.catalog-group-navigation-enabled;"));
        assert!(!source.contains(
            "enabled: root.catalog-selection-enabled;\n                            checkable: true;"
        ));
    }

    #[test]
    fn catalog_display_keeps_reference_flag_and_symbol_ux() {
        assert_eq!(
            derive_catalog_display("🇩🇪 Germany [de]", CatalogDisplayKind::Group),
            CatalogDisplay {
                icon: "🇩🇪".into(),
                label: "Germany [de]".into(),
            }
        );
        assert_eq!(
            derive_catalog_display("🌍 Сервер", CatalogDisplayKind::Group),
            CatalogDisplay {
                icon: "🌍".into(),
                label: "Сервер".into(),
            }
        );
        assert_eq!(
            derive_catalog_display("🎮 Игры", CatalogDisplayKind::Group),
            CatalogDisplay {
                icon: "🎮".into(),
                label: "Игры".into(),
            }
        );
        assert_eq!(
            derive_catalog_display("Без VPN", CatalogDisplayKind::Node),
            CatalogDisplay {
                icon: "↗".into(),
                label: "Без VPN".into(),
            }
        );
        assert_eq!(
            derive_catalog_display("Minecraft (javaw.exe)", CatalogDisplayKind::Group),
            CatalogDisplay {
                icon: "🎮".into(),
                label: "Minecraft (javaw.exe)".into(),
            }
        );
    }

    #[test]
    #[ignore = "superseded by desktop_control_center_source_contract"]
    fn reference_driven_catalog_visual_contract() {
        let theme = include_str!("../ui/theme.slint");
        let components = include_str!("../ui/components.slint");
        let app = include_str!("../ui/app.slint");
        let build = include_str!("../build.rs");
        let main = include_str!("main.rs");

        for token in [
            "void: #08090c;",
            "canvas: #0a0a0d;",
            "surface: #111114;",
            "elevated: #1a1a1f;",
            "line: #242429;",
            "accent-blue: #0091ff;",
            "accent-green: #22c55e;",
            "radius-large: 14px;",
        ] {
            assert!(theme.contains(token), "missing reference token: {token}");
        }
        assert!(app.matches("icon: string,").count() >= 2);
        assert!(app.contains("icon: group.icon;"));
        assert!(app.contains("icon: node.icon;"));
        assert!(app.contains("preferred-height: 92px;"));
        assert!(!app.contains("preferred-height: 136px;"));
        assert!(app.contains("if !root.has-profile: PrimaryAction"));
        assert!(app.contains("connection-touch := TouchArea"));
        assert!(app.contains("accessible-label: root.primary-label;"));
        assert!(components.matches("in property <string> icon;").count() >= 2);
        assert!(components.contains("text: root.icon;"));
        assert!(theme.contains("emoji-font-family: \"Twemoji Mozilla\";"));
        assert!(
            components
                .matches("font-family: Theme.emoji-font-family;")
                .count()
                >= 2
        );
        assert!(components.contains("import \"../assets/fonts/Twemoji.Mozilla.ttf\";"));
        assert!(build.contains("EmbedResourcesKind::EmbedFiles"));
        let notices = include_str!("../../../packaging/windows-x64/THIRD_PARTY_NOTICES.md");
        assert!(notices.contains("Twemoji Mozilla font"));
        assert!(notices.contains("https://github.com/mozilla/twemoji-colr"));
        assert!(notices.contains("CC BY 4.0"));
        assert!(components.contains("text: root.selected ? \"✓\""));
        assert!(main.contains("derive_catalog_display"));
        assert!(main.contains("icon: display.icon.into()"));
        assert!(main.contains("label: display.label.into()"));

        for rejected in [
            "BrandMark",
            "text: \"Локально\";",
            "text: \"Подписка\";",
            "label: \"Заменить\";",
            "? \"✓\" : \"↗\"",
        ] {
            assert!(
                !app.contains(rejected),
                "rejected visual remains: {rejected}"
            );
        }
        assert!(app.contains("source: @image-url(\"../assets/power.svg\");"));
        assert!(app.contains("state-label: \"Изменить ссылку подписки\";"));
        assert!(components.contains("accessible-label: root.state-label;"));
        assert!(app.contains("utility-bar := Rectangle"));
        assert!(app.contains("text: \"Главная\";"));
        assert!(
            app.contains(
                "label: root.subscription-refresh-pending ? \"Обновляем\" : \"Обновить\";"
            )
        );
    }

    #[test]
    #[ignore = "superseded by desktop_control_center_source_contract"]
    fn quiet_signal_responsive_inline_home_contract() {
        let source = include_str!("../ui/app.slint");
        let build = include_str!("../build.rs");
        let main = include_str!("main.rs");

        for required in [
            "preferred-width: 480px;",
            "preferred-height: 720px;",
            "min-width: 420px;",
            "min-height: 640px;",
            "max-width: 620px;",
            "max-height: 920px;",
            "VerticalLayout",
            "HorizontalLayout",
            "padding: 20px;",
            "preferred-height: 44px;",
            "preferred-height: 92px;",
            "state-busy",
            "LineEdit",
            "multicore://install-sub?url=…",
            "QuietChip",
            "RouteNodeRow",
            "ScrollView",
            "DetailsSurface",
            "Диагностика",
            "root.open-panel(\"events\")",
            "root.refresh-subscription();",
            "subscription-refresh-enabled",
            "root.catalog-selection-queued ? \"После подключения\"",
            "event.text == Key.Escape",
            "diagnostics-action.focus();",
        ] {
            assert!(
                source.contains(required),
                "missing inline-home contract: {required}"
            );
        }

        assert_eq!(
            source.matches("PrimaryAction {").count(),
            1,
            "the main surface must expose exactly one dominant action"
        );
        assert!(source.contains("root.import-url(root.subscription-draft);"));
        assert!(source.contains("root.primary-action();"));
        assert!(source.contains("activated => { root.select-catalog-group(group.id); }"));
        assert!(source.contains("activated => { root.select-catalog-node(node.id); }"));

        for rejected in [
            "Button",
            "StatePill",
            "GlowCard",
            "NavigationRow",
            "OverlayPanel",
            "events-row",
            "active-panel == \"subscription\"",
            "active-panel == \"routes\"",
            "background: #06060acc",
        ] {
            assert!(
                !source.contains(rejected),
                "legacy or fixed-screen composition remains: {rejected}"
            );
        }
        assert!(
            !source
                .lines()
                .any(|line| line.trim_start().starts_with("y:")),
            "fixed screen-position y bindings remain"
        );

        for legacy in ["StatePill", "GlowCard", "NavigationRow", "OverlayPanel"] {
            assert!(
                !build.contains(legacy),
                "build validation still imports {legacy}"
            );
        }
        assert!(main.contains("ui.set_has_profile(presentation.has_profile);"));
        assert!(main.contains("ui.set_state_busy(presentation.is_busy);"));
        assert!(main.contains("if refresh_catalog && current.presentation.has_profile"));
        assert!(main.contains("load_catalog(weak, model);"));
    }

    #[test]
    fn desktop_control_center_source_contract() {
        let source = include_str!("../ui/app.slint");
        let components = include_str!("../ui/components.slint");
        let main = include_str!("main.rs");

        let small_action = source
            .split("component SmallAction")
            .nth(1)
            .and_then(|source| source.split("component HealthRow").next())
            .expect("SmallAction source");
        assert!(small_action.contains("horizontal-stretch: 0;"));
        assert!(small_action.contains("preferred-width: small-label.preferred-width + 28px;"));
        assert!(small_action.contains("height: 44px;"));
        assert!(small_action.contains("in property <bool> selected: false;"));
        assert!(small_action.contains("accessible-description:"));

        assert!(source.contains("inline-routes := Rectangle"));
        assert!(source.contains("connection-strip := Rectangle"));
        assert!(source.contains("height: 96px;"));
        assert!(source.contains("power-action := FocusScope"));
        assert!(source.contains("width: 72px;"));
        assert!(source.contains("height: 72px;"));
        assert!(source.contains("accessible-label: root.primary-label;"));
        assert!(source.contains("in property <image> service-logo;"));
        assert!(source.contains("in property <bool> has-service-logo: false;"));
        assert!(source.contains("if root.has-service-logo: Image"));
        assert!(source.contains("if !root.has-service-logo: Image"));
        assert!(source.contains("@image-url(\"../assets/power.svg\")"));
        assert!(source.contains("subscription-announcement-text"));
        assert!(source.contains("subscription-announcement-tone"));
        for semantic_icon in [
            "announcement-info.svg",
            "announcement-success.svg",
            "announcement-danger.svg",
        ] {
            assert!(source.contains(semantic_icon));
        }
        let announcement = source
            .split("if root.subscription-announcement-text != \"\": Rectangle")
            .nth(1)
            .and_then(|source| source.split("inline-routes := Rectangle").next())
            .expect("announcement strip source");
        assert_eq!(announcement.matches("accessible-role: none;").count(), 3);
        assert!(source.contains("in property <string> subscription-title:"));
        assert!(source.contains("text: root.subscription-title;"));
        assert!(!source.to_ascii_lowercase().contains("text: \"gate8\""));
        assert!(!source.to_ascii_lowercase().contains("text: \"work\""));
        assert!(main.contains("slint::Image::load_from_path(Path::new(path))"));
        assert!(main.contains("ui.set_has_service_logo(service_logo.is_some());"));
        assert!(main.contains("ui.set_service_logo(service_logo.unwrap_or_default());"));
        let snapshot = main
            .split("struct UiSnapshot")
            .nth(1)
            .and_then(|source| source.split("fn main()").next())
            .expect("UiSnapshot source");
        assert!(!snapshot.contains("slint::Image"));
        assert!(!main.contains("Image::load_from_path(Path::new(\"http"));
        assert!(!source.contains("if root.has-profile: PrimaryAction"));
        assert!(components.contains("border-radius: 14px;"));
        assert!(components.contains("border-radius: 6px;"));
        let quiet_chip = components
            .split("export component QuietChip")
            .nth(1)
            .and_then(|source| source.split("export component RouteNodeRow").next())
            .expect("QuietChip source");
        assert!(quiet_chip.contains("horizontal-stretch: 0;"));
        assert!(quiet_chip.contains("max-width: 164px;"));
        assert!(quiet_chip.contains("border-radius: 14px;"));
        assert!(source.contains("min-width: 104px;"));
        assert!(!source.contains("min-width: 132px;"));
        assert!(source.contains("padding: 20px;"));
        assert!(source.contains("for node in root.catalog-nodes: RouteNodeRow"));
        assert!(source.contains("height: 46px;"));
        for filter in ["Все", "Система", "Ошибки"] {
            assert!(source.contains(&format!("selected: root.event-filter == \"{filter}\";")));
        }
        assert!(!source.contains("connection-center := Rectangle"));
        assert!(!source.contains("height: 238px;"));

        for required in [
            "preferred-width: 840px;",
            "preferred-height: 720px;",
            "min-width: 700px;",
            "min-height: 620px;",
            "no-frame: true;",
            "title-bar := Rectangle",
            "callback window-drag();",
            "callback window-minimize();",
            "callback window-toggle-maximize();",
            "callback window-close();",
            "width: 44px;",
            "height: 44px;",
            "navigation-rail := Rectangle",
            "main-pane := Rectangle",
            "in-out property <string> local-page: \"home\";",
            "in property <image> icon-source;",
            "source: root.icon-source;",
            "@image-url(\"../assets/home.svg\")",
            "@image-url(\"../assets/status.svg\")",
            "@image-url(\"../assets/settings.svg\")",
            "label: \"Главная\";",
            "label: \"Состояние\";",
            "label: \"Настройки\";",
            "text: \"Маршруты\";",
            "root.latency-check-queued",
            "\"Проверим автоматически\"",
            "root.open-panel(\"events\")",
            "root.refresh-subscription();",
            "root.primary-action();",
            "root.select-catalog-group(group.id);",
            "root.select-catalog-node(node.id);",
            "if !root.has-profile: PrimaryAction",
            "label: \"Добавить URL\";",
            "state-label: \"Добавить подписку\";",
            "root.replace-editor-open = true;",
            "callback update-action();",
            "root.update-action();",
            "Отключитесь, чтобы установить обновление",
        ] {
            assert!(
                source.contains(required),
                "missing desktop control-center contract: {required}"
            );
        }

        for rejected in [
            "preferred-width: 480px;",
            "min-width: 420px;",
            "max-width: 620px;",
            "max-height: 920px;",
            "text: \"ГОТОВО\";",
            "traffic-chart",
            "fake-metric",
            "gradient",
            "glow",
            "if root.visible-page == \"routes\"",
            "label: \"Все маршруты\"",
        ] {
            assert!(
                !source
                    .to_ascii_lowercase()
                    .contains(&rejected.to_ascii_lowercase()),
                "mobile or decorative pattern remains: {rejected}"
            );
        }
    }

    #[test]
    #[ignore = "superseded by desktop_control_center_source_contract"]
    fn quiet_signal_review_security_focus_and_intrinsic_layout_contract() {
        let source = include_str!("../ui/app.slint");
        let components = include_str!("../ui/components.slint");
        let main = include_str!("main.rs");

        for required in [
            "title: \"MultiCore\";",
            "in-out property <string> subscription-draft;",
            "in-out property <bool> replace-editor-open: false;",
            "in-out property <int> import-success-serial: 0;",
            "changed import-success-serial",
            "root.subscription-draft = \"\";",
            "root.replace-editor-open = false;",
            "focus-epoch: root.import-success-serial;",
            "focus-on-init: true;",
            "accessible-label: root.has-profile ? \"Новый URL подписки\" : \"URL подписки\";",
            "accessible-live-region: polite;",
            "if !root.has-profile || root.replace-editor-open: Rectangle",
            "preferred-height:",
            "vertical-stretch: 1;",
        ] {
            assert!(
                source.contains(required),
                "missing review contract: {required}"
            );
        }

        for rejected in [
            "title: \"Quiet Signal\";",
            "text: \"Quiet Signal\";",
            "BrandMark",
            "text: \"MultiCore\";",
            "text: \"Локально\";",
            "text: \"Подписка\";",
            "label: \"Заменить\";",
            "init => { self.focus(); }",
            "label: root.replace-editor-open ? \"Скрыть\" : \"Заменить\";",
        ] {
            assert!(
                !source.contains(rejected),
                "rejected review pattern remains: {rejected}"
            );
        }

        assert!(components.contains("changed focus-epoch"));
        let route = components
            .split("export component RouteNodeRow")
            .nth(1)
            .and_then(|source| source.split("export component DetailsSurface").next())
            .expect("RouteNodeRow source");
        assert!(route.contains("font-size: Theme.type-metadata-size;"));
        assert!(route.contains("text: root.icon;"));

        let import_worker = main
            .split("fn run_import(")
            .nth(1)
            .and_then(|source| source.split("fn run_connect(").next())
            .expect("run_import source");
        assert!(import_worker.contains("let import_succeeded = result.is_ok();"));
        assert!(import_worker.contains("import_succeeded"));
        assert!(main.contains("ui.set_import_success_serial("));
        assert!(!main.contains("set_subscription_draft(url"));

        let disconnect_worker = main
            .split("fn run_disconnect(")
            .nth(1)
            .and_then(|source| source.split("fn run_disconnect_reconciliation(").next())
            .expect("run_disconnect source");
        assert!(disconnect_worker.contains("model.finish_disconnect(mutation, result)"));
        assert!(disconnect_worker.contains("model.begin_disconnect_reconciliation()"));
        assert!(disconnect_worker.contains("run_disconnect_reconciliation("));

        let reconciliation_worker = main
            .split("fn run_disconnect_reconciliation(")
            .nth(1)
            .and_then(|source| source.split("fn apply_snapshot_from_worker(").next())
            .expect("disconnect reconciliation source");
        assert!(reconciliation_worker.contains("let result = client.status();"));
        assert!(
            reconciliation_worker.contains("finish_disconnect_reconciliation(mutation, result)")
        );
    }

    #[test]
    fn disconnecting_is_busy_and_definitive_failure_rolls_back_without_reconciliation() {
        let mut view_model = connected_view_model_with_catalog();
        let previous = view_model.state.clone();

        let (action, mutation) = view_model.begin_primary_action().expect("start disconnect");
        assert_eq!(action, PrimaryAction::Disconnect);
        assert_eq!(
            view_model.state,
            UiState::Disconnecting {
                profile: "Тестовый".into(),
                node: "Первый".into(),
            }
        );

        let pending = view_model.presentation();
        assert_eq!(pending.primary_label, "Отключаем…");
        assert_eq!(pending.primary_action, PrimaryAction::None);
        assert!(!pending.primary_enabled);
        assert!(pending.has_profile);
        assert!(pending.is_connected);
        assert!(pending.is_busy);
        assert!(view_model.begin_primary_action().is_none());
        assert!(
            view_model
                .begin_import("https://subscription.invalid/private")
                .is_none()
        );
        assert!(view_model.begin_node_selection("opaque:node-c").is_none());

        assert!(!view_model.finish_disconnect(
            mutation,
            Err(DaemonError::new(
                "definitive rejection https://private.invalid token=secret"
            ))
        ));
        assert_eq!(view_model.state, previous);
        let rolled_back = view_model.presentation();
        assert_eq!(rolled_back.primary_action, PrimaryAction::Disconnect);
        assert_eq!(
            rolled_back.supporting,
            "Не удалось отключиться. Соединение остаётся активным."
        );
        assert!(!rolled_back.supporting.contains("private"));
        assert!(!rolled_back.supporting.contains("secret"));
    }

    #[test]
    fn ambiguous_disconnect_response_loss_is_degraded_until_authoritative_status() {
        for status in [
            DaemonStatus {
                state: "ready".into(),
                profile: Some("Тестовый".into()),
                current_node: Some("Авто".into()),
                ..DaemonStatus::default()
            },
            DaemonStatus {
                state: "connected".into(),
                profile: Some("Тестовый".into()),
                current_node: Some("Первый".into()),
                ..DaemonStatus::default()
            },
        ] {
            let mut view_model = connected_view_model_with_catalog();
            let (_, disconnect) = view_model.begin_primary_action().expect("start disconnect");
            assert!(view_model.finish_disconnect(
                disconnect,
                Err(DaemonError::ambiguous(
                    "response lost https://private.invalid token=secret"
                ))
            ));

            let uncertain = view_model.presentation();
            assert!(uncertain.is_degraded);
            assert!(!uncertain.is_connected);
            assert!(!uncertain.supporting.contains("private"));
            assert!(!uncertain.supporting.contains("secret"));

            let reconciliation = view_model.begin_disconnect_reconciliation();
            assert!(view_model.presentation().is_degraded);
            assert!(view_model.presentation().is_busy);
            assert!(
                view_model
                    .begin_import("https://subscription.invalid/private")
                    .is_none()
            );
            assert!(
                view_model.finish_disconnect_reconciliation(reconciliation, Ok(status.clone()))
            );
            assert_eq!(
                view_model.presentation().is_connected,
                status.state == "connected"
            );
            assert!(!view_model.presentation().is_degraded);
        }
    }

    #[test]
    fn successful_disconnect_status_is_authoritative_without_reconciliation() {
        let mut view_model = connected_view_model_with_catalog();
        let (_, disconnect) = view_model.begin_primary_action().expect("start disconnect");

        assert!(!view_model.finish_disconnect(
            disconnect,
            Ok(DaemonStatus {
                state: "ready".into(),
                profile: Some("Тестовый".into()),
                current_node: Some("Авто".into()),
                ..DaemonStatus::default()
            })
        ));

        assert_eq!(
            view_model.state,
            UiState::Ready {
                profile: "Тестовый".into(),
                node: "Авто".into(),
            }
        );
        let confirmed = view_model.presentation();
        assert!(!confirmed.is_connected);
        assert!(!confirmed.is_busy);
        assert_eq!(confirmed.primary_action, PrimaryAction::Connect);
    }

    #[test]
    fn failed_or_stale_disconnect_reconciliation_never_claims_connected() {
        let mut failed = connected_view_model_with_catalog();
        let (_, disconnect) = failed.begin_primary_action().expect("start disconnect");
        assert!(failed.finish_disconnect(disconnect, Err(DaemonError::ambiguous("response lost"))));
        let reconciliation = failed.begin_disconnect_reconciliation();
        assert!(failed.finish_disconnect_reconciliation(
            reconciliation,
            Err(DaemonError::ambiguous("private status decode failure"))
        ));
        let presentation = failed.presentation();
        assert!(presentation.is_degraded);
        assert!(!presentation.is_connected);
        assert!(!presentation.supporting.contains("private"));

        let mut stale = connected_view_model_with_catalog();
        let (_, disconnect) = stale.begin_primary_action().expect("start disconnect");
        assert!(stale.finish_disconnect(disconnect, Err(DaemonError::ambiguous("response lost"))));
        let older = stale.begin_disconnect_reconciliation();
        let newer = stale.begin_refresh();
        assert!(!stale.finish_disconnect_reconciliation(
            older,
            Ok(DaemonStatus {
                state: "connected".into(),
                profile: Some("Устаревший".into()),
                current_node: Some("Устаревший".into()),
                ..DaemonStatus::default()
            })
        ));
        assert!(stale.presentation().is_degraded);
        stale.finish_refresh(
            newer,
            Ok(DaemonStatus {
                state: "ready".into(),
                profile: Some("Актуальный".into()),
                current_node: Some("Авто".into()),
                ..DaemonStatus::default()
            }),
        );
        assert_eq!(stale.presentation().profile, "Актуальный");
        assert!(!stale.presentation().is_connected);
    }

    #[test]
    fn quiet_signal_primitives() {
        let theme = include_str!("../ui/theme.slint");
        let components = include_str!("../ui/components.slint");
        let build = include_str!("../build.rs");

        for token in [
            "void: #0d0f12;",
            "canvas: #0d0f12;",
            "surface: #15181d;",
            "elevated: #1c2026;",
            "line: #2a3038;",
            "accent-blue: #4c9dff;",
            "focus: #83bdff;",
            "type-metadata-size: 12px;",
            "type-label-size: 14px;",
            "type-value-size: 16px;",
            "type-title-size: 22px;",
            "radius-small: 8px;",
            "radius-medium: 8px;",
            "radius-large: 12px;",
            "radius-panel: 12px;",
            "interactive-min: 44px;",
            "focus-width: 2px;",
        ] {
            assert!(
                theme.contains(token),
                "missing exact Native Control Center token: {token}"
            );
        }
        assert!(!theme.contains("type-icon-size"));
        let visual_sources = format!("{theme}\n{components}");
        for rejected in ["999px", "gradient", "blur", "glow", "shadow"] {
            assert!(
                !visual_sources.to_ascii_lowercase().contains(rejected),
                "rejected visual effect or radius remains: {rejected}"
            );
        }
        for legacy in ["StatePill", "GlowCard", "NavigationRow", "OverlayPanel"] {
            assert!(
                !components.contains(legacy),
                "legacy component remains: {legacy}"
            );
            assert!(
                !build.contains(legacy),
                "legacy build import remains: {legacy}"
            );
        }
        assert!(!components.contains("std-widgets.slint"));
        assert!(!components.contains(" Button "));

        for component in [
            "IconAction",
            "PrimaryAction",
            "QuietChip",
            "RouteNodeRow",
            "DetailsSurface",
        ] {
            assert!(
                components.contains(&format!("export component {component}")),
                "missing Quiet Signal primitive: {component}"
            );
            assert!(
                build.contains(&format!("{component} {{")),
                "build-time component validation must instantiate {component}"
            );
        }

        let component_source = |name: &str, next: Option<&str>| {
            let source = components
                .split(&format!("export component {name}"))
                .nth(1)
                .unwrap_or_else(|| panic!("missing component source for {name}"));
            next.and_then(|next| source.split(&format!("export component {next}")).next())
                .unwrap_or(source)
        };
        let icon_action = component_source("IconAction", Some("PrimaryAction"));
        assert!(icon_action.contains("preferred-width: Theme.interactive-min;"));
        assert!(icon_action.contains("preferred-height: Theme.interactive-min;"));
        assert!(icon_action.contains("accessible-label: root.state-label;"));

        for (name, next) in [
            ("PrimaryAction", "QuietChip"),
            ("QuietChip", "RouteNodeRow"),
            ("RouteNodeRow", "DetailsSurface"),
        ] {
            let source = component_source(name, Some(next));
            assert!(
                source.contains("min-height: Theme.interactive-min;")
                    || (name == "RouteNodeRow" && source.contains("min-height: 52px;")),
                "{name}"
            );
            assert!(source.contains("Theme.focus-width"), "{name}");
            assert!(source.contains("event.text == \" \""), "{name}");
            assert!(source.contains("event.text == \"\\n\""), "{name}");
            assert!(source.contains("accessible-role: button;"), "{name}");
            assert!(source.contains("accessible-label:"), "{name}");
        }
        let primary = component_source("PrimaryAction", Some("QuietChip"));
        assert!(primary.contains("font-size: Theme.type-value-size;"));
        assert!(primary.contains("action-touch.pressed"));
        assert!(
            primary.contains("max-height: Theme.interactive-min;"),
            "primary action must not stretch into a giant color block"
        );
        assert!(!primary.contains("destructive"));

        let route = component_source("RouteNodeRow", Some("DetailsSurface"));
        assert!(route.contains("pending-state-label"));
        assert!(route.contains("root.pending ?"));
        assert!(route.contains("font-size: Theme.type-metadata-size;"));
        assert!(route.contains("min-height: Theme.interactive-min;"));
        assert!(route.contains("preferred-height: 46px;"));
        assert!(route.contains("max-height: 46px;"));
        assert!(
            !route.contains("opacity: root.enabled"),
            "read-only and selected route rows must remain fully legible"
        );
        assert!(route.contains("accessible-enabled: root.enabled;"));
        assert!(route.contains("TouchArea"));
        assert!(route.contains("enabled: root.enabled;"));
        assert!(route.contains("clicked => { root.activated(); }"));
        assert!(!route.contains("parent.width - 136px"));

        let details = component_source("DetailsSurface", None);
        assert!(details.contains("width: Theme.interactive-min;"));
        assert!(details.contains("height: Theme.interactive-min;"));
        assert!(details.contains("Theme.focus-width"));
        assert!(details.contains("event.text == \" \""));
        assert!(details.contains("event.text == \"\\n\""));
        assert!(details.contains("dismiss-label"));
        assert!(!details.contains("\"Close "));

        let capture_runner = include_str!("../../../scripts/run-preview-capture.ps1");
        assert!(capture_runner.contains("$captureFileNames = @("));
        assert!(capture_runner.contains("Remove-Item -LiteralPath $captureFile -Force"));
        assert!(!capture_runner.contains("Get-ChildItem -LiteralPath $outputPath -Filter"));
        assert!(capture_runner.contains("$allowedOutputRoot"));
        assert!(capture_runner.contains("StartsWith($allowedOutputPrefix"));

        let capture_helper = include_str!("../../../scripts/capture-preview.ps1");
        assert!(capture_helper.contains("function Test-PreviewFrame"));
        assert!(capture_helper.contains("$requiredStableFrames = 2"));
        assert!(capture_helper.contains("Preview frame validation failed"));
        assert!(!capture_helper.contains("Name = \"product header\""));
        assert!(!capture_helper.contains("Name = \"window title\""));
        assert!(!capture_helper.contains("Name = \"window controls\""));
        assert!(capture_helper.contains(") -ge 160); Name = \"control-center content\""));
        assert!(
            capture_helper.contains("ExpectedState -eq \"selection-pending\"")
                && capture_helper.contains("$Bitmap.Width - 180")
                && capture_helper.contains("$Bitmap.Width - 55")
                && capture_helper
                    .matches("Get-SemanticPixelCountInRegion")
                    .count()
                    >= 3
        );
        let click_helper = capture_helper
            .split("function Click-Preview")
            .nth(1)
            .and_then(|source| source.split("function Test-ColorNear").next())
            .expect("Click-Preview helper");
        assert!(capture_helper.contains("public static void ClickClient"));
        assert!(capture_helper.contains("PostMessage(window, 0x0201"));
        assert!(click_helper.contains("[PreviewWindow]::ClickClient($Handle, $ClientX, $ClientY)"));
        assert!(!capture_helper.contains("mouse_event"));
        assert!(
            capture_helper
                .contains("Click-Preview $handle ([Math]::Floor(($Width + 176) / 2)) 420")
        );
        assert!(
            capture_helper.contains("[System.Drawing.Imaging.PixelFormat]::Format24bppRgb"),
            "captured PNG must be fully opaque instead of inheriting sparse DWM alpha"
        );

        let preview_fixture = include_str!("../../../scripts/preview-fixture.ps1");
        assert!(preview_fixture.contains("delay_ms = 34"));
        assert!(preview_fixture.contains("events = @()"));
        for rejected_fixture_copy in ["Automatic", "Route A", "Local profile"] {
            assert!(
                !preview_fixture.contains(rejected_fixture_copy),
                "preview fixture must use neutral Russian copy without fake telemetry: {rejected_fixture_copy}"
            );
        }
    }

    #[test]
    fn delayed_selection_network_call_does_not_hold_the_model_lock() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let client = Arc::new(DelayedSelectionClient {
            started: Mutex::new(Some(started_tx)),
            release: Mutex::new(release_rx),
        });
        let mut view_model = DesktopViewModel::new(client);
        let refresh = view_model.begin_refresh();
        view_model.finish_refresh(
            refresh,
            Ok(DaemonStatus {
                state: "connected".into(),
                profile: Some("Тестовый".into()),
                current_node: Some("Восток".into()),
                ..DaemonStatus::default()
            }),
        );
        let catalog = view_model.begin_catalog_request().expect("catalog request");
        let result = catalog.client.catalog();
        view_model.finish_catalog(catalog.token, result);
        let model = Arc::new(Mutex::new(view_model));
        let request = model
            .lock()
            .expect("view model lock")
            .begin_node_selection("opaque:node-c")
            .expect("selection request");
        let worker_model = model.clone();
        let worker = thread::spawn(move || {
            let result =
                request
                    .client
                    .select_node(&request.group_id, request.revision, &request.node_id);
            worker_model
                .lock()
                .expect("view model lock")
                .finish_node_selection(request.token, result)
        });

        started_rx.recv().expect("selection request started");
        {
            let mut model = model
                .try_lock()
                .expect("model lock must remain available during selection I/O");
            model.open_panel(Panel::Events);
            assert_eq!(model.panel(), Panel::Events);
        }
        release_tx.send(()).expect("release selection request");
        assert!(worker.join().expect("join selection worker").is_some());
    }

    #[test]
    fn selection_success_applies_returned_catalog_and_current_route_label() {
        let mut view_model = connected_view_model_with_catalog();
        let request = view_model
            .begin_node_selection("opaque:node-c")
            .expect("valid selection");
        let mut updated = catalog_fixture();
        updated.groups[1].nodes[0].selected = false;
        updated.groups[1].nodes[1].selected = true;
        updated.groups[1].nodes[1].label = "Новый маршрут".into();

        let outcome = view_model
            .finish_node_selection(request.token, Ok(updated))
            .expect("current selection completion");

        assert!(outcome.refresh_catalog);
        assert_eq!(view_model.presentation().node, "Новый маршрут");
        assert!(!view_model.catalog_presentation().selection_enabled);
        assert!(view_model.catalog_presentation().error.is_none());
    }

    #[test]
    fn selection_response_keeps_the_group_the_user_is_editing() {
        let mut view_model = connected_view_model_with_catalog();
        let request = view_model
            .begin_node_selection("opaque:node-c")
            .expect("selection in the visible group");
        let mut updated = catalog_fixture();
        updated.groups[0].selected = true;
        updated.groups[1].selected = false;
        updated.groups[1].nodes[0].selected = false;
        updated.groups[1].nodes[1].selected = true;

        view_model
            .finish_node_selection(request.token, Ok(updated))
            .expect("current selection completion");

        let presentation = view_model.catalog_presentation();
        assert_eq!(
            presentation.selected_group_id.as_deref(),
            Some("opaque:selected")
        );
        assert!(presentation.nodes[1].selected);
    }

    #[test]
    fn selection_failure_rolls_back_and_stale_completion_is_ignored() {
        let mut view_model = connected_view_model_with_catalog();
        let request = view_model
            .begin_node_selection("opaque:node-c")
            .expect("valid selection");

        let outcome = view_model
            .finish_node_selection(
                request.token,
                Err(DaemonError::new(
                    "unsafe opaque:node-b https://private.invalid token=secret",
                )),
            )
            .expect("current failed completion");

        assert!(!outcome.refresh_catalog);
        let presentation = view_model.catalog_presentation();
        assert!(presentation.nodes[0].selected);
        assert!(!presentation.nodes[1].selected);
        assert_eq!(
            presentation.error.as_deref(),
            Some("Не удалось изменить маршрут. Повторите попытку.")
        );
        assert!(!presentation.error.unwrap().contains("private"));
        assert!(
            view_model
                .finish_node_selection(request.token, Ok(catalog_fixture()))
                .is_none()
        );
    }

    #[test]
    fn catalog_request_is_deferred_without_superseding_an_active_selection() {
        let mut view_model = connected_view_model_with_catalog();
        let selection = view_model
            .begin_node_selection("opaque:node-c")
            .expect("selection");

        assert!(view_model.begin_catalog_request().is_none());
        let pending = view_model.catalog_presentation();
        assert!(pending.selection_pending);
        assert!(pending.nodes[1].selected);

        let mut selected = catalog_fixture();
        selected.groups[1].nodes[0].selected = false;
        selected.groups[1].nodes[1].selected = true;
        let outcome = view_model
            .finish_node_selection(selection.token, Ok(selected))
            .expect("selection completion");
        assert!(outcome.refresh_catalog);
        assert!(!view_model.catalog_presentation().selection_enabled);
        assert!(view_model.begin_catalog_request().is_some());
    }

    #[test]
    fn post_selection_refresh_reconciles_a_delayed_older_catalog_authoritatively() {
        let mut view_model = connected_view_model_with_catalog();
        let delayed_old = view_model
            .begin_catalog_request()
            .expect("delayed pre-selection catalog");
        assert!(view_model.begin_node_selection("opaque:node-c").is_none());
        view_model
            .finish_catalog(delayed_old.token, Ok(catalog_fixture()))
            .expect("older catalog completion");

        let selection = view_model
            .begin_node_selection("opaque:node-c")
            .expect("selection after old GET");
        let mut put_catalog = catalog_fixture();
        put_catalog.groups[1].nodes[0].selected = false;
        put_catalog.groups[1].nodes[1].selected = true;
        put_catalog.groups[1].nodes[1].label = "Ответ выбора".into();
        let outcome = view_model
            .finish_node_selection(selection.token, Ok(put_catalog))
            .expect("PUT completion");
        assert!(outcome.refresh_catalog);

        let authoritative = view_model
            .begin_catalog_request()
            .expect("required authoritative GET");
        let mut final_catalog = catalog_fixture();
        final_catalog.groups[1].nodes[0].selected = false;
        final_catalog.groups[1].nodes[1].selected = true;
        final_catalog.groups[1].nodes[1].label = "Подтверждённый маршрут".into();
        view_model
            .finish_catalog(authoritative.token, Ok(final_catalog))
            .expect("authoritative catalog completion");

        assert!(view_model.catalog_presentation().selection_enabled);
        assert_eq!(view_model.presentation().node, "Подтверждённый маршрут");
    }

    #[test]
    fn stale_revision_rolls_back_invalidates_catalog_and_requests_safe_refresh() {
        let mut view_model = connected_view_model_with_catalog();
        let request = view_model
            .begin_node_selection("opaque:node-c")
            .expect("valid selection");

        let outcome = view_model
            .finish_node_selection(request.token, Err(DaemonError::stale_revision()))
            .expect("current stale completion");

        assert!(outcome.refresh_catalog);
        let stale = view_model.catalog_presentation();
        assert!(stale.nodes[0].selected);
        assert!(!stale.nodes[1].selected);
        assert!(!stale.selection_enabled);
        assert!(stale.revision.is_none());
        assert_eq!(
            stale.error.as_deref(),
            Some("Каталог маршрутов изменился. Обновляем его; повторите выбор.")
        );
    }

    #[test]
    fn ambiguous_selection_failure_rolls_back_then_reconciles_without_losing_safe_error() {
        let mut view_model = connected_view_model_with_catalog();
        let request = view_model
            .begin_node_selection("opaque:node-c")
            .expect("valid selection");

        let outcome = view_model
            .finish_node_selection(
                request.token,
                Err(DaemonError::ambiguous(
                    "unsafe transport detail https://private.invalid token=secret",
                )),
            )
            .expect("ambiguous completion");

        assert!(outcome.refresh_catalog);
        let rolled_back = view_model.catalog_presentation();
        assert!(rolled_back.nodes[0].selected);
        assert!(!rolled_back.nodes[1].selected);
        assert!(!rolled_back.selection_enabled);
        assert_eq!(
            rolled_back.error.as_deref(),
            Some(
                "Не удалось подтвердить изменение маршрута. Каталог обновляется; повторите попытку."
            )
        );

        let refresh = view_model
            .begin_catalog_request()
            .expect("reconciliation catalog request");
        assert_eq!(
            view_model.catalog_presentation().error.as_deref(),
            rolled_back.error.as_deref()
        );
        view_model
            .finish_catalog(refresh.token, Ok(catalog_fixture()))
            .expect("reconciliation completion");
        let reconciled = view_model.catalog_presentation();
        assert!(reconciled.selection_enabled);
        assert_eq!(reconciled.error, rolled_back.error);
    }

    #[test]
    fn selection_and_connection_or_import_mutations_are_gated_in_both_directions() {
        let mut selecting = connected_view_model_with_catalog();
        let selection = selecting
            .begin_node_selection("opaque:node-c")
            .expect("selection");
        assert!(selecting.begin_primary_action().is_none());
        assert!(
            selecting
                .begin_import("https://subscription.invalid/private")
                .is_none()
        );
        assert!(!selecting.catalog_presentation().selection_enabled);
        selecting
            .finish_node_selection(selection.token, Ok(catalog_fixture()))
            .expect("finish selection");

        let mut disconnecting = connected_view_model_with_catalog();
        assert!(disconnecting.begin_primary_action().is_some());
        assert!(
            disconnecting
                .begin_node_selection("opaque:node-c")
                .is_none()
        );
        assert!(!disconnecting.catalog_presentation().selection_enabled);

        let mut importing = connected_view_model_with_catalog();
        assert!(
            importing
                .begin_import("https://subscription.invalid/private")
                .is_some()
        );
        assert!(importing.begin_node_selection("opaque:node-c").is_none());
        assert!(!importing.catalog_presentation().selection_enabled);
    }

    #[test]
    fn subscription_panel_stays_open_and_repeated_import_is_suppressed() {
        let client = Arc::new(MockDaemonClient::ready());
        let mut view_model = DesktopViewModel::new(client.clone());
        refresh(&mut view_model);
        view_model.open_panel(Panel::Subscription);

        assert!(
            view_model
                .begin_import("https://subscription.invalid/private")
                .is_some()
        );
        assert!(view_model.import_pending);
        assert!(
            view_model
                .begin_import("https://subscription.invalid/second")
                .is_none()
        );
        import_subscription(&mut view_model, "https://subscription.invalid/private");

        assert_eq!(view_model.panel, Panel::Subscription);
        assert!(!view_model.import_pending);
        assert_eq!(view_model.import_error, None);
        assert_eq!(
            client.calls.lock().expect("mock call lock").as_slice(),
            [
                "GET /v1/status",
                "POST /v1/subscriptions/import https://subscription.invalid/private",
            ]
        );
    }

    #[test]
    fn import_validation_and_server_failures_are_inline_safe_and_preserve_state() {
        let mut client = MockDaemonClient::ready();
        client.fail_import = true;
        let client = Arc::new(client);
        let mut view_model = DesktopViewModel::new(client);
        refresh(&mut view_model);
        view_model.state = UiState::Connected {
            profile: "Рабочий профиль".into(),
            node: "nl-01".into(),
        };
        let previous_state = view_model.state.clone();
        view_model.open_panel(Panel::Subscription);

        assert!(view_model.begin_import("not a URL").is_none());
        assert_eq!(
            view_model.import_error.as_deref(),
            Some("Введите корректный HTTP(S) URL.")
        );
        assert!(view_model.begin_import("   ").is_none());
        assert_eq!(
            view_model.import_error.as_deref(),
            Some("Введите корректный HTTP(S) URL.")
        );
        assert!(
            view_model
                .begin_import("https://secret.example/path?token=private")
                .is_some()
        );
        import_subscription(&mut view_model, "https://secret.example/path?token=private");

        assert_eq!(view_model.state, previous_state);
        assert_eq!(view_model.panel, Panel::Subscription);
        assert!(!view_model.import_pending);
        assert_eq!(
            view_model.import_error.as_deref(),
            Some("Не удалось добавить подписку. Проверьте URL и повторите попытку.")
        );
        assert!(
            !view_model
                .import_error
                .as_deref()
                .unwrap()
                .contains("secret")
        );
        assert!(
            !view_model
                .import_error
                .as_deref()
                .unwrap()
                .contains("token")
        );
    }

    #[test]
    fn presentation_exposes_exact_status_tone_and_inline_profile_mode() {
        let cases = [
            (UiState::Empty, "ГОТОВО", SemanticTone::Neutral, false),
            (
                UiState::Importing,
                "ПОДКЛЮЧЕНИЕ",
                SemanticTone::Warning,
                false,
            ),
            (
                UiState::Ready {
                    profile: "Работа".into(),
                    node: "Авто".into(),
                },
                "ГОТОВО",
                SemanticTone::Accent,
                true,
            ),
            (
                UiState::Connecting {
                    profile: "Работа".into(),
                    node: "Авто".into(),
                    step: "Запуск".into(),
                },
                "ПОДКЛЮЧЕНИЕ",
                SemanticTone::Warning,
                true,
            ),
            (
                UiState::Connected {
                    profile: "Работа".into(),
                    node: "nl-01".into(),
                },
                "В СЕТИ",
                SemanticTone::Success,
                true,
            ),
            (
                UiState::Error {
                    message: "Ошибка".into(),
                },
                "ОШИБКА",
                SemanticTone::Danger,
                true,
            ),
            (
                UiState::DaemonDegraded {
                    message: "Сервис".into(),
                },
                "СЕРВИС",
                SemanticTone::Warning,
                true,
            ),
        ];

        for (state, label, tone, has_profile) in cases {
            let presentation = state.presentation();
            assert_eq!(presentation.pill_label, label);
            assert_eq!(presentation.semantic_tone, tone);
            assert_eq!(presentation.has_profile, has_profile);
        }
    }

    #[test]
    fn events_advance_the_monotonic_cursor_and_do_not_replay_cached_ids() {
        let mut client = MockDaemonClient::ready();
        client.events = vec![
            DaemonEvent {
                id: 7,
                timestamp: "1".into(),
                level: "info".into(),
                message: "Started".into(),
            },
            DaemonEvent {
                id: 9,
                timestamp: "2".into(),
                level: "warning".into(),
                message: "Updated".into(),
            },
        ];
        let client = Arc::new(client);
        let mut view_model = DesktopViewModel::new(client.clone());

        let first = events(&mut view_model, None);
        let second = events(&mut view_model, None);

        assert_eq!(
            first.iter().map(|event| event.id).collect::<Vec<_>>(),
            [7, 9]
        );
        assert_eq!(
            second.iter().map(|event| event.id).collect::<Vec<_>>(),
            [7, 9]
        );
        assert_eq!(
            client.calls.lock().expect("mock call lock").as_slice(),
            [
                "GET /v1/events after=0",
                "GET /v1/events after=9 epoch=test-epoch"
            ]
        );
    }

    #[test]
    fn maps_each_state_to_one_contextual_action_and_copy() {
        let cases = [
            (
                UiState::Empty,
                "Добавьте подписку",
                PrimaryAction::None,
                "Подключиться",
            ),
            (
                UiState::Importing,
                "Добавляем профиль…",
                PrimaryAction::None,
                "Добавляем…",
            ),
            (
                UiState::Ready {
                    profile: "Работа".into(),
                    node: "Авто".into(),
                },
                "Можно подключаться",
                PrimaryAction::Connect,
                "Подключиться",
            ),
            (
                UiState::Connecting {
                    profile: "Работа".into(),
                    node: "Авто".into(),
                    step: "Проверяем маршрут…".into(),
                },
                "Устанавливаем соединение…",
                PrimaryAction::None,
                "Подключаем…",
            ),
            (
                UiState::Connected {
                    profile: "Работа".into(),
                    node: "nl-01".into(),
                },
                "Соединение защищено",
                PrimaryAction::Disconnect,
                "Отключиться",
            ),
            (
                UiState::Error {
                    message: "Нет сети".into(),
                },
                "Что-то пошло не так",
                PrimaryAction::Retry,
                "Повторить",
            ),
            (
                UiState::DaemonDegraded {
                    message: "Сервис запускается".into(),
                },
                "Ограниченный режим",
                PrimaryAction::Cleanup,
                "Очистить",
            ),
        ];

        for (state, headline, action, label) in cases {
            let presentation = state.presentation();
            assert_eq!(presentation.headline, headline);
            assert_eq!(presentation.primary_action, action);
            assert_eq!(presentation.primary_label, label);
        }
    }

    #[test]
    fn connected_state_exposes_current_route_line() {
        let presentation = UiState::Connected {
            profile: "Основной".into(),
            node: "Stockholm 02".into(),
        }
        .presentation();

        assert_eq!(presentation.supporting, "Текущий маршрут");
        assert!(presentation.is_connected);
    }

    #[test]
    fn event_presentation_redacts_secrets_and_subscription_urls() {
        let event = DaemonEvent {
            id: 1,
            timestamp: "12:04:07".into(),
            level: "info".into(),
            message: "import url=https://user:pass@example.test/sub?token=abc token=xyz password: hunter2"
                .into(),
        };

        let presented = PresentedEvent::from(event);

        assert_eq!(
            presented.message,
            "import url=[REDACTED] token=[REDACTED] password:[REDACTED]"
        );
        assert!(!presented.message.contains("hunter2"));
        assert!(!presented.message.contains("example.test"));
    }

    #[test]
    fn event_presentation_redacts_bare_urls_authorization_and_uuid() {
        let redacted = redact_event_text(
            "Authorization: Bearer top-secret proxy https://alice:pass@edge.example/path?id=550e8400-e29b-41d4-a716-446655440000 uuid=550e8400-e29b-41d4-a716-446655440000",
        );

        assert_eq!(redacted, "Authorization: [REDACTED]");
        assert!(!redacted.contains("top-secret"));
        assert!(!redacted.contains("edge.example"));
        assert!(!redacted.contains("550e8400"));
    }

    #[test]
    fn event_presentation_redacts_entire_authorization_values_for_any_scheme() {
        let redacted = redact_event_text(
            "Authorization: Basic dXNlcjpzZWNyZXQ=\nProxy-Authorization: Negotiate private-ticket",
        );

        assert_eq!(
            redacted,
            "Authorization: [REDACTED]\nProxy-Authorization: [REDACTED]"
        );
        assert!(!redacted.contains("Basic"));
        assert!(!redacted.contains("Negotiate"));
        assert!(!redacted.contains("private-ticket"));
    }

    #[test]
    fn view_model_uses_daemon_contract_and_maps_its_dto() {
        let client = Arc::new(MockDaemonClient::ready());
        let mut view_model = DesktopViewModel::new(client.clone());

        refresh(&mut view_model);
        import_subscription(&mut view_model, "https://subscription.invalid/private");
        connect(&mut view_model);
        disconnect(&mut view_model);
        let _ = events(&mut view_model, Some("error"));

        assert_eq!(
            client.calls.lock().expect("mock call lock").as_slice(),
            [
                "GET /v1/status",
                "POST /v1/subscriptions/import https://subscription.invalid/private",
                "POST /v1/connect",
                "POST /v1/disconnect",
                "GET /v1/events after=0",
            ]
        );
        assert_eq!(
            view_model.state,
            UiState::Ready {
                profile: "Тестовый".into(),
                node: "Авто".into(),
            }
        );
    }

    #[test]
    fn events_failure_is_presented_safely_instead_of_as_an_empty_list() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let initial = view_model
            .begin_events_request()
            .expect("initial event request");
        view_model
            .finish_events(
                initial.token,
                Ok(DaemonEventBatch {
                    epoch: "stable-epoch".into(),
                    events: vec![DaemonEvent {
                        id: 7,
                        timestamp: "now".into(),
                        level: "info".into(),
                        message: "cached".into(),
                    }],
                }),
            )
            .expect("successful initial batch");
        let before = (
            view_model.event_epoch.clone(),
            view_model.event_cursor,
            view_model.event_cache.clone(),
        );

        let failed = view_model
            .begin_events_request()
            .expect("failing event request");
        let events = view_model
            .finish_events(
                failed.token,
                Err(DaemonError::new("unsafe upstream details")),
            )
            .expect("current failed request");

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].level, "error");
        assert_eq!(
            events[0].message,
            "Не удалось загрузить события. Повторите попытку."
        );
        assert!(!events[0].message.contains("upstream"));
        assert_eq!(events[1].message, "cached");
        assert_eq!(
            (
                view_model.event_epoch.clone(),
                view_model.event_cursor,
                view_model.event_cache.clone(),
            ),
            before,
            "a failed request must not mutate epoch, cursor, or cached events"
        );

        let after_filter_change = view_model.set_event_filter(Some("system"));
        assert_eq!(after_filter_change, vec![events[1].clone()]);

        let retry = view_model
            .begin_events_request()
            .expect("retry event request");
        let recovered = view_model
            .finish_events(
                retry.token,
                Ok(DaemonEventBatch {
                    epoch: "stable-epoch".into(),
                    events: Vec::new(),
                }),
            )
            .expect("successful retry");
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].message, "cached");
    }

    #[test]
    fn system_filter_uses_real_daemon_severity_levels() {
        let mut client = MockDaemonClient::ready();
        client.events = vec![
            DaemonEvent {
                id: 1,
                timestamp: "1".into(),
                level: "info".into(),
                message: "Started".into(),
            },
            DaemonEvent {
                id: 2,
                timestamp: "2".into(),
                level: "warning".into(),
                message: "Degraded".into(),
            },
            DaemonEvent {
                id: 3,
                timestamp: "3".into(),
                level: "error".into(),
                message: "Failed".into(),
            },
        ];
        let mut view_model = DesktopViewModel::new(Arc::new(client));

        let system = events(&mut view_model, Some("system"));
        let errors = events(&mut view_model, Some("error"));

        assert_eq!(
            system
                .iter()
                .map(|event| event.level.as_str())
                .collect::<Vec<_>>(),
            ["info", "warning"]
        );
        assert_eq!(
            errors
                .iter()
                .map(|event| event.level.as_str())
                .collect::<Vec<_>>(),
            ["error"]
        );
    }

    #[test]
    fn diagnostics_present_owned_health_and_keep_logs_when_refresh_fails() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let request = view_model
            .begin_diagnostics_request()
            .expect("start diagnostics request");
        assert!(view_model.diagnostics_presentation().loading);

        let feed = view_model
            .finish_diagnostics(
                request.token,
                Ok(DaemonDiagnostics {
                    xray: "ready".into(),
                    mihomo: "failed".into(),
                    tun: "starting".into(),
                    logs: vec![DaemonDiagnosticLog {
                        id: 7,
                        timestamp: "11".into(),
                        component: "mihomo".into(),
                        level: "error".into(),
                        message: "Authorization: Bearer private-secret".into(),
                    }],
                    mappings: vec![DaemonProxyMapping {
                        proxy_name: "Sweden [se]".into(),
                        address: "127.0.0.1:31006".into(),
                        xray_label: "Sweden [se]".into(),
                    }],
                }),
            )
            .expect("current diagnostics completion");
        let presentation = view_model.diagnostics_presentation();
        assert_eq!(presentation.xray.value, "Готов");
        assert_eq!(presentation.xray.tone, "success");
        assert_eq!(presentation.mihomo.value, "Сбой");
        assert_eq!(presentation.mihomo.tone, "danger");
        assert_eq!(presentation.tun.value, "Запуск");
        assert_eq!(presentation.mappings.len(), 1);
        assert_eq!(presentation.mappings[0].proxy_name, "Sweden [se]");
        assert_eq!(presentation.mappings[0].address, "127.0.0.1:31006");
        assert_eq!(presentation.mappings[0].xray_label, "Sweden [se]");
        assert!(feed[0].message.contains("[REDACTED]"));
        assert!(!feed[0].message.contains("private-secret"));

        let retry = view_model
            .begin_diagnostics_request()
            .expect("start diagnostics retry");
        let retained = view_model
            .finish_diagnostics(retry.token, Err(DaemonError::new("upstream secret")))
            .expect("current diagnostics failure");
        assert!(
            retained
                .iter()
                .any(|event| event.message.contains("Не удалось"))
        );
        assert!(retained.iter().any(|event| event.id == 7));
        assert!(!format!("{retained:?}").contains("upstream secret"));
    }

    #[test]
    #[ignore = "superseded by desktop_control_center_source_contract"]
    fn diagnostics_surface_has_compact_owned_health_dashboard_and_log_feed() {
        let source = include_str!("../ui/app.slint");
        for required in [
            "diagnostics-xray-value",
            "diagnostics-mihomo-value",
            "diagnostics-tun-value",
            "diagnostics-loading",
            "event.component",
            "Диагностика Xray",
            "Диагностика Mihomo",
            "Диагностика TUN MultiCore",
        ] {
            assert!(source.contains(required), "missing {required}");
        }
        assert!(source.contains("height: 76px"));
        assert!(!source.contains("Диагностика FlClashX"));
    }
}
