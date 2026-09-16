use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;
use serde_json::Value;

static HTTP_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)https?://[^\s<>"']+"#).expect("static HTTP URL regex"));
static AUTHORIZATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(authorization\s*:\s*)(?:bearer\s+)?[^\s,;}]+")
        .expect("static authorization regex")
});
static COOKIE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(cookie\s*:\s*)[^\s,;}]+").expect("static cookie regex"));
static UUID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
        .expect("static UUID regex")
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Debug,
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Component {
    Core,
    Subscription,
    Mihomo,
    Xray,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Event {
    timestamp_ms: u64,
    severity: Severity,
    component: Component,
    phase: String,
    profile_id: Option<String>,
    node_id: Option<String>,
    correlation_id: String,
    safe_message: String,
    structured_details: Value,
}

impl Event {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        timestamp_ms: u64,
        severity: Severity,
        component: Component,
        phase: impl Into<String>,
        correlation_id: impl Into<String>,
        message: &str,
        mut structured_details: Value,
    ) -> Self {
        redact_value(&mut structured_details, None);
        Self {
            timestamp_ms,
            severity,
            component,
            phase: redact_text(&phase.into()),
            profile_id: None,
            node_id: None,
            correlation_id: redact_text(&correlation_id.into()),
            safe_message: redact_text(message),
            structured_details,
        }
    }

    pub fn with_profile_id(mut self, profile_id: &str) -> Self {
        self.profile_id = Some(redact_text(profile_id));
        self
    }

    pub fn with_node_id(mut self, node_id: &str) -> Self {
        self.node_id = Some(redact_text(node_id));
        self
    }
}

fn redact_value(value: &mut Value, key: Option<&str>) {
    if key.is_some_and(sensitive_key) {
        *value = Value::String("[redacted]".into());
        return;
    }
    match value {
        Value::String(text) => *text = redact_text(text),
        Value::Array(values) => {
            for value in values {
                redact_value(value, key);
            }
        }
        Value::Object(fields) => {
            for (key, value) in fields {
                redact_value(value, Some(key));
            }
        }
        _ => {}
    }
}

fn sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    if key == "url" || key.ends_with("_url") {
        return true;
    }
    [
        "authorization",
        "cookie",
        "password",
        "passwd",
        "secret",
        "token",
    ]
    .iter()
    .any(|sensitive| key.contains(sensitive))
}

pub(crate) fn redact_text(text: &str) -> String {
    let text = HTTP_URL.replace_all(text, "[redacted]");
    let text = AUTHORIZATION.replace_all(&text, "$1[redacted]");
    let text = COOKIE.replace_all(&text, "$1[redacted]");
    UUID.replace_all(&text, "[redacted]").into_owned()
}
