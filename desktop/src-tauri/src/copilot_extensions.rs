//! Versioned SDK operations in a separately owned background workspace, plus native ACP elicitation.

/// Stable ACP v1 elicitation (schema 1.33.0). No URL is fetched or opened on receipt.
///
/// The runtime must supply freshly verified ownership to `begin` and `respond`, and call
/// the cancellation hooks before replacing a connection/session or closing a parent request.
/// Renderer input consists only of the opaque local ID and response, never a schema or URL.
pub mod elicitation {
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::sync::{mpsc, Arc, Mutex};

    use reqwest::Url;
    use serde::{Deserialize, Serialize};
    use serde_json::{json, Map, Value};
    use tauri::{AppHandle, Emitter, Runtime};
    use tauri_plugin_opener::OpenerExt;

    pub const REQUEST_EVENT: &str = "copilot:elicitation-request";
    pub const CLOSED_EVENT: &str = "copilot:elicitation-closed";
    pub const COMPLETE_EVENT: &str = "copilot:elicitation-complete";
    const LIMIT: usize = 32;
    const MAX_PAYLOAD: usize = 128 * 1024;
    const MAX_FIELDS: usize = 128;
    const MAX_TEXT: usize = 16 * 1024;

    pub type Result<T> = std::result::Result<T, Error>;
    pub type EventSink = Arc<dyn Fn(&str, Value) -> std::result::Result<(), String> + Send + Sync>;
    pub type BrowserOpener = Arc<dyn Fn(&str) -> std::result::Result<(), String> + Send + Sync>;

    /// Validate/compile on `None`; match on `Some(value)` using JSON Schema search semantics.
    /// Must return an error for unsupported syntax, never treat it as an unconstrained string.
    /// Supply a bounded native ECMAScript-compatible regex implementation to advertise form.
    /// No default regex engine is assumed: a transitive Cargo dependency is not a direct import.
    pub type PatternValidator =
        Arc<dyn Fn(&str, Option<&str>) -> std::result::Result<bool, String> + Send + Sync>;

    const PATTERN_WORKER: &str = "--darbot-elicitation-pattern";

    fn evaluate_pattern(pattern: &str, input: Option<&str>) -> std::result::Result<bool, String> {
        if pattern.len() > 4096 || input.is_some_and(|text| text.len() > MAX_PAYLOAD) {
            return Err("Pattern or value exceeds the native validation limit".into());
        }
        let expression = regress::Regex::new(pattern).map_err(|error| error.to_string())?;
        Ok(input.is_none_or(|value| expression.find(value).is_some()))
    }

    /// Keep a pathological agent-supplied expression out of the desktop process.
    pub fn native_patterns() -> PatternValidator {
        Arc::new(|pattern, input| {
            use std::io::{Read, Write};
            use std::process::Stdio;
            use std::time::{Duration, Instant};
            if pattern.len() > 4096 || input.is_some_and(|text| text.len() > MAX_PAYLOAD) {
                return Err("Pattern or value exceeds the native validation limit".into());
            }
            let executable = std::env::current_exe().map_err(|error| error.to_string())?;
            let mut child = crate::quiet::command(executable)
                .arg(PATTERN_WORKER)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|error| error.to_string())?;
            let result = (|| {
                let mut stdin = child.stdin.take().ok_or("Pattern validator has no input")?;
                let data = serde_json::to_vec(&json!({"pattern":pattern,"input":input}))
                    .map_err(|error| error.to_string())?;
                stdin.write_all(&data).map_err(|error| error.to_string())?;
                drop(stdin);
                let started = Instant::now();
                loop {
                    if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
                        if !status.success() {
                            return Err("Native pattern validation failed".into());
                        }
                        let output = child
                            .stdout
                            .take()
                            .ok_or("Pattern validator has no output")?;
                        let mut data = String::new();
                        output
                            .take(1025)
                            .read_to_string(&mut data)
                            .map_err(|error| error.to_string())?;
                        if data.len() > 1024 {
                            return Err("Invalid pattern-validator response".into());
                        }
                        return serde_json::from_str::<std::result::Result<bool, String>>(&data)
                            .map_err(|error| error.to_string())?;
                    }
                    if started.elapsed() > Duration::from_secs(2) {
                        return Err("Pattern validation exceeded its two-second budget".into());
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            })();
            if result.is_err() && child.try_wait().ok().flatten().is_none() {
                child.kill().map_err(|error| {
                    format!("Could not stop native pattern validation: {error}")
                })?;
                child.wait().map_err(|error| {
                    format!("Could not reap native pattern validation: {error}")
                })?;
            }
            result
        })
    }

    pub fn run_pattern_worker() -> Option<i32> {
        if std::env::args_os().nth(1).as_deref() != Some(std::ffi::OsStr::new(PATTERN_WORKER)) {
            return None;
        }
        use std::io::{Read, Write};
        let result = (|| -> std::result::Result<bool, String> {
            let mut input = Vec::new();
            std::io::stdin()
                .take((MAX_PAYLOAD * 2 + 1) as u64)
                .read_to_end(&mut input)
                .map_err(|error| error.to_string())?;
            if input.len() > MAX_PAYLOAD * 2 {
                return Err("Pattern request exceeds limit".into());
            }
            let value: Value = serde_json::from_slice(&input).map_err(|error| error.to_string())?;
            let pattern = value["pattern"].as_str().ok_or("Pattern must be text")?;
            evaluate_pattern(pattern, value["input"].as_str())
        })();
        let output = serde_json::to_vec(&result)
            .unwrap_or_else(|_| br#"{"Err":"Pattern result could not be encoded"}"#.to_vec());
        Some(if std::io::stdout().write_all(&output).is_ok() {
            0
        } else {
            1
        })
    }

    #[derive(Debug, Clone, Serialize, PartialEq, Eq)]
    pub struct Error {
        pub code: i64,
        pub message: String,
    }

    impl std::fmt::Display for Error {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.message)
        }
    }

    impl std::error::Error for Error {}

    fn invalid(message: impl Into<String>) -> Error {
        Error {
            code: -32602,
            message: message.into(),
        }
    }

    fn interrupted() -> Error {
        Error {
            code: -32800,
            message: "Elicitation interrupted".into(),
        }
    }

    fn internal(message: &str) -> Error {
        Error {
            code: -32603,
            message: message.into(),
        }
    }

    /// Constructed only by the native runtime from its live connection and pending RPC map.
    /// Request IDs retain their JSON type: `12` is different from `"12"`.
    #[derive(Debug, Clone, Default)]
    pub struct ScopeContext {
        pub connection_id: String,
        pub current_session_id: Option<String>,
        pub session_generation: Option<u128>,
        pub in_flight_request_ids: Vec<Value>,
        pub tool_call_ids: Vec<String>,
    }

    #[derive(Debug, Clone, PartialEq)]
    enum Scope {
        Session {
            session_id: String,
            tool_call_id: Option<String>,
        },
        Request(Value),
    }

    impl Scope {
        fn parse(params: &Map<String, Value>) -> Result<Self> {
            match (non_null(params, "sessionId"), params.get("requestId")) {
                (Some(Value::String(session)), None) if !session.is_empty() => {
                    let tool = match non_null(params, "toolCallId") {
                        None => None,
                        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
                        _ => return Err(invalid("Invalid toolCallId")),
                    };
                    Ok(Self::Session {
                        session_id: session.clone(),
                        tool_call_id: tool,
                    })
                }
                (None, Some(id))
                    if !params.contains_key("sessionId")
                        && non_null(params, "toolCallId").is_none()
                        && (id.is_string() || id.as_i64().is_some()) =>
                {
                    Ok(Self::Request(id.clone()))
                }
                _ => Err(invalid(
                    "Use sessionId with optional toolCallId OR an in-flight requestId",
                )),
            }
        }

        fn validate(&self, context: &ScopeContext) -> Result<()> {
            if context.connection_id.is_empty() {
                return Err(invalid("Missing connection ownership"));
            }
            let valid = match self {
                Self::Session {
                    session_id,
                    tool_call_id,
                } => {
                    context.current_session_id.as_ref() == Some(session_id)
                        && tool_call_id
                            .as_ref()
                            .is_none_or(|id| context.tool_call_ids.contains(id))
                }
                Self::Request(id) => context.in_flight_request_ids.contains(id),
            };
            if valid {
                Ok(())
            } else {
                Err(invalid("Elicitation scope is not live or owned"))
            }
        }
    }

    #[derive(Debug, Clone, Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct PendingRequest {
        /// Local opaque handle, not the agent's parent JSON-RPC requestId.
        pub request_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub session_id: Option<String>,
        pub mode: String,
        pub message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub requested_schema: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub defaults: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub url: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub domain: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub elicitation_id: Option<String>,
        pub suspicious_domain: bool,
    }

    #[derive(Debug, Copy, Clone, Deserialize, Serialize, PartialEq, Eq)]
    #[serde(rename_all = "lowercase")]
    pub enum Action {
        Accept,
        Decline,
        Cancel,
    }

    #[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    pub struct Response {
        pub action: Action,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub content: Option<Value>,
    }

    /// The ACP dispatcher waits on `response` away from its reader loop. An interrupted
    /// request resolves to Err(-32800), never to a fabricated successful form response.
    pub struct Ticket {
        pub request: PendingRequest,
        pub response: mpsc::Receiver<Result<Response>>,
    }

    struct Entry {
        connection: String,
        session_at_creation: Option<String>,
        session_generation: Option<u128>,
        scope: Scope,
        request: PendingRequest,
        form: Option<Form>,
        sender: mpsc::Sender<Result<Response>>,
    }

    struct OutstandingUrl {
        connection: String,
        session_at_creation: Option<String>,
        scope: Scope,
        local_id: String,
        accepted: bool,
        completed: bool,
    }

    #[derive(Default)]
    struct State {
        pending: HashMap<String, Entry>,
        urls: HashMap<(String, String), OutstandingUrl>,
    }

    pub struct Controller {
        state: Mutex<State>,
        events: EventSink,
        opener: BrowserOpener,
        patterns: Option<PatternValidator>,
    }

    impl Controller {
        pub fn new<R: Runtime>(app: AppHandle<R>, patterns: Option<PatternValidator>) -> Arc<Self> {
            let event_app = app.clone();
            Self::with_hooks(
                Arc::new(move |name, payload| {
                    event_app.emit(name, payload).map_err(|e| e.to_string())
                }),
                Arc::new(move |url| {
                    app.opener()
                        .open_url(url, None::<&str>)
                        .map_err(|e| e.to_string())
                }),
                patterns,
            )
        }

        /// Hooks must be synchronous and must not re-enter this controller.
        /// Opener is invoked while locked so stop/session-close cannot race consent.
        pub fn with_hooks(
            events: EventSink,
            opener: BrowserOpener,
            patterns: Option<PatternValidator>,
        ) -> Arc<Self> {
            Arc::new(Self {
                state: Mutex::new(State::default()),
                events,
                opener,
                patterns,
            })
        }

        /// URL works without a regex dependency. Do not claim form support without validation.
        pub fn capabilities(&self) -> Value {
            if self.patterns.is_some() {
                json!({"form": {}, "url": {}})
            } else {
                json!({"url": {}})
            }
        }

        pub fn begin(&self, params: Value, context: &ScopeContext) -> Result<Ticket> {
            if params.to_string().len() > MAX_PAYLOAD {
                return Err(invalid("Elicitation payload exceeds host limit"));
            }
            let object = params
                .as_object()
                .ok_or_else(|| invalid("Expected request object"))?;
            let scope = Scope::parse(object)?;
            scope.validate(context)?;
            let message = text(object.get("message"), "message")?.to_owned();
            let mode = text(object.get("mode"), "mode")?;
            let local_id = format!("el-{:032x}", rand::random::<u128>());
            let mut request = PendingRequest {
                request_id: local_id.clone(),
                session_id: match &scope {
                    Scope::Session { session_id, .. } => Some(session_id.clone()),
                    Scope::Request(_) => None,
                },
                mode: mode.into(),
                message,
                requested_schema: None,
                defaults: None,
                url: None,
                domain: None,
                elicitation_id: None,
                suspicious_domain: false,
            };
            let form = match mode {
                "form" => {
                    if secret(&request.message, &Map::new()) {
                        return Err(invalid(
                            "Host policy prohibits credential requests in forms; use URL mode",
                        ));
                    }
                    let patterns = self.patterns.as_ref().ok_or_else(|| {
                        invalid(
                            "Form mode is unavailable: native pattern validation is not configured",
                        )
                    })?;
                    let schema = object
                        .get("requestedSchema")
                        .ok_or_else(|| invalid("Missing requestedSchema"))?;
                    let form = Form::parse(schema, patterns)?;
                    request.requested_schema = Some(schema.clone());
                    request.defaults = Some(Value::Object(form.defaults()));
                    Some(form)
                }
                "url" => {
                    let url = text(object.get("url"), "url")?;
                    let domain = validate_url(url)?;
                    request.suspicious_domain = domain.split('.').any(|p| p.starts_with("xn--"))
                        || domain.parse::<std::net::IpAddr>().is_ok();
                    request.url = Some(url.into());
                    request.domain = Some(domain);
                    request.elicitation_id =
                        Some(text(object.get("elicitationId"), "elicitationId")?.into());
                    None
                }
                _ => return Err(invalid("Unsupported elicitation mode")),
            };
            let mut state = self
                .state
                .lock()
                .map_err(|_| internal("Elicitation state unavailable"))?;
            if state.pending.len() + state.urls.values().filter(|u| u.accepted).count() >= LIMIT {
                return Err(invalid("Too many outstanding elicitations"));
            }
            if state.pending.contains_key(&local_id) {
                return Err(internal("Elicitation handle collision"));
            }
            if let Some(id) = &request.elicitation_id {
                let key = (context.connection_id.clone(), id.clone());
                if state.urls.contains_key(&key) {
                    return Err(invalid("Duplicate outstanding elicitationId"));
                }
                state.urls.insert(
                    key,
                    OutstandingUrl {
                        connection: context.connection_id.clone(),
                        session_at_creation: context.current_session_id.clone(),
                        scope: scope.clone(),
                        local_id: local_id.clone(),
                        accepted: false,
                        completed: false,
                    },
                );
            }
            let (sender, response) = mpsc::channel();
            state.pending.insert(
                local_id.clone(),
                Entry {
                    connection: context.connection_id.clone(),
                    session_at_creation: context.current_session_id.clone(),
                    session_generation: context.session_generation,
                    scope,
                    request: request.clone(),
                    form,
                    sender,
                },
            );
            if (self.events)(REQUEST_EVENT, serde_json::to_value(&request).unwrap()).is_err() {
                remove_entry(&mut state, &local_id);
                return Err(internal("Unable to display elicitation"));
            }
            Ok(Ticket { request, response })
        }

        pub fn cancel_local(&self, request_id: &str) {
            if let Ok(mut state) = self.state.lock() {
                if let Some(entry) = remove_entry(&mut state, request_id) {
                    let _ = entry.sender.send(Err(interrupted()));
                    let _ = (self.events)(CLOSED_EVENT, json!({"requestId":request_id}));
                }
            }
        }

        pub fn scope_for_response(&self, request_id: &str, connection: &str) -> Result<Value> {
            let state = self
                .state
                .lock()
                .map_err(|_| internal("Elicitation state unavailable"))?;
            let entry = state
                .pending
                .get(request_id)
                .filter(|entry| entry.connection == connection)
                .ok_or_else(|| invalid("Unknown elicitation request"))?;
            Ok(match &entry.scope {
                Scope::Session {
                    session_id,
                    tool_call_id,
                } => json!({
                    "sessionId":session_id, "toolCallId":tool_call_id
                }),
                Scope::Request(id) => json!({"requestId":id}),
            })
        }

        pub fn respond(
            &self,
            request_id: &str,
            response: Response,
            context: &ScopeContext,
        ) -> Result<()> {
            let mut state = self
                .state
                .lock()
                .map_err(|_| internal("Elicitation state unavailable"))?;
            let entry = state
                .pending
                .get(request_id)
                .ok_or_else(|| invalid("Unknown elicitation request"))?;
            if entry.connection != context.connection_id
                || entry.session_at_creation != context.current_session_id
                || entry.session_generation != context.session_generation
            {
                return Err(invalid("Elicitation connection or current session changed"));
            }

            entry.scope.validate(context)?;
            let content = if response.action == Action::Accept {
                if let Some(form) = &entry.form {
                    form.validate_content(
                        response.content.as_ref(),
                        self.patterns.as_ref().unwrap(),
                    )?
                } else {
                    // The URL comes exclusively from native stored state, never from renderer content.
                    (self.opener)(entry.request.url.as_deref().unwrap()).map_err(|_| {
                        internal("Unable to open the system browser; elicitation remains pending")
                    })?;
                    None
                }
            } else {
                None
            };
            let entry = state.pending.remove(request_id).unwrap();
            if let Some(id) = &entry.request.elicitation_id {
                let key = (entry.connection.clone(), id.clone());
                if response.action == Action::Accept {
                    if let Some(url) = state.urls.get_mut(&key) {
                        url.accepted = true;
                        if url.completed {
                            state.urls.remove(&key);
                        }
                    }
                } else {
                    state.urls.remove(&key);
                }
            }
            // A dropped waiter means the owning operation has already ended.
            if entry
                .sender
                .send(Ok(Response {
                    action: response.action,
                    content,
                }))
                .is_err()
            {
                if let Some(id) = &entry.request.elicitation_id {
                    state.urls.remove(&(entry.connection, id.clone()));
                }
                let _ = (self.events)(CLOSED_EVENT, json!({"requestId": request_id}));
                return Err(interrupted());
            }
            let _ = (self.events)(CLOSED_EVENT, json!({"requestId": request_id}));
            Ok(())
        }

        /// Unknown or repeated IDs, including IDs on another connection, are ignored.
        /// Completion is informational; it never converts an unapproved dialog into accept.
        pub fn complete(&self, connection_id: &str, elicitation_id: &str) {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            let key = (connection_id.to_owned(), elicitation_id.to_owned());
            let Some(url) = state.urls.get_mut(&key) else {
                return;
            };
            if url.completed {
                return;
            }
            url.completed = true;
            let payload = json!({"requestId": url.local_id, "elicitationId": elicitation_id});
            if url.accepted {
                state.urls.remove(&key);
            }
            let _ = (self.events)(COMPLETE_EVENT, payload);
        }

        pub fn cancel_all(&self) {
            self.cancel_where(|_, _, _| true);
        }

        pub fn cancel_connection(&self, connection_id: &str) {
            self.cancel_where(|connection, _, _| connection == connection_id);
        }

        pub fn cancel_session(&self, connection_id: &str, session_id: &str) {
            self.cancel_where(|connection, current, scope| connection == connection_id
                && (current.as_deref() == Some(session_id)
                    || matches!(scope, Scope::Session { session_id: id, .. } if id == session_id)));
        }

        pub fn cancel_request(&self, connection_id: &str, request_id: &Value) {
            self.cancel_where(|connection, _, scope| {
                connection == connection_id
                    && matches!(scope, Scope::Request(id) if id == request_id)
            });
        }

        fn cancel_where(&self, matches: impl Fn(&str, &Option<String>, &Scope) -> bool) {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            let ids: Vec<_> = state
                .pending
                .iter()
                .filter(|(_, entry)| {
                    matches(&entry.connection, &entry.session_at_creation, &entry.scope)
                })
                .map(|(id, _)| id.clone())
                .collect();
            for id in ids {
                if let Some(entry) = remove_entry(&mut state, &id) {
                    let _ = entry.sender.send(Err(interrupted()));
                    let _ = (self.events)(CLOSED_EVENT, json!({"requestId": id}));
                }
            }
            state
                .urls
                .retain(|_, url| !matches(&url.connection, &url.session_at_creation, &url.scope));
        }
    }

    impl Drop for Controller {
        fn drop(&mut self) {
            self.cancel_all();
        }
    }

    fn remove_entry(state: &mut State, id: &str) -> Option<Entry> {
        let entry = state.pending.remove(id)?;
        if let Some(url_id) = &entry.request.elicitation_id {
            state
                .urls
                .remove(&(entry.connection.clone(), url_id.clone()));
        }
        Some(entry)
    }

    fn validate_url(raw: &str) -> Result<String> {
        if raw.chars().any(|c| c.is_control() || c.is_whitespace())
            || raw.contains('\\')
            || !raw.contains("://")
        {
            return Err(invalid("Unsafe URL characters"));
        }
        let url = Url::parse(raw).map_err(|_| invalid("Invalid elicitation URL"))?;
        // Even empty userinfo is misleading; Url::username alone does not detect https://@host.
        let authority = raw
            .split_once("://")
            .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(""))
            .unwrap_or("");
        if !url.username().is_empty() || url.password().is_some() || authority.contains('@') {
            return Err(invalid("URL userinfo is prohibited"));
        }
        let host = url
            .host_str()
            .ok_or_else(|| invalid("URL must have a host"))?;
        let loopback = host.eq_ignore_ascii_case("localhost")
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
            return Err(invalid(
                "URL requires HTTPS (HTTP is allowed only on loopback)",
            ));
        }
        Ok(host.to_owned())
    }

    fn non_null<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
        object.get(key).filter(|v| !v.is_null())
    }

    fn text<'a>(value: Option<&'a Value>, name: &str) -> Result<&'a str> {
        value
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && s.len() <= MAX_TEXT)
            .ok_or_else(|| invalid(format!("Invalid or missing {name}")))
    }

    fn allowed(object: &Map<String, Value>, keys: &[&str]) -> Result<()> {
        if let Some(key) = object.keys().find(|k| !keys.contains(&k.as_str())) {
            return Err(invalid(format!("Unsupported schema constraint: {key}")));
        }
        for key in ["title", "description"] {
            if non_null(object, key).is_some_and(|v| !v.is_string()) {
                return Err(invalid(format!("Invalid schema {key}")));
            }
        }
        Ok(())
    }

    fn secret(name: &str, object: &Map<String, Value>) -> bool {
        [
            Some(name),
            object.get("title").and_then(Value::as_str),
            object.get("description").and_then(Value::as_str),
        ]
        .into_iter()
        .flatten()
        .any(|s| {
            let compact: String = s
                .chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect();
            [
                "password",
                "passwd",
                "passphrase",
                "apikey",
                "apitoken",
                "accesstoken",
                "refreshtoken",
                "bearertoken",
                "privatekey",
                "secretkey",
                "clientsecret",
                "recoverycode",
                "backupcode",
                "securitycode",
                "cardnumber",
                "creditcard",
                "debitcard",
                "paymentcredential",
                "paymenttoken",
                "cardverification",
                "seedphrase",
                "mnemonicphrase",
                "bankpassword",
                "paymentpin",
                "authtoken",
                "authenticationtoken",
            ]
            .iter()
            .any(|term| compact.contains(term))
                || matches!(
                    compact.as_str(),
                    "secret" | "token" | "cvv" | "cvc" | "pin" | "otp"
                )
                || s.split(|c: char| !c.is_ascii_alphanumeric()).any(|word| {
                    matches!(
                        word.to_ascii_lowercase().as_str(),
                        "cvv" | "cvc" | "password" | "secret" | "otp"
                    )
                })
        })
    }

    struct Form {
        fields: BTreeMap<String, Field>,
        required: HashSet<String>,
    }

    struct Field {
        schema: Map<String, Value>,
        kind: String,
        choices: Option<Vec<String>>,
    }

    impl Form {
        fn parse(schema: &Value, patterns: &PatternValidator) -> Result<Self> {
            let object = schema
                .as_object()
                .ok_or_else(|| invalid("Schema must be an object"))?;
            if secret("", object) {
                return Err(invalid(
                    "Host policy prohibits credential requests in forms; use URL mode",
                ));
            }
            allowed(
                object,
                &[
                    "type",
                    "title",
                    "description",
                    "properties",
                    "required",
                    "_meta",
                ],
            )?;
            if non_null(object, "type").is_some_and(|v| v != "object") {
                return Err(invalid("Form schema type must be object"));
            }
            let empty = Map::new();
            let properties = match object.get("properties") {
                None => &empty,
                Some(value) => value
                    .as_object()
                    .ok_or_else(|| invalid("Invalid schema properties"))?,
            };
            if properties.len() > MAX_FIELDS {
                return Err(invalid("Too many form fields"));
            }
            let mut fields = BTreeMap::new();
            for (name, value) in properties {
                if name.is_empty()
                    || name.len() > 256
                    || matches!(name.as_str(), "__proto__" | "prototype" | "constructor")
                {
                    return Err(invalid("Unsafe form field name"));
                }
                let object = value
                    .as_object()
                    .ok_or_else(|| invalid("Field schema must be an object"))?;
                if secret(name, object) {
                    return Err(invalid(
                        "Host policy prohibits credential fields in forms; use URL mode",
                    ));
                }
                fields.insert(name.clone(), Field::parse(object, patterns)?);
            }
            let mut required = HashSet::new();
            if let Some(value) = non_null(object, "required") {
                for name in value
                    .as_array()
                    .ok_or_else(|| invalid("required must be an array"))?
                {
                    let name = name
                        .as_str()
                        .ok_or_else(|| invalid("Invalid required field"))?;
                    if !fields.contains_key(name) || !required.insert(name.to_owned()) {
                        return Err(invalid("Unknown or duplicate required field"));
                    }
                }
            }
            Ok(Self { fields, required })
        }

        fn defaults(&self) -> Map<String, Value> {
            self.fields
                .iter()
                .filter_map(|(name, field)| {
                    non_null(&field.schema, "default").map(|value| (name.clone(), value.clone()))
                })
                .collect()
        }

        fn validate_content(
            &self,
            content: Option<&Value>,
            patterns: &PatternValidator,
        ) -> Result<Option<Value>> {
            // Omitted and explicit null are equivalent, but neither bypasses required fields.
            let Some(content) = content.filter(|v| !v.is_null()) else {
                return if self.required.is_empty() {
                    Ok(None)
                } else {
                    Err(invalid("Required form fields are missing"))
                };
            };
            if content.to_string().len() > MAX_PAYLOAD {
                return Err(invalid("Form response exceeds host limit"));
            }
            let values = content
                .as_object()
                .ok_or_else(|| invalid("Form content must be an object"))?;
            if self.required.iter().any(|key| !values.contains_key(key)) {
                return Err(invalid("Required form fields are missing"));
            }
            for (name, value) in values {
                let field = self
                    .fields
                    .get(name)
                    .ok_or_else(|| invalid("Unknown form response field"))?;
                field.validate(value, patterns)?;
            }
            // Defaults are presented for review, never silently inserted after consent.
            Ok(Some(content.clone()))
        }
    }

    fn choices(value: &Value, titled: bool) -> Result<Vec<String>> {
        let items = value
            .as_array()
            .ok_or_else(|| invalid("Enum choices must be an array"))?;
        let mut result = Vec::new();
        for item in items {
            let value = if titled {
                let object = item
                    .as_object()
                    .ok_or_else(|| invalid("Invalid titled enum"))?;
                allowed(object, &["const", "title", "description", "_meta"])?;
                if !object.get("title").is_some_and(Value::is_string) {
                    return Err(invalid("Enum title is required"));
                }
                object.get("const").and_then(Value::as_str)
            } else {
                item.as_str()
            }
            .ok_or_else(|| invalid("Enum values must be strings"))?;
            if result.iter().any(|v| v == value) {
                return Err(invalid("Duplicate enum value"));
            }
            result.push(value.to_owned());
        }
        Ok(result)
    }

    impl Field {
        fn parse(object: &Map<String, Value>, patterns: &PatternValidator) -> Result<Self> {
            let kind = text(object.get("type"), "field type")?;
            let mut keys = vec!["type", "title", "description", "default", "_meta"];
            let mut selected = None;
            match kind {
                "string" => {
                    keys.extend([
                        "minLength",
                        "maxLength",
                        "pattern",
                        "format",
                        "enum",
                        "oneOf",
                    ]);
                    lengths(object, "minLength", "maxLength", u32::MAX as u64)?;
                    if let Some(pattern) = non_null(object, "pattern") {
                        let pattern = pattern
                            .as_str()
                            .ok_or_else(|| invalid("pattern must be a string"))?;
                        if !patterns(pattern, None)
                            .map_err(|_| invalid("Invalid or unsupported pattern constraint"))?
                        {
                            return Err(invalid("Invalid or unsupported pattern constraint"));
                        }
                    }
                    if non_null(object, "format").is_some_and(|format| {
                        !matches!(
                            format.as_str(),
                            Some("date" | "date-time" | "email" | "uri")
                        )
                    }) {
                        return Err(invalid("Unsupported string format"));
                    }
                    if let Some(value) = non_null(object, "enum") {
                        selected = Some(choices(value, false)?);
                    }
                    if let Some(value) = non_null(object, "oneOf") {
                        let titled = choices(value, true)?;
                        selected = Some(match selected {
                            Some(plain) => {
                                plain.into_iter().filter(|v| titled.contains(v)).collect()
                            }
                            None => titled,
                        });
                    }
                }
                "number" | "integer" => {
                    keys.extend(["minimum", "maximum"]);
                    for key in ["minimum", "maximum"] {
                        if let Some(value) = non_null(object, key) {
                            if value.as_f64().filter(|n| n.is_finite()).is_none()
                                || (kind == "integer" && value.as_i64().is_none())
                            {
                                return Err(invalid("Invalid numeric bound"));
                            }
                        }
                    }
                    if let (Some(min), Some(max)) =
                        (non_null(object, "minimum"), non_null(object, "maximum"))
                    {
                        let inverted = if kind == "integer" {
                            min.as_i64() > max.as_i64()
                        } else {
                            min.as_f64() > max.as_f64()
                        };
                        if inverted {
                            return Err(invalid("Inverted numeric bounds"));
                        }
                    }
                }
                "boolean" => {}
                "array" => {
                    keys.extend(["items", "minItems", "maxItems"]);
                    lengths(object, "minItems", "maxItems", u64::MAX)?;
                    let items = object
                        .get("items")
                        .and_then(Value::as_object)
                        .ok_or_else(|| invalid("Multi-select items are required"))?;
                    if items.get("type") == Some(&json!("string")) {
                        allowed(items, &["type", "enum", "_meta"])?;
                        selected = Some(choices(
                            items
                                .get("enum")
                                .ok_or_else(|| invalid("Multi-select enum is required"))?,
                            false,
                        )?);
                    } else if !items.contains_key("type") {
                        allowed(items, &["anyOf", "_meta"])?;
                        selected = Some(choices(
                            items
                                .get("anyOf")
                                .ok_or_else(|| invalid("Titled multi-select anyOf is required"))?,
                            true,
                        )?);
                    } else {
                        return Err(invalid("Unsupported multi-select items"));
                    }
                }
                _ => return Err(invalid("Unsupported form field type")),
            }
            allowed(object, &keys)?;
            let field = Self {
                schema: object.clone(),
                kind: kind.into(),
                choices: selected,
            };
            if let Some(default) = non_null(object, "default") {
                field.validate(default, patterns)?;
            }
            Ok(field)
        }

        fn validate(&self, value: &Value, patterns: &PatternValidator) -> Result<()> {
            let bad = || invalid("Form value does not conform to the stored schema");
            match self.kind.as_str() {
                "string" => {
                    let string = value.as_str().ok_or_else(bad)?;
                    if !within_length(
                        &self.schema,
                        "minLength",
                        "maxLength",
                        string.chars().count() as u64,
                    ) || self
                        .choices
                        .as_ref()
                        .is_some_and(|v| !v.iter().any(|s| s == string))
                    {
                        return Err(bad());
                    }
                    if let Some(pattern) = non_null(&self.schema, "pattern").and_then(Value::as_str)
                    {
                        if !patterns(pattern, Some(string))
                            .map_err(|_| invalid("Pattern validation failed"))?
                        {
                            return Err(bad());
                        }
                    }
                    if let Some(format) = non_null(&self.schema, "format").and_then(Value::as_str) {
                        if !valid_format(format, string) {
                            return Err(bad());
                        }
                    }
                }
                "number" => {
                    let number = value.as_f64().filter(|n| n.is_finite()).ok_or_else(bad)?;
                    if non_null(&self.schema, "minimum")
                        .and_then(Value::as_f64)
                        .is_some_and(|n| number < n)
                        || non_null(&self.schema, "maximum")
                            .and_then(Value::as_f64)
                            .is_some_and(|n| number > n)
                    {
                        return Err(bad());
                    }
                }
                "integer" => {
                    let number = value.as_i64().ok_or_else(bad)?;
                    if non_null(&self.schema, "minimum")
                        .and_then(Value::as_i64)
                        .is_some_and(|n| number < n)
                        || non_null(&self.schema, "maximum")
                            .and_then(Value::as_i64)
                            .is_some_and(|n| number > n)
                    {
                        return Err(bad());
                    }
                }
                "boolean" => {
                    if !value.is_boolean() {
                        return Err(bad());
                    }
                }
                "array" => {
                    let values = value.as_array().ok_or_else(bad)?;
                    if !within_length(&self.schema, "minItems", "maxItems", values.len() as u64) {
                        return Err(bad());
                    }
                    let mut seen = HashSet::new();
                    for value in values {
                        let string = value.as_str().ok_or_else(bad)?;
                        if !self.choices.as_ref().unwrap().iter().any(|v| v == string)
                            || !seen.insert(string)
                        {
                            return Err(bad());
                        }
                    }
                }
                _ => return Err(bad()),
            }
            Ok(())
        }
    }

    fn lengths(object: &Map<String, Value>, min: &str, max: &str, limit: u64) -> Result<()> {
        for key in [min, max] {
            if let Some(value) = non_null(object, key) {
                if value.as_u64().is_none_or(|n| n > limit) {
                    return Err(invalid("Invalid length bound"));
                }
            }
        }
        if let (Some(min), Some(max)) = (non_null(object, min), non_null(object, max)) {
            if min.as_u64() > max.as_u64() {
                return Err(invalid("Inverted length bounds"));
            }
        }
        Ok(())
    }

    fn within_length(object: &Map<String, Value>, min: &str, max: &str, length: u64) -> bool {
        non_null(object, min)
            .and_then(Value::as_u64)
            .is_none_or(|min| length >= min)
            && non_null(object, max)
                .and_then(Value::as_u64)
                .is_none_or(|max| length <= max)
    }

    fn valid_date(value: &str) -> bool {
        let bytes = value.as_bytes();
        if bytes.len() != 10
            || bytes[4] != b'-'
            || bytes[7] != b'-'
            || !bytes
                .iter()
                .enumerate()
                .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
        {
            return false;
        }
        let year = value[..4].parse::<u32>().unwrap();
        let month = value[5..7].parse::<u32>().unwrap();
        let day = value[8..].parse::<u32>().unwrap();
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let days = match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if leap => 29,
            2 => 28,
            _ => 0,
        };
        day > 0 && day <= days
    }

    fn valid_datetime(value: &str) -> bool {
        if !value.is_ascii()
            || value.len() < 20
            || !valid_date(&value[..10])
            || !matches!(value.as_bytes()[10], b'T' | b't')
        {
            return false;
        }
        let rest = &value[11..];
        let bytes = rest.as_bytes();
        if bytes[2] != b':'
            || bytes[5] != b':'
            || ![0, 1, 3, 4, 6, 7]
                .iter()
                .all(|i| bytes[*i].is_ascii_digit())
        {
            return false;
        }
        let hour = rest[..2].parse::<u32>().unwrap();
        let minute = rest[3..5].parse::<u32>().unwrap();
        let second = rest[6..8].parse::<u32>().unwrap();
        if hour > 23 || minute > 59 || second > 60 {
            return false;
        }
        let mut tail = &rest[8..];
        if tail.starts_with('.') {
            tail = &tail[1..];
            let len = tail.bytes().take_while(u8::is_ascii_digit).count();
            if len == 0 {
                return false;
            }
            tail = &tail[len..];
        }
        let offset = if tail.eq_ignore_ascii_case("z") {
            0
        } else {
            let b = tail.as_bytes();
            if b.len() != 6
                || !matches!(b[0], b'+' | b'-')
                || b[3] != b':'
                || ![1, 2, 4, 5].iter().all(|i| b[*i].is_ascii_digit())
            {
                return false;
            }
            let h = tail[1..3].parse::<i32>().unwrap();
            let m = tail[4..].parse::<i32>().unwrap();
            if h > 23 || m > 59 {
                return false;
            }
            (h * 60 + m) * if b[0] == b'-' { -1 } else { 1 }
        };
        // RFC3339 permits leap seconds only at 23:59 UTC (including offset equivalents).
        second != 60 || ((hour * 60 + minute) as i32 - offset).rem_euclid(1440) == 1439
    }

    fn valid_email(value: &str) -> bool {
        if !value.is_ascii() || value.len() > 254 {
            return false;
        }
        let Some((local, domain)) = value.rsplit_once('@') else {
            return false;
        };
        if local.is_empty() || local.len() > 64 || domain.is_empty() {
            return false;
        }
        let local_valid = if local.starts_with('"') && local.ends_with('"') && local.len() >= 2 {
            let mut escaped = false;
            let mut valid = true;
            for b in local[1..local.len() - 1].bytes() {
                if escaped {
                    valid &= (32..=126).contains(&b);
                    escaped = false;
                } else if b == b'\\' {
                    escaped = true;
                } else {
                    valid &= (32..=126).contains(&b) && b != b'"';
                }
            }
            valid && !escaped
        } else {
            !local.starts_with('.')
                && !local.ends_with('.')
                && !local.contains("..")
                && local
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-/=?^_`{|}~.".contains(&b))
        };
        let domain_valid = if domain.starts_with('[') && domain.ends_with(']') {
            let address = &domain[1..domain.len() - 1];
            address
                .strip_prefix("IPv6:")
                .unwrap_or(address)
                .parse::<std::net::IpAddr>()
                .is_ok()
        } else {
            domain.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
        };
        local_valid && domain_valid
    }

    fn valid_format(format: &str, value: &str) -> bool {
        match format {
            "date" => valid_date(value),
            "date-time" => valid_datetime(value),
            "email" => valid_email(value),
            "uri" => {
                if !value.is_ascii()
                    || value.bytes().any(|b| {
                        !b.is_ascii_alphanumeric() && !b"-._~:/?#[]@!$&'()*+,;=%".contains(&b)
                    })
                {
                    return false;
                }
                let bytes = value.as_bytes();
                for (i, b) in bytes.iter().enumerate() {
                    if *b == b'%'
                        && (i + 2 >= bytes.len()
                            || !bytes[i + 1].is_ascii_hexdigit()
                            || !bytes[i + 2].is_ascii_hexdigit())
                    {
                        return false;
                    }
                }
                Url::parse(value).is_ok()
            }
            _ => false,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        type Events = Arc<Mutex<Vec<(String, Value)>>>;

        fn context() -> ScopeContext {
            ScopeContext {
                connection_id: "connection-one".into(),
                current_session_id: Some("session-one".into()),
                session_generation: Some(0),
                in_flight_request_ids: vec![json!(12), json!("parent-two")],
                tool_call_ids: vec!["tool-one".into()],
            }
        }

        fn pattern_fixture() -> PatternValidator {
            Arc::new(evaluate_pattern)
        }

        #[test]
        fn native_ecmascript_patterns_support_lookahead_and_backreferences() {
            assert!(evaluate_pattern(r"^(?=.{3}$)[A-Z]+$", Some("ABC")).unwrap());
            assert!(!evaluate_pattern(r"^(?=.{3}$)[A-Z]+$", Some("AB")).unwrap());
            assert!(evaluate_pattern(r"^(ab)\1$", Some("abab")).unwrap());
            assert!(evaluate_pattern("[", None).is_err());
        }

        fn controller(browser_fails: bool) -> (Arc<Controller>, Events, Arc<AtomicUsize>) {
            let events = Arc::new(Mutex::new(Vec::new()));
            let sink = events.clone();
            let opens = Arc::new(AtomicUsize::new(0));
            let opener = opens.clone();
            (
                Controller::with_hooks(
                    Arc::new(move |name, value| {
                        sink.lock().unwrap().push((name.into(), value));
                        Ok(())
                    }),
                    Arc::new(move |url| {
                        assert_eq!(url, "https://example.com/connect?opaque=1");
                        opener.fetch_add(1, Ordering::SeqCst);
                        if browser_fails {
                            Err("browser failed".into())
                        } else {
                            Ok(())
                        }
                    }),
                    Some(pattern_fixture()),
                ),
                events,
                opens,
            )
        }

        fn form(schema: Value) -> Value {
            json!({"sessionId":"session-one","mode":"form","message":"Choose fields","requestedSchema":schema})
        }

        fn url(id: &str) -> Value {
            json!({"sessionId":"session-one","mode":"url","message":"Open sign-in?",
                "elicitationId":id,"url":"https://example.com/connect?opaque=1"})
        }

        fn response(action: Action, content: Option<Value>) -> Response {
            Response { action, content }
        }

        fn receive(ticket: &Ticket) -> Result<Response> {
            ticket
                .response
                .recv_timeout(Duration::from_secs(1))
                .expect("resolved waiter")
        }

        #[test]
        fn mode_capabilities_fail_closed_without_native_pattern_engine() {
            let no_patterns =
                Controller::with_hooks(Arc::new(|_, _| Ok(())), Arc::new(|_| Ok(())), None);
            assert_eq!(no_patterns.capabilities(), json!({"url":{}}));
            assert_eq!(
                no_patterns
                    .begin(form(json!({})), &context())
                    .err()
                    .unwrap()
                    .code,
                -32602
            );
            let (controller, _, _) = controller(false);
            assert_eq!(controller.capabilities(), json!({"form":{}, "url":{}}));
            assert!(controller
                .begin(
                    json!({"sessionId":"session-one","mode":"future","message":"?"}),
                    &context()
                )
                .is_err());
        }

        #[test]
        fn invalid_or_unowned_scope_is_rejected_without_events() {
            let (controller, events, _) = controller(false);
            let base = form(json!({}));
            for scope in [
                json!({}),
                json!({"sessionId":"other"}),
                json!({"sessionId":"session-one","requestId":12}),
                json!({"requestId":null}),
                json!({"requestId":"12"}),
                json!({"requestId":1.5}),
                json!({"sessionId":null,"requestId":12}),
                json!({"toolCallId":"tool-one","requestId":12}),
                json!({"sessionId":"session-one","toolCallId":"unknown"}),
            ] {
                let mut request = base.clone();
                request.as_object_mut().unwrap().remove("sessionId");
                request
                    .as_object_mut()
                    .unwrap()
                    .extend(scope.as_object().unwrap().clone());
                assert!(controller.begin(request, &context()).is_err(), "{scope}");
            }
            let mut owner = context();
            owner.connection_id.clear();
            assert!(controller.begin(base, &owner).is_err());
            assert!(events.lock().unwrap().is_empty());
        }

        #[test]
        fn request_ids_preserve_numeric_and_string_types_and_cancel_exact_owner() {
            let (controller, _, _) = controller(false);
            let mut owner = context();
            owner.in_flight_request_ids.push(json!("12"));
            let request =
                |id| json!({"requestId":id,"mode":"form","message":"?","requestedSchema":{}});
            let numeric = controller.begin(request(json!(12)), &owner).unwrap();
            let string = controller.begin(request(json!("12")), &owner).unwrap();
            controller.cancel_request("connection-one", &json!(12));
            assert_eq!(receive(&numeric).unwrap_err().code, -32800);
            assert!(string.response.try_recv().is_err());
            controller
                .respond(
                    &string.request.request_id,
                    response(Action::Decline, None),
                    &owner,
                )
                .unwrap();
            assert_eq!(receive(&string).unwrap().action, Action::Decline);
        }

        #[test]
        fn response_requires_same_connection_session_and_live_scope() {
            let (controller, _, _) = controller(false);
            let ticket = controller.begin(form(json!({})), &context()).unwrap();
            let mut wrong = context();
            wrong.connection_id = "other".into();
            assert!(controller
                .respond(
                    &ticket.request.request_id,
                    response(Action::Accept, None),
                    &wrong
                )
                .is_err());
            wrong = context();
            wrong.current_session_id = Some("other".into());
            assert!(controller
                .respond(
                    &ticket.request.request_id,
                    response(Action::Accept, None),
                    &wrong
                )
                .is_err());
            let ticket2 = controller
                .begin(
                    json!({"requestId":12,"mode":"form","message":"?","requestedSchema":{}}),
                    &context(),
                )
                .unwrap();
            wrong = context();
            wrong.in_flight_request_ids.clear();
            assert!(controller
                .respond(
                    &ticket2.request.request_id,
                    response(Action::Accept, None),
                    &wrong
                )
                .is_err());
            controller.cancel_all();
            assert!(receive(&ticket).is_err());
            assert!(receive(&ticket2).is_err());
        }

        #[test]
        fn invalid_schemas_unknown_constraints_and_credentials_are_refused() {
            let (controller, _, _) = controller(false);
            for field in [
                json!({"type":"object"}),
                json!({"type":"string","format":"password"}),
                json!({"type":"string","minLength":-1}),
                json!({"type":"string","minLength":3,"maxLength":2}),
                json!({"type":"number","minimum":"1"}),
                json!({"type":"number","multipleOf":2}),
                json!({"type":"boolean","default":"yes"}),
                json!({"type":"string","pattern":"["}),
                json!({"type":"string","default":"lower","pattern":"^[A-Z]+$"}),
                json!({"type":"array","items":{"type":"number"}}),
                json!({"type":"array","items":{"type":"string","enum":["a"]},"default":["b"]}),
                json!({"type":"string","enum":["a","a"]}),
            ] {
                assert!(
                    controller
                        .begin(form(json!({"properties":{"field":field}})), &context())
                        .is_err(),
                    "{field}"
                );
            }
            for name in [
                "password",
                "api_key",
                "accessToken",
                "private_key",
                "credit_card_number",
                "cvv",
                "paymentPIN",
            ] {
                assert!(controller
                    .begin(
                        form(json!({"properties":{name:{"type":"string"}}})),
                        &context()
                    )
                    .is_err());
            }
            for schema in [
                json!({"properties":{"field":{"type":"string","description":"Enter your API token"}}}),
                json!({"properties":{"constructor":{"type":"string"}}}),
                json!({"properties":{},"required":["missing"]}),
                json!({"type":"object","additionalProperties":true}),
            ] {
                assert!(controller.begin(form(schema), &context()).is_err());
            }
            let ordinary = form(
                json!({"properties":{"name":{"type":"string"},"email":{"type":"string","format":"email"},"username":{"type":"string"}}}),
            );
            assert!(controller.begin(ordinary, &context()).is_ok());
        }

        #[test]
        fn validates_all_types_against_stored_schema_and_keeps_invalid_submission_pending() {
            let (controller, events, _) = controller(false);
            let schema = json!({"type":"object","properties":{
                "label":{"type":"string","minLength":2,"maxLength":4,"pattern":"^[A-Z]+$"},
                "count":{"type":"integer","minimum":1,"maximum":3},
                "ratio":{"type":"number","minimum":0.1,"maximum":0.9},
                "flag":{"type":"boolean"},
                "choice":{"type":"string","oneOf":[{"const":"a","title":"A"},{"const":"b","title":"B"}]},
                "many":{"type":"array","minItems":1,"maxItems":2,"items":{"anyOf":[{"const":"x","title":"X"},{"const":"y","title":"Y"}]}}
            },"required":["label","count","ratio","flag","choice","many"]});
            let ticket = controller.begin(form(schema.clone()), &context()).unwrap();
            assert_eq!(ticket.request.requested_schema, Some(schema));
            let good = json!({"label":"AB","count":2,"ratio":0.5,"flag":false,"choice":"b","many":["x","y"]});
            for (field, bad) in [
                ("label", json!("a")),
                ("label", json!("ABCDE")),
                ("count", json!(2.5)),
                ("count", json!(4)),
                ("ratio", json!(1.0)),
                ("flag", json!("true")),
                ("choice", json!("c")),
                ("many", json!(["x", "x"])),
                ("many", json!(["z"])),
                ("many", json!([])),
                ("extra", json!("injected")),
                ("label", Value::Null),
            ] {
                let mut value = good.clone();
                value[field] = bad;
                assert!(
                    controller
                        .respond(
                            &ticket.request.request_id,
                            response(Action::Accept, Some(value)),
                            &context()
                        )
                        .is_err(),
                    "{field}"
                );
                assert!(ticket.response.try_recv().is_err());
            }
            controller
                .respond(
                    &ticket.request.request_id,
                    response(Action::Accept, Some(good.clone())),
                    &context(),
                )
                .unwrap();
            assert_eq!(receive(&ticket).unwrap().content, Some(good));
            assert_eq!(events.lock().unwrap().last().unwrap().0, CLOSED_EVENT);
        }

        #[test]
        fn integer_bounds_do_not_lose_precision_via_f64() {
            let (controller, _, _) = controller(false);
            let ticket = controller.begin(form(json!({"properties":{"n":{"type":"integer","minimum":9007199254740993_i64}}})), &context()).unwrap();
            assert!(controller
                .respond(
                    &ticket.request.request_id,
                    response(Action::Accept, Some(json!({"n":9007199254740992_i64}))),
                    &context()
                )
                .is_err());
        }

        #[test]
        fn omitted_null_and_empty_content_are_explicit_and_never_fill_defaults_after_consent() {
            let (controller, _, _) = controller(false);
            let schema = json!({"properties":{"choice":{"type":"string","enum":["a","b"],"default":"b"},
                "multi":{"type":"array","items":{"type":"string","enum":["x","y"]},"default":["y"]},
                "flag":{"type":"boolean","default":false}}});
            for content in [None, Some(Value::Null), Some(json!({}))] {
                let ticket = controller.begin(form(schema.clone()), &context()).unwrap();
                assert_eq!(
                    ticket.request.defaults,
                    Some(json!({"choice":"b","multi":["y"],"flag":false}))
                );
                controller
                    .respond(
                        &ticket.request.request_id,
                        response(Action::Accept, content.clone()),
                        &context(),
                    )
                    .unwrap();
                assert_eq!(
                    receive(&ticket).unwrap().content,
                    content.filter(|v| !v.is_null())
                );
            }
            let required = controller.begin(form(json!({"properties":{"name":{"type":"string","default":"review me"}},"required":["name"]})), &context()).unwrap();
            for content in [None, Some(Value::Null), Some(json!({}))] {
                assert!(controller
                    .respond(
                        &required.request.request_id,
                        response(Action::Accept, content),
                        &context()
                    )
                    .is_err());
            }
            let omitted: Response = serde_json::from_value(json!({"action":"accept"})).unwrap();
            let null: Response =
                serde_json::from_value(json!({"action":"accept","content":null})).unwrap();
            assert_eq!(omitted, null);
            assert!(serde_json::from_value::<Response>(json!({"action":"future"})).is_err());
            assert!(serde_json::from_value::<Response>(
                json!({"action":"accept","url":"https://evil.example"})
            )
            .is_err());
        }

        #[test]
        fn format_checks_include_calendar_leap_days_timezones_email_and_absolute_uris() {
            for (format, valid, invalid) in [
                ("date", "2024-02-29", "2025-02-29"),
                (
                    "date-time",
                    "2024-02-29T23:59:59.125+01:30",
                    "2024-02-29T24:00:00Z",
                ),
                (
                    "email",
                    "first.last+tag@example.com",
                    "name@-invalid.example",
                ),
                ("uri", "urn:isbn:123", "relative/path"),
            ] {
                let (controller, _, _) = controller(false);
                let ticket = controller
                    .begin(
                        form(json!({"properties":{"value":{"type":"string","format":format}}})),
                        &context(),
                    )
                    .unwrap();
                assert!(controller
                    .respond(
                        &ticket.request.request_id,
                        response(Action::Accept, Some(json!({"value":invalid}))),
                        &context()
                    )
                    .is_err());
                controller
                    .respond(
                        &ticket.request.request_id,
                        response(Action::Accept, Some(json!({"value":valid}))),
                        &context(),
                    )
                    .unwrap();
            }
            assert!(!valid_format("uri", "https://example.com/%zz"));
            assert!(!valid_format("uri", "https://example.com/a b"));
            assert!(valid_format("date-time", "2016-12-31T23:59:60Z"));
            assert!(!valid_format("date-time", "2016-12-31T12:00:60Z"));
            assert!(!valid_format("date-time", "2024-01-01T10:00:00."));
            assert!(valid_format("email", "\"quoted local\"@example.com"));
        }

        #[test]
        fn url_security_is_enforced_before_any_event_or_browser_call() {
            let (controller, events, opens) = controller(false);
            for value in [
                "javascript:alert(1)",
                "file:///C:/test",
                "http://example.com",
                "https://user@example.com",
                "https://@example.com",
                "https://example.com\\@evil.example",
                "https:example.com",
                "https://example.com/\n",
                "ftp://example.com",
            ] {
                let mut request = url("one");
                request["url"] = json!(value);
                assert!(controller.begin(request, &context()).is_err(), "{value}");
            }
            for value in [
                "http://localhost:3000/auth",
                "http://127.0.0.1:8080",
                "http://[::1]:8080",
                "https://example.com",
            ] {
                assert!(validate_url(value).is_ok(), "{value}");
            }
            assert!(events.lock().unwrap().is_empty());
            assert_eq!(opens.load(Ordering::SeqCst), 0);
        }

        #[test]
        fn url_decline_and_cancel_ignore_content_and_never_open_browser() {
            let (controller, _, opens) = controller(false);
            for action in [Action::Decline, Action::Cancel] {
                let ticket = controller.begin(url("one"), &context()).unwrap();
                assert_eq!(ticket.request.domain.as_deref(), Some("example.com"));
                assert_eq!(
                    ticket.request.url.as_deref(),
                    Some("https://example.com/connect?opaque=1")
                );
                assert_eq!(opens.load(Ordering::SeqCst), 0);
                controller
                    .respond(
                        &ticket.request.request_id,
                        response(action, Some(json!({"url":"https://evil.example"}))),
                        &context(),
                    )
                    .unwrap();
                assert_eq!(
                    receive(&ticket).unwrap(),
                    Response {
                        action,
                        content: None
                    }
                );
            }
            assert_eq!(opens.load(Ordering::SeqCst), 0);
            assert!(controller.state.lock().unwrap().urls.is_empty());
        }

        #[test]
        fn url_accept_opens_stored_url_only_and_completion_is_separate_idempotent() {
            let (controller, events, opens) = controller(false);
            let ticket = controller.begin(url("opaque-id"), &context()).unwrap();
            assert!(controller.begin(url("opaque-id"), &context()).is_err());
            controller
                .respond(
                    &ticket.request.request_id,
                    response(
                        Action::Accept,
                        Some(json!({"url":"https://evil.example","done":true})),
                    ),
                    &context(),
                )
                .unwrap();
            assert_eq!(
                receive(&ticket).unwrap(),
                Response {
                    action: Action::Accept,
                    content: None
                }
            );
            assert_eq!(opens.load(Ordering::SeqCst), 1);
            assert!(controller.begin(url("opaque-id"), &context()).is_err());
            controller.complete("other", "opaque-id");
            controller.complete("connection-one", "unknown");
            assert_eq!(controller.state.lock().unwrap().urls.len(), 1);
            controller.complete("connection-one", "opaque-id");
            controller.complete("connection-one", "opaque-id");
            assert!(controller.state.lock().unwrap().urls.is_empty());
            assert_eq!(
                events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(name, _)| name == COMPLETE_EVENT)
                    .count(),
                1
            );
            assert!(controller.begin(url("opaque-id"), &context()).is_ok());
        }

        #[test]
        fn browser_failure_is_reported_without_resolving_success_or_losing_pending_request() {
            let (controller, _, opens) = controller(true);
            let ticket = controller.begin(url("one"), &context()).unwrap();
            let error = controller
                .respond(
                    &ticket.request.request_id,
                    response(Action::Accept, None),
                    &context(),
                )
                .unwrap_err();
            assert_eq!(error.code, -32603);
            assert!(ticket.response.try_recv().is_err());
            assert_eq!(opens.load(Ordering::SeqCst), 1);
            controller
                .respond(
                    &ticket.request.request_id,
                    response(Action::Cancel, None),
                    &context(),
                )
                .unwrap();
            assert_eq!(receive(&ticket).unwrap().action, Action::Cancel);
            assert_eq!(opens.load(Ordering::SeqCst), 1);
        }

        #[test]
        fn complete_before_consent_is_informational_and_does_not_open_browser() {
            let (controller, events, opens) = controller(false);
            let ticket = controller.begin(url("early"), &context()).unwrap();
            controller.complete("connection-one", "early");
            controller.complete("connection-one", "early");
            assert!(ticket.response.try_recv().is_err());
            assert_eq!(opens.load(Ordering::SeqCst), 0);
            assert_eq!(
                events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(name, _)| name == COMPLETE_EVENT)
                    .count(),
                1
            );
            controller
                .respond(
                    &ticket.request.request_id,
                    response(Action::Cancel, None),
                    &context(),
                )
                .unwrap();
            assert!(controller.state.lock().unwrap().urls.is_empty());
        }

        #[test]
        fn cleanup_interrupts_waiters_and_clears_accepted_url_state() {
            let (controller, events, _) = controller(false);
            let accepted = controller.begin(url("accepted"), &context()).unwrap();
            controller
                .respond(
                    &accepted.request.request_id,
                    response(Action::Accept, None),
                    &context(),
                )
                .unwrap();
            let pending = controller.begin(url("pending"), &context()).unwrap();
            let form = controller.begin(form(json!({})), &context()).unwrap();
            controller.cancel_session("other", "session-one");
            assert!(pending.response.try_recv().is_err());
            controller.cancel_session("connection-one", "session-one");
            assert_eq!(receive(&pending).unwrap_err().code, -32800);
            assert_eq!(receive(&form).unwrap_err().code, -32800);
            assert!(controller.state.lock().unwrap().pending.is_empty());
            assert!(controller.state.lock().unwrap().urls.is_empty());
            assert!(controller
                .respond(
                    &pending.request.request_id,
                    response(Action::Accept, None),
                    &context()
                )
                .is_err());
            assert_eq!(
                events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(name, _)| name == CLOSED_EVENT)
                    .count(),
                3
            );
        }

        #[test]
        fn connection_cleanup_and_controller_drop_interrupt_pending_requests() {
            let (controller, _, _) = controller(false);
            let ticket = controller.begin(form(json!({})), &context()).unwrap();
            controller.cancel_connection("connection-one");
            assert!(receive(&ticket).is_err());
            let ticket = controller.begin(form(json!({})), &context()).unwrap();
            drop(controller);
            assert_eq!(receive(&ticket).unwrap_err().code, -32800);
        }

        #[test]
        fn bounded_requests_and_failed_event_delivery_do_not_leak() {
            let (controller, _, _) = controller(false);
            let mut tickets = Vec::new();
            for _ in 0..LIMIT {
                tickets.push(controller.begin(form(json!({})), &context()).unwrap());
            }
            assert!(controller.begin(form(json!({})), &context()).is_err());
            controller.cancel_all();
            assert!(tickets.iter().all(|ticket| receive(ticket).is_err()));
            let failing = Controller::with_hooks(
                Arc::new(|_, _| Err("window closed".into())),
                Arc::new(|_| panic!("must not open")),
                Some(pattern_fixture()),
            );
            assert_eq!(
                failing.begin(url("failed"), &context()).err().unwrap().code,
                -32603
            );
            assert!(failing.state.lock().unwrap().pending.is_empty());
            assert!(failing.state.lock().unwrap().urls.is_empty());
        }
    }
}

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use github_copilot_sdk::handler::{PermissionHandler, PermissionResult};
use github_copilot_sdk::rpc::{
    FleetStartRequest, InstalledPluginInfo, PluginsDisableRequest, PluginsEnableRequest,
    RemoteSessionMetadataValue, SessionSource, SessionsListRequest, TasksCancelRequest,
    TasksSendMessageRequest, TasksStartAgentRequest,
};
use github_copilot_sdk::session::Session;
use github_copilot_sdk::{
    CliProgram, Client, ClientInfo, ClientMode, ClientOptions, CreateSessionOptions,
    MemoryConfiguration, PermissionRequestData, PermissionRequestKind, RequestId,
    RuntimeConnection, SessionId, SpawnedProcessOptions,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Runtime};

use crate::copilot::{self, record_transport, CopilotAgent, CopilotPermissionOrigin, Operation};
use crate::copilot_client::{ClientConsent, ConsentDecision};
use crate::problem::Problem;

pub const SCHEMA_VERSION: u8 = 1;
const SDK_VERSION: &str = "2.0.1";
const RPC_TIMEOUT: Duration = Duration::from_secs(60);
const OPEN_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_ROWS: usize = 200;
const MAX_TEXT: usize = 16_000;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum Feature<T> {
    Available { data: T },
    Unavailable { problem: Problem },
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionPlugin {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub version: Option<String>,
    pub can_toggle: bool,
    pub origin: &'static str,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionTask {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub title: String,
    pub output: Option<String>,
    pub output_truncated: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskMetadata {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    status: String,
    description: Option<String>,
    display_name: Option<String>,
    result: Option<String>,
    latest_response: Option<String>,
    error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteSession {
    pub id: String,
    pub title: String,
    pub modified_at: String,
    pub state: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionSnapshot {
    pub schema_version: u8,
    pub sdk_version: String,
    pub runtime_version: String,
    pub protocol_version: u32,
    pub connection_id: String,
    pub session_id: String,
    pub cwd: String,
    pub memory_enabled: bool,
    pub agents: Feature<Vec<CopilotAgent>>,
    pub tasks: Feature<Vec<ExtensionTask>>,
    pub plugins: Feature<Vec<ExtensionPlugin>>,
    pub remote_sessions: Option<Feature<Vec<RemoteSession>>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionRequest {
    pub schema_version: u8,
    pub action: ExtensionAction,
}

#[derive(Debug, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ExtensionAction {
    Connect {
        cwd: String,
        memory_enabled: bool,
    },
    Disconnect {
        connection_id: String,
    },
    Refresh {
        connection_id: String,
    },
    StartTask {
        connection_id: String,
        agent_id: String,
        name: String,
        prompt: String,
    },
    CancelTask {
        connection_id: String,
        task_id: String,
    },
    SendTaskMessage {
        connection_id: String,
        task_id: String,
        message: String,
    },
    StartFleet {
        connection_id: String,
        prompt: String,
    },
    SetPluginEnabled {
        connection_id: String,
        plugin_id: String,
        enabled: bool,
    },
    ListRemote {
        connection_id: String,
    },
}

struct Workspace {
    client: Client,
    session: Session,
    id: String,
    cwd: String,
    memory_enabled: bool,
    version: String,
    protocol: u32,
    closed: Arc<AtomicBool>,
    consent: Arc<ClientConsent>,
    remote: Mutex<Option<Feature<Vec<RemoteSession>>>>,
    cached: Mutex<Option<ExtensionSnapshot>>,
}

enum RpcAccess {
    Read(Operation),
    Mutation,
}

async fn observe_read<T>(
    cwd: &str,
    operation: Operation,
    future: impl std::future::Future<Output = Result<T, Problem>>,
) -> Result<T, Problem> {
    let started = Instant::now();
    let result = future.await;
    record_transport(Some(cwd), operation, started, result.is_ok());
    result
}

impl Workspace {
    fn shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.consent.cancel(&self.id, None);
        self.client.force_stop();
    }

    async fn rpc<T>(
        &self,
        operation: &str,
        access: RpcAccess,
        future: impl std::future::Future<Output = Result<T, github_copilot_sdk::Error>>,
    ) -> Result<T, Problem> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(Problem::plain(
                "The SDK background workspace is disconnected.",
            ));
        }
        let call = async {
            tokio::time::timeout(RPC_TIMEOUT, future)
                .await
                .map_err(|_| {
                    Problem::plain(format!("SDK {operation} exceeded its one-minute deadline."))
                })?
                .map_err(|error| sdk_problem(operation, error))
        };
        if let RpcAccess::Read(op) = access {
            return observe_read(&self.cwd, op, call).await;
        }
        call.await.map_err(|problem| {
            self.shutdown();
            Problem::with(
                format!("{} The SDK workspace was stopped to avoid leaving untracked background work. ACP conversations were not changed.", problem.said),
                problem.detail.unwrap_or_default(),
            )
        })
    }

    async fn tasks(&self) -> Result<Vec<ExtensionTask>, Problem> {
        let list = self
            .rpc(
                "task listing",
                RpcAccess::Read(Operation::SdkTasks),
                self.session.rpc().tasks().list(),
            )
            .await?;
        if list.tasks.len() > MAX_ROWS {
            return Err(Problem::plain("The SDK returned more than 200 background tasks; the list was not silently truncated."));
        }
        list.tasks.into_iter().map(project_task).collect()
    }

    async fn plugins(&self) -> Result<Vec<InstalledPluginInfo>, Problem> {
        let list = self
            .rpc(
                "plugin listing",
                RpcAccess::Read(Operation::SdkPlugins),
                self.client.rpc().plugins().list(),
            )
            .await?;
        if list.plugins.len() > MAX_ROWS {
            return Err(Problem::plain("The SDK returned more than 200 plugins."));
        }
        Ok(list.plugins)
    }

    async fn agents(&self) -> Result<Vec<CopilotAgent>, Problem> {
        let list = self
            .rpc(
                "agent listing",
                RpcAccess::Read(Operation::SdkAgents),
                self.session.rpc().agent().list(),
            )
            .await?;
        if list.agents.len() > MAX_ROWS {
            return Err(Problem::plain("The SDK returned more than 200 agents."));
        }
        list.agents
            .into_iter()
            .map(|agent| {
                bounded(&agent.id, 512, "agent identifier")?;
                Ok(CopilotAgent {
                    id: agent.id,
                    name: bounded(&agent.display_name, 512, "agent name")?,
                    description: bounded(&agent.description, MAX_TEXT, "agent description")?,
                    model: None,
                    definition: None,
                    source: None,
                })
            })
            .collect()
    }

    async fn snapshot(&self) -> ExtensionSnapshot {
        let snapshot = ExtensionSnapshot {
            schema_version: SCHEMA_VERSION,
            sdk_version: SDK_VERSION.into(),
            runtime_version: self.version.clone(),
            protocol_version: self.protocol,
            connection_id: self.id.clone(),
            session_id: self.session.id().to_string(),
            cwd: self.cwd.clone(),
            memory_enabled: self.memory_enabled,
            agents: feature(self.agents().await),
            tasks: feature(self.tasks().await),
            plugins: feature(
                self.plugins()
                    .await
                    .and_then(|plugins| plugins.into_iter().map(project_plugin).collect()),
            ),
            remote_sessions: self.remote.lock().unwrap().clone(),
        };
        *self.cached.lock().unwrap() = Some(snapshot.clone());
        snapshot
    }

    async fn approve<R: Runtime>(&self, app: &AppHandle<R>, details: Value) -> Result<(), Problem> {
        let app = app.clone();
        let consent = Arc::clone(&self.consent);
        let owner = self.id.clone();
        let session = self.session.id().to_string();
        let decision = tauri::async_runtime::spawn_blocking(move || {
            consent.ask(
                &app,
                &owner,
                &session,
                CopilotPermissionOrigin::Background,
                details,
            )
        })
        .await
        .map_err(|error| Problem::with("The approval operation stopped.", error.to_string()))??;
        if decision != ConsentDecision::Allow {
            return Err(Problem::plain("The SDK action was not approved."));
        }
        if self.closed.load(Ordering::SeqCst) {
            return Err(Problem::plain("The SDK workspace closed during approval."));
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct ExtensionState {
    workspace: Mutex<Option<Arc<Workspace>>>,
    opening: tauri::async_runtime::Mutex<()>,
}

impl ExtensionState {
    pub fn shutdown(&self) {
        if let Some(workspace) = self.workspace.lock().unwrap().take() {
            workspace.shutdown();
        }
    }

    pub fn status(&self) -> Option<ExtensionSnapshot> {
        let workspace = self.workspace.lock().unwrap().clone();
        match workspace {
            Some(workspace) if !workspace.closed.load(Ordering::SeqCst) => {
                workspace.cached.lock().unwrap().clone()
            }
            _ => None,
        }
    }

    fn current(&self, id: &str) -> Result<Arc<Workspace>, Problem> {
        self.workspace.lock().unwrap().as_ref()
            .filter(|workspace| workspace.id == id && !workspace.closed.load(Ordering::SeqCst))
            .cloned().ok_or_else(|| Problem::plain("That SDK workspace is no longer active. Reconnect explicitly; ACP is still available."))
    }

    async fn connect<R: Runtime>(
        &self,
        app: &AppHandle<R>,
        consent: Arc<ClientConsent>,
        cwd: String,
        memory_enabled: bool,
    ) -> Result<ExtensionSnapshot, Problem> {
        let _opening = self.opening.lock().await;
        if self
            .workspace
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|workspace| !workspace.closed.load(Ordering::SeqCst))
        {
            return Err(Problem::plain(
                "Disconnect the current SDK workspace before selecting another.",
            ));
        }
        let cwd = copilot::workspace(Some(cwd))?.cwd;
        let client = start_client(Path::new(&cwd)).await?;
        let id = format!("sdk-{:032x}", rand::random::<u128>());
        let closed = Arc::new(AtomicBool::new(false));
        let prepared = async {
            let status = observe_read(&cwd, Operation::SdkStatus, async {
                tokio::time::timeout(RPC_TIMEOUT, client.get_status()).await
                    .map_err(|_| Problem::plain("SDK status timed out."))?
                    .map_err(|error| sdk_problem("status", error))
            }).await?;
            observe_read(&cwd, Operation::SdkVerifyAuth, async {
                let auth = tokio::time::timeout(RPC_TIMEOUT, client.get_auth_status()).await
                    .map_err(|_| Problem::plain("SDK authentication status timed out."))?
                    .map_err(|error| sdk_problem("authentication status", error))?;
                if auth.is_authenticated { Ok(()) } else {
                    Err(Problem::plain("Sign in through Copilot CLI before starting an SDK workspace. Darbot does not collect its tokens."))
                }
            }).await?;
            let permissions = NativePermissions {
                app: app.clone(),
                consent: Arc::clone(&consent),
                owner: id.clone(),
                closed: Arc::clone(&closed),
            };
            let config = CreateSessionOptions::default()
                .with_working_directory(&cwd)
                .with_state(|state| state.with_memory(
                    MemoryConfiguration::disabled().with_enabled(memory_enabled),
                ))
                .with_permissions(|options| options.with_on_request(Arc::new(permissions)));
            let session = tokio::time::timeout(OPEN_TIMEOUT, client.create_session(config)).await
                .map_err(|_| Problem::plain("Opening the SDK workspace exceeded five minutes."))?
                .map_err(|error| sdk_problem("workspace creation", error))?;
            Ok(Arc::new(Workspace {
                client: client.clone(),
                session,
                id: id.clone(),
                cwd,
                memory_enabled,
                version: status.version,
                protocol: status.protocol_version,
                closed: Arc::clone(&closed),
                consent: Arc::clone(&consent),
                remote: Mutex::new(None),
                cached: Mutex::new(None),
            }))
        }.await;
        match prepared {
            Ok(workspace) => {
                *self.workspace.lock().unwrap() = Some(Arc::clone(&workspace));
                Ok(workspace.snapshot().await)
            }
            Err(problem) => {
                closed.store(true, Ordering::SeqCst);
                consent.cancel(&id, None);
                client.force_stop();
                Err(problem)
            }
        }
    }

    pub async fn execute<R: Runtime>(
        &self,
        app: &AppHandle<R>,
        consent: Arc<ClientConsent>,
        request: ExtensionRequest,
    ) -> Result<Option<ExtensionSnapshot>, Problem> {
        if request.schema_version != SCHEMA_VERSION {
            return Err(Problem::plain("Unsupported SDK extension schema version."));
        }
        if let ExtensionAction::Connect {
            cwd,
            memory_enabled,
        } = request.action
        {
            return self
                .connect(app, consent, cwd, memory_enabled)
                .await
                .map(Some);
        }
        let id = match &request.action {
            ExtensionAction::Disconnect { connection_id }
            | ExtensionAction::Refresh { connection_id }
            | ExtensionAction::StartTask { connection_id, .. }
            | ExtensionAction::CancelTask { connection_id, .. }
            | ExtensionAction::SendTaskMessage { connection_id, .. }
            | ExtensionAction::StartFleet { connection_id, .. }
            | ExtensionAction::SetPluginEnabled { connection_id, .. }
            | ExtensionAction::ListRemote { connection_id } => connection_id,
            ExtensionAction::Connect { .. } => unreachable!(),
        };
        let workspace = self.current(id)?;
        match request.action {
            ExtensionAction::Disconnect { .. } => {
                workspace.approve(app, json!({"title": "Disconnect the SDK background workspace", "workingDirectory": workspace.cwd, "effect": "Stops only this SDK workspace and its owned background tasks; ACP conversations remain connected."})).await?;
                workspace.shutdown();
                self.workspace.lock().unwrap().take();
                return Ok(None);
            }
            ExtensionAction::StartTask {
                agent_id,
                name,
                prompt,
                ..
            } => {
                let name = required(&name, 128, "task name")?;
                let prompt = required(&prompt, MAX_TEXT, "task prompt")?;
                if !workspace
                    .agents()
                    .await?
                    .iter()
                    .any(|agent| agent.id == agent_id)
                {
                    return Err(Problem::plain(
                        "That agent was not offered by this SDK workspace.",
                    ));
                }
                workspace.approve(app, json!({"title":"Start one background agent", "agent":agent_id, "name":name, "prompt":prompt, "workingDirectory":workspace.cwd, "notice":"This uses Copilot credits. Native approval remains required for tool actions."})).await?;
                workspace
                    .rpc(
                        "starting a background agent",
                        RpcAccess::Mutation,
                        workspace
                            .session
                            .rpc()
                            .tasks()
                            .start_agent(TasksStartAgentRequest {
                                agent_type: agent_id,
                                name,
                                prompt,
                                description: None,
                                model: None,
                            }),
                    )
                    .await?;
            }
            ExtensionAction::CancelTask { task_id, .. } => {
                require_task(&workspace, &task_id).await?;
                workspace
                    .approve(
                        app,
                        json!({"title":"Cancel a background task", "taskId":task_id}),
                    )
                    .await?;
                let result = workspace
                    .rpc(
                        "task cancellation",
                        RpcAccess::Mutation,
                        workspace
                            .session
                            .rpc()
                            .tasks()
                            .cancel(TasksCancelRequest { id: task_id }),
                    )
                    .await?;
                if !result.cancelled {
                    return Err(Problem::plain(
                        "The runtime did not cancel that task; refresh its current state.",
                    ));
                }
            }
            ExtensionAction::SendTaskMessage {
                task_id, message, ..
            } => {
                require_task(&workspace, &task_id).await?;
                let message = required(&message, MAX_TEXT, "task message")?;
                workspace.approve(app, json!({"title":"Send a background task another message", "taskId":task_id, "message":message})).await?;
                let result = workspace
                    .rpc(
                        "task message",
                        RpcAccess::Mutation,
                        workspace
                            .session
                            .rpc()
                            .tasks()
                            .send_message(TasksSendMessageRequest {
                                id: task_id,
                                message,
                                from_agent_id: None,
                            }),
                    )
                    .await?;
                if !result.sent {
                    return Err(Problem::with(
                        "The task message was not delivered.",
                        result
                            .error
                            .unwrap_or_else(|| "The runtime returned sent=false.".into()),
                    ));
                }
            }
            ExtensionAction::StartFleet { prompt, .. } => {
                let prompt = required(&prompt, MAX_TEXT, "fleet prompt")?;
                workspace.approve(app, json!({"title":"Start Copilot fleet orchestration", "prompt":prompt, "workingDirectory":workspace.cwd, "notice":"This can launch multiple agents and consume substantial Copilot credits. It is separate from the current ACP chat."})).await?;
                let mut request = FleetStartRequest::default();
                request.prompt = Some(prompt);
                request.wait = Some(false);
                let result = workspace
                    .rpc(
                        "fleet start",
                        RpcAccess::Mutation,
                        workspace.session.rpc().fleet().start(request),
                    )
                    .await?;
                if !result.started {
                    return Err(Problem::plain(
                        "The runtime did not start fleet orchestration.",
                    ));
                }
            }
            ExtensionAction::SetPluginEnabled {
                plugin_id, enabled, ..
            } => {
                let plugin = workspace
                    .plugins()
                    .await?
                    .into_iter()
                    .map(project_plugin)
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .find(|plugin| plugin.id == plugin_id && plugin.can_toggle)
                    .ok_or_else(|| {
                        Problem::plain("That plugin cannot be toggled through this SDK workspace.")
                    })?;
                workspace.approve(app, json!({"title": if enabled {"Enable a Copilot plugin"} else {"Disable a Copilot plugin"}, "plugin":plugin.name, "workingDirectory":workspace.cwd, "notice":"Changes CLI-owned plugin settings. Disabling can stop that plugin's MCP servers in active CLI sessions."})).await?;
                if enabled {
                    workspace
                        .rpc(
                            "plugin enable",
                            RpcAccess::Mutation,
                            workspace
                                .client
                                .rpc()
                                .plugins()
                                .enable(PluginsEnableRequest {
                                    names: vec![plugin_id.clone()],
                                    working_directory: Some(workspace.cwd.clone()),
                                }),
                        )
                        .await?;
                } else {
                    workspace
                        .rpc(
                            "plugin disable",
                            RpcAccess::Mutation,
                            workspace
                                .client
                                .rpc()
                                .plugins()
                                .disable(PluginsDisableRequest {
                                    names: vec![plugin_id.clone()],
                                    working_directory: Some(workspace.cwd.clone()),
                                }),
                        )
                        .await?;
                }
                // This RPC acknowledges even unknown names. Check the effective
                // setting rather than presenting an ignored request as success.
                let snapshot = workspace.snapshot().await;
                verify_toggle(&snapshot.plugins, &plugin_id, enabled)?;
                return Ok(Some(snapshot));
            }
            ExtensionAction::ListRemote { .. } => {
                let result =
                    workspace
                        .rpc(
                            "remote session discovery",
                            RpcAccess::Read(Operation::SdkRemote),
                            workspace.client.rpc().sessions().list_with_params(
                                SessionsListRequest {
                                    source: Some(SessionSource::Remote),
                                    throw_on_error: Some(true),
                                    metadata_limit: Some(0),
                                    include_detached: Some(false),
                                    filter: None,
                                },
                            ),
                        )
                        .await
                        .and_then(|list| {
                            if list.sessions.len() > MAX_ROWS {
                                return Err(Problem::plain(
                                    "More than 200 remote sessions were returned.",
                                ));
                            }
                            list.sessions.into_iter().map(project_remote).collect()
                        });
                *workspace.remote.lock().unwrap() = Some(feature(result));
            }
            ExtensionAction::Refresh { .. } => {}
            ExtensionAction::Connect { .. } => unreachable!(),
        }
        Ok(Some(workspace.snapshot().await))
    }
}

struct NativePermissions<R: Runtime> {
    app: AppHandle<R>,
    consent: Arc<ClientConsent>,
    owner: String,
    closed: Arc<AtomicBool>,
}

#[async_trait]
impl<R: Runtime> PermissionHandler for NativePermissions<R> {
    async fn handle(
        &self,
        session_id: SessionId,
        _request_id: RequestId,
        data: PermissionRequestData,
    ) -> PermissionResult {
        if self.closed.load(Ordering::SeqCst) {
            return PermissionResult::user_not_available();
        }
        let details = match permission_details(&data) {
            Ok(details) => details,
            Err(problem) => return PermissionResult::reject(Some(problem.said)),
        };
        let app = self.app.clone();
        let consent = Arc::clone(&self.consent);
        let owner = self.owner.clone();
        let decision = tauri::async_runtime::spawn_blocking(move || {
            consent.ask(
                &app,
                &owner,
                session_id.as_str(),
                CopilotPermissionOrigin::Background,
                details,
            )
        })
        .await;
        match decision {
            Ok(Ok(ConsentDecision::Allow)) if !self.closed.load(Ordering::SeqCst) => {
                PermissionResult::approve_once()
            }
            Ok(Ok(ConsentDecision::Reject)) => PermissionResult::reject(None),
            Ok(Ok(_)) => PermissionResult::user_not_available(),
            Ok(Err(problem)) => PermissionResult::reject(Some(problem.said)),
            Err(error) => PermissionResult::reject(Some(format!(
                "Native permission handling failed: {error}"
            ))),
        }
    }
}

// A host launch setting, never an IPC field or a path read from a plugin.
fn sdk_program(explicit: Option<PathBuf>) -> Result<PathBuf, Problem> {
    let Some(path) = explicit else {
        return Ok("copilot".into());
    };
    let name = if cfg!(windows) {
        "copilot.exe"
    } else {
        "copilot"
    };
    if !path.is_absolute()
        || path.file_name() != Some(std::ffi::OsStr::new(name))
        || !path.is_file()
    {
        return Err(Problem::plain("DARBOT_COPILOT_SDK_CLI must point to an existing absolute Copilot CLI executable. No other CLI was started."));
    }
    Ok(path)
}

pub fn agent_plugin_launch_args() -> Result<Vec<String>, Problem> {
    let paths = std::env::var_os("DARBOT_AGENT_PLUGIN_DIRS");
    plugin_launch_args(paths.as_deref())
}

fn plugin_launch_args(paths: Option<&std::ffi::OsStr>) -> Result<Vec<String>, Problem> {
    let Some(paths) = paths else {
        return Ok(Vec::new());
    };
    if paths.is_empty() || paths.to_string_lossy().len() > 32 * 4096 {
        return Err(Problem::plain(
            "DARBOT_AGENT_PLUGIN_DIRS must list bounded absolute plugin directories.",
        ));
    }
    let mut roots = std::collections::BTreeSet::new();
    let mut args = Vec::new();
    for path in std::env::split_paths(paths) {
        if !path.is_absolute() || roots.len() >= 32 {
            return Err(Problem::plain(
                "Configure at most 32 absolute Agent Plugins directories.",
            ));
        }
        let root = path.canonicalize().map_err(|error| {
            Problem::with(
                "An explicitly configured Agent Plugins directory is unavailable.",
                error.to_string(),
            )
        })?;
        if !root.is_dir() {
            return Err(Problem::plain("An Agent Plugins root must be a directory."));
        }
        if !roots.insert(root.clone()) {
            continue;
        }
        let manifest = root.join("plugin.json").canonicalize().map_err(|error| {
            Problem::with(
                "An Agent Plugins root has no readable plugin.json.",
                error.to_string(),
            )
        })?;
        if !manifest.starts_with(&root) || !manifest.is_file() {
            return Err(Problem::plain(
                "The Agent Plugins manifest must resolve to a file within its plugin root.",
            ));
        }
        let mut bytes = Vec::new();
        std::fs::File::open(&manifest)
            .and_then(|file| file.take(128 * 1024 + 1).read_to_end(&mut bytes))
            .map_err(|error| {
                Problem::with(
                    "The Agent Plugins manifest could not be read.",
                    error.to_string(),
                )
            })?;
        if bytes.len() > 128 * 1024 {
            return Err(Problem::plain(
                "The Agent Plugins manifest exceeds 128 KiB.",
            ));
        }
        validate_agent_plugin_manifest(&bytes)?;
        args.push("--plugin-dir".into());
        args.push(
            root.to_str()
                .ok_or_else(|| {
                    Problem::plain("The Agent Plugins path cannot be represented as Unicode.")
                })?
                .into(),
        );
    }
    Ok(args)
}

fn validate_agent_plugin_manifest(bytes: &[u8]) -> Result<(), Problem> {
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        Problem::with(
            "Agent Plugins plugin.json is not valid JSON.",
            error.to_string(),
        )
    })?;
    let object = value
        .as_object()
        .ok_or_else(|| Problem::plain("Agent Plugins plugin.json must be an object."))?;
    if value["$schema"] != "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json" {
        return Err(Problem::plain(
            "Darbot requires the published Agent Plugins 1.0.0 manifest schema.",
        ));
    }
    let name = value["name"]
        .as_str()
        .ok_or_else(|| Problem::plain("The Agent Plugins manifest requires a name."))?;
    let alphanumeric = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|byte| alphanumeric(byte) || byte == b'-' || byte == b'.')
        || !name.bytes().next().is_some_and(alphanumeric)
        || !name.bytes().last().is_some_and(alphanumeric)
        || name.contains("--")
        || name.contains("..")
    {
        return Err(Problem::plain(
            "The Agent Plugins manifest has an invalid portable name.",
        ));
    }
    for field in [
        "version",
        "description",
        "homepage",
        "repository",
        "license",
    ] {
        if object.get(field).is_some_and(|v| !v.is_string()) {
            return Err(Problem::plain(
                "An Agent Plugins metadata field has an invalid type.",
            ));
        }
    }
    if let Some(author) = object.get("author") {
        let author = author
            .as_object()
            .ok_or_else(|| Problem::plain("The Agent Plugins author must be an object."))?;
        if author.iter().any(|(key, value)| {
            !["name", "email", "url"].contains(&key.as_str()) || !value.is_string()
        }) {
            return Err(Problem::plain(
                "The Agent Plugins author contains invalid fields.",
            ));
        }
    }
    if let Some(keywords) = object.get("keywords") {
        if !keywords
            .as_array()
            .is_some_and(|items| items.iter().all(Value::is_string))
        {
            return Err(Problem::plain(
                "Agent Plugins keywords must be an array of strings.",
            ));
        }
    }
    const FIELDS: &[&str] = &[
        "$schema",
        "name",
        "version",
        "description",
        "author",
        "homepage",
        "repository",
        "license",
        "keywords",
        "extensions",
    ];
    for _ in object.keys().filter(|key| !FIELDS.contains(&key.as_str())) {
        eprintln!("[plugins] ignored an unknown Agent Plugins manifest field");
    }
    if object
        .get("extensions")
        .is_some_and(|value| !value.is_object())
    {
        eprintln!("[plugins] ignored non-object Agent Plugins extensions metadata");
    }
    Ok(())
}

fn client_options(cwd: &Path, program: &Path) -> Result<ClientOptions, Problem> {
    let mut args = vec!["--no-auto-update".to_owned()];
    args.extend(agent_plugin_launch_args()?);
    let process = SpawnedProcessOptions::new()
        .with_program(CliProgram::Path(program.to_owned()))
        .with_working_directory(cwd)
        .with_args(args);
    Ok(ClientOptions::new(ClientMode::CodingAgent)
        .with_runtime(RuntimeConnection::for_spawned_stdio(process))
        .with_github(|options| options.with_use_logged_in_user(true))
        .with_client_info(
            ClientInfo::new()
                .with_application_name("Darbot")
                .with_application_version(env!("CARGO_PKG_VERSION"))
                .with_integration_name("native-sdk-extensions")
                .with_integration_version(SCHEMA_VERSION.to_string()),
        ))
}

async fn start_client(cwd: &Path) -> Result<Client, Problem> {
    let program = sdk_program(std::env::var_os("DARBOT_COPILOT_SDK_CLI").map(PathBuf::from))?;
    let started = Instant::now();
    let result =
        tokio::time::timeout(RPC_TIMEOUT, Client::start(client_options(cwd, &program)?)).await;
    if let Some(cwd) = cwd.to_str() {
        record_transport(
            Some(cwd),
            Operation::SdkStart,
            started,
            result.as_ref().is_ok_and(Result::is_ok),
        );
    }
    let result =
        result.map_err(|_| Problem::plain("The Copilot CLI did not start within one minute."))?;
    match result {
        Err(error)
            if matches!(
                error.kind(),
                github_copilot_sdk::ErrorKind::Protocol(
                    github_copilot_sdk::ProtocolErrorKind::VersionMismatch { .. }
                )
            ) =>
        {
            let mut problem = sdk_problem("startup", error);
            // Inventory is still supported by the CLI itself. Never downgrade the
            // negotiated protocol, start a session, or attempt a plugin operation.
            let started = Instant::now();
            let inventory = cli_inventory(cwd, &program).await;
            if let Some(cwd) = cwd.to_str() {
                record_transport(
                    Some(cwd),
                    Operation::FallbackPlugins,
                    started,
                    inventory.is_ok(),
                );
            }
            problem.detail = Some(match inventory {
                Ok(rows) if rows.is_empty() => {
                    "CLI inventory: no installed plugins in this profile.".into()
                }
                Ok(rows) => format!(
                    "CLI inventory (not a runtime health check):\n{}",
                    rows.iter()
                        .map(|p| format!(
                            "{} · {} · {}",
                            p.name,
                            p.origin,
                            if p.enabled { "enabled" } else { "disabled" }
                        ))
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
                Err(reason) => reason.said,
            });
            Err(problem)
        }
        other => other.map_err(|error| sdk_problem("startup", error)),
    }
}

async fn cli_inventory(cwd: &Path, program: &Path) -> Result<Vec<ExtensionPlugin>, Problem> {
    use std::process::Stdio;
    use tokio::io::AsyncReadExt;
    let read = async {
        let mut command = crate::quiet::command(program);
        command
            .args(["--no-auto-update", "plugin", "list", "--json"])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .stdout(Stdio::piped());
        let mut child = tokio::process::Command::from(command)
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| {
                Problem::plain("Could not start the installed CLI for plugin inventory.")
            })?;
        let mut bytes = Vec::new();
        const LIMIT: u64 = 256 * 1024;
        child
            .stdout
            .take()
            .ok_or_else(|| Problem::plain("Missing CLI inventory output."))?
            .take(LIMIT + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| Problem::plain("Could not read CLI inventory."))?;
        if bytes.len() as u64 > LIMIT {
            return Err(Problem::plain(
                "CLI plugin inventory exceeded its size limit.",
            ));
        }
        if !child.wait().await.is_ok_and(|s| s.success()) {
            return Err(Problem::plain(
                "CLI plugin inventory failed. Check whether plugin list --json is supported.",
            ));
        }
        let rows: Vec<InstalledPluginInfo> = serde_json::from_slice(&bytes).map_err(|_| {
            Problem::plain("The CLI returned an unsupported plugin inventory format.")
        })?;
        if rows.len() > MAX_ROWS {
            return Err(Problem::plain("CLI inventory exceeds 200 plugins."));
        }
        rows.into_iter().map(project_plugin).collect()
    };
    tokio::time::timeout(Duration::from_secs(10), read)
        .await
        .map_err(|_| Problem::plain("CLI plugin inventory exceeded ten seconds."))?
}

fn permission_details(data: &PermissionRequestData) -> Result<Value, Problem> {
    let kind = data
        .kind
        .ok_or_else(|| Problem::plain("The SDK permission has no action category."))?;
    if matches!(kind, PermissionRequestKind::Unknown) {
        return Err(Problem::plain(
            "This SDK permission category is not supported by Darbot.",
        ));
    }
    validate_details(&data.extra, 0)?;
    if !data.extra.is_object() || data.extra.as_object().is_some_and(|map| map.is_empty()) {
        return Err(Problem::plain(
            "The SDK supplied no action details to review.",
        ));
    }
    let details = json!({"title":format!("Background SDK {kind:?} action"), "request":data.extra});
    if details.to_string().len() > 30 * 1024 {
        return Err(Problem::plain(
            "The SDK permission details are too large to review safely.",
        ));
    }
    Ok(details)
}

fn validate_details(value: &Value, depth: usize) -> Result<(), Problem> {
    if depth > 8 {
        return Err(Problem::plain(
            "The SDK action details exceed the nesting limit.",
        ));
    }
    match value {
        Value::Object(fields) => {
            if fields.len() > 64 {
                return Err(Problem::plain("Too many SDK action fields."));
            }
            for (key, value) in fields {
                let key: String = key
                    .chars()
                    .filter(char::is_ascii_alphanumeric)
                    .flat_map(char::to_lowercase)
                    .collect();
                if [
                    "token",
                    "accesstoken",
                    "refreshtoken",
                    "idtoken",
                    "apikey",
                    "password",
                    "clientsecret",
                    "privatekey",
                ]
                .contains(&key.as_str())
                {
                    return Err(Problem::plain(
                        "Credential-bearing SDK action details cannot be sent to the renderer.",
                    ));
                }
                validate_details(value, depth + 1)?;
            }
        }
        Value::Array(items) => {
            if items.len() > 128 {
                return Err(Problem::plain("Too many SDK action detail items."));
            }
            for item in items {
                validate_details(item, depth + 1)?;
            }
        }
        Value::String(text) if text.len() > MAX_TEXT => {
            return Err(Problem::plain(
                "An SDK action detail exceeds the text limit.",
            ))
        }
        _ => {}
    }
    Ok(())
}

fn sdk_problem(operation: &str, error: github_copilot_sdk::Error) -> Problem {
    use github_copilot_sdk::ErrorKind;
    if let ErrorKind::Protocol(github_copilot_sdk::ProtocolErrorKind::VersionMismatch {
        server,
        min,
        max,
    }) = error.kind()
    {
        return Problem::plain(format!("Copilot CLI reports protocol {server}; Darbot SDK {SDK_VERSION} supports {min}–{max}. Use a reviewed, compatible CLI/SDK pair for the background workspace. This is not a plugin or sign-in failure."));
    }
    let cause = match error.kind() {
        ErrorKind::BinaryNotFound { .. } => {
            "Copilot CLI was not found. Check the installed CLI on PATH."
        }
        ErrorKind::Rpc { code: -32601 } => {
            "The CLI does not support this SDK operation. Check CLI/SDK compatibility."
        }
        ErrorKind::Rpc { code: -32602 } => {
            "The CLI rejected the operation parameters. Check CLI/SDK compatibility."
        }
        ErrorKind::Rpc { .. } => {
            "The CLI returned an error; this does not establish a plugin failure."
        }
        ErrorKind::GitHubTokenProvider => {
            "GitHub authentication could not be obtained. Check the CLI's own sign-in status."
        }
        _ if error.is_transport_failure() => {
            "The SDK transport failed. Reconnect the background workspace."
        }
        _ => "The operation failed. Check the installed CLI and SDK versions.",
    };
    // RPC messages can include credentials, absolute paths and plugin-supplied text.
    Problem::with(
        format!("Copilot SDK {operation}: {cause}"),
        error
            .rpc_code()
            .map(|n| format!("JSON-RPC {n}"))
            .unwrap_or_default(),
    )
}

fn feature<T>(result: Result<T, Problem>) -> Feature<T> {
    match result {
        Ok(data) => Feature::Available { data },
        Err(problem) => Feature::Unavailable { problem },
    }
}

fn bounded(value: &str, limit: usize, label: &str) -> Result<String, Problem> {
    if value.len() > limit || value.contains('\0') {
        return Err(Problem::plain(format!(
            "The SDK {label} is invalid or exceeds {limit} bytes."
        )));
    }
    Ok(value.into())
}

fn required(value: &str, limit: usize, label: &str) -> Result<String, Problem> {
    if value.trim().is_empty() {
        return Err(Problem::plain(format!("A nonempty {label} is required.")));
    }
    bounded(value, limit, label)
}

fn inventory_atom(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
}

fn verify_toggle(
    plugins: &Feature<Vec<ExtensionPlugin>>,
    id: &str,
    enabled: bool,
) -> Result<(), Problem> {
    match plugins {
        Feature::Unavailable { problem } => Err(problem.clone()),
        Feature::Available { data } => match data.iter().find(|plugin| plugin.id == id) {
            Some(plugin) if plugin.enabled == enabled => Ok(()),
            Some(_) => Err(Problem::plain("The CLI did not apply that plugin setting. Check the profile and repository settings before retrying.")),
            None => Err(Problem::plain("The plugin is no longer listed in this CLI profile. Refresh its inventory.")),
        },
    }
}

fn project_plugin(plugin: InstalledPluginInfo) -> Result<ExtensionPlugin, Problem> {
    let name = plugin.name;
    let marketplace = plugin.marketplace;
    if !inventory_atom(&name)
        || (!marketplace.is_empty() && !inventory_atom(&marketplace))
        || plugin
            .direct_source_id
            .as_deref()
            .is_some_and(|id| !inventory_atom(id))
    {
        return Err(Problem::plain(
            "The CLI returned an unsupported plugin identifier. Check inventory in the CLI.",
        ));
    }
    let builtin = plugin.source.as_deref() == Some("builtin");
    let can_toggle = !marketplace.is_empty() && !builtin && plugin.direct_source_id.is_none();
    let id = if builtin {
        format!("builtin:{name}")
    } else if marketplace.is_empty() {
        format!(
            "direct:{}",
            plugin.direct_source_id.as_deref().unwrap_or(&name)
        )
    } else {
        format!("{name}@{marketplace}")
    };
    Ok(ExtensionPlugin {
        id,
        name,
        enabled: plugin.enabled,
        version: plugin.version.filter(|v| {
            v.len() <= 64
                && v.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
        }),
        can_toggle,
        origin: if builtin {
            "Built-in"
        } else if marketplace.is_empty() {
            "Direct install"
        } else {
            "Marketplace"
        },
    })
}

fn project_task(value: Value) -> Result<ExtensionTask, Problem> {
    let task: TaskMetadata = serde_json::from_value(value).map_err(|error| {
        Problem::with("The SDK returned invalid task metadata.", error.to_string())
    })?;
    let output = task.error.or(task.latest_response).or(task.result);
    let output_truncated = output.as_ref().is_some_and(|text| text.len() > MAX_TEXT);
    let output = output.map(|mut text| {
        if text.len() > MAX_TEXT {
            let mut end = MAX_TEXT;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
        text
    });
    Ok(ExtensionTask {
        id: required(&task.id, 512, "task identifier")?,
        kind: required(&task.kind, 64, "task kind")?,
        status: required(&task.status, 64, "task status")?,
        title: bounded(
            &task
                .display_name
                .or(task.description)
                .unwrap_or_else(|| task.id.clone()),
            1024,
            "task title",
        )?,
        output,
        output_truncated,
    })
}

fn project_remote(value: Value) -> Result<RemoteSession, Problem> {
    let session: RemoteSessionMetadataValue = serde_json::from_value(value).map_err(|error| {
        Problem::with(
            "The SDK returned invalid remote session metadata.",
            error.to_string(),
        )
    })?;
    if !session.is_remote {
        return Err(Problem::plain(
            "A local session was returned in the remote-only listing.",
        ));
    }
    Ok(RemoteSession {
        id: required(
            session.session_id.as_str(),
            512,
            "remote session identifier",
        )?,
        title: bounded(
            &session
                .name
                .or(session.summary)
                .unwrap_or_else(|| session.session_id.to_string()),
            1024,
            "remote session title",
        )?,
        modified_at: bounded(&session.modified_time, 128, "remote modification time")?,
        state: session.state,
    })
}

async fn require_task(workspace: &Workspace, id: &str) -> Result<(), Problem> {
    required(id, 512, "task identifier")?;
    if !workspace.tasks().await?.iter().any(|task| task.id == id) {
        return Err(Problem::plain(
            "That task does not belong to the current SDK workspace.",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an installed Copilot CLI; isolated profile and fixture plugin only"]
    fn installed_cli_loads_portable_plugin_skill_from_explicit_directory() {
        let root = crate::test_support::temp_root("portable-plugin-cli");
        let plugin = root.join("plugin");
        let home = root.join("profile");
        std::fs::create_dir_all(plugin.join("skills").join("local-proof")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(plugin.join("plugin.json"),
            br#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"darbot-conformance"}"#).unwrap();
        std::fs::write(plugin.join("skills").join("local-proof").join("SKILL.md"),
            "---\nname: local-proof\ndescription: Report a local plugin loading marker without executing tools.\n---\nReturn the text DARBOT_PLUGIN_LOADED.\n").unwrap();
        let output = crate::quiet::command("copilot")
            .args(["--no-auto-update", "--log-level", "none"])
            .args(plugin_launch_args(Some(plugin.as_os_str())).unwrap())
            .args(["skill", "list", "--json"])
            .env("COPILOT_HOME", &home)
            .current_dir(&root)
            .output();
        std::fs::remove_dir_all(&root).unwrap();
        let output = output.expect("Installed Copilot CLI must start");
        assert!(
            output.status.success(),
            "CLI plugin enumeration failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output: Value =
            serde_json::from_slice(&output.stdout).expect("CLI must return actual JSON inventory");
        assert!(
            output.to_string().contains("local-proof"),
            "CLI did not load the explicit portable skill"
        );
    }

    #[test]
    fn portable_plugin_manifest_enforces_v1_without_fetching_schema_or_executing_components() {
        let valid = json!({
            "$schema": "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json",
            "name": "review.tools"
        });
        assert!(validate_agent_plugin_manifest(&serde_json::to_vec(&valid).unwrap()).is_ok());
        for name in ["", "Upper", "../escape", "-prefix", "end.", "a--b", "a..b"] {
            let mut manifest = valid.clone();
            manifest["name"] = json!(name);
            assert!(
                validate_agent_plugin_manifest(&serde_json::to_vec(&manifest).unwrap()).is_err()
            );
        }
        for (field, value) in [
            ("$schema", json!("https://example.com/schema")),
            ("description", json!(false)),
            ("keywords", json!(["valid", 42])),
            ("author", json!({"unknown": "field"})),
        ] {
            let mut manifest = valid.clone();
            manifest[field] = value;
            assert!(
                validate_agent_plugin_manifest(&serde_json::to_vec(&manifest).unwrap()).is_err()
            );
        }
        let mut ignored = valid;
        ignored["unknown"] = json!({"instructions": "must not execute"});
        ignored["extensions"] = json!("invalid extensions are report-and-ignore");
        assert!(validate_agent_plugin_manifest(&serde_json::to_vec(&ignored).unwrap()).is_ok());
    }

    #[test]
    fn portable_plugin_directories_produce_exact_cli_tokens_and_never_implicit_fallbacks() {
        assert!(plugin_launch_args(None).unwrap().is_empty());
        assert!(plugin_launch_args(Some(std::ffi::OsStr::new(""))).is_err());
        assert!(plugin_launch_args(Some(std::ffi::OsStr::new("relative-plugin"))).is_err());
        let root = crate::test_support::temp_root("portable-plugin-roots");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("plugin.json"),
            br#"{
              "$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json",
              "name":"native-review"
        }"#,
        )
        .unwrap();
        let configured = std::env::join_paths([&root, &root]).unwrap();
        let expected = vec![
            "--plugin-dir".to_string(),
            root.canonicalize().unwrap().to_str().unwrap().into(),
        ];
        assert_eq!(plugin_launch_args(Some(&configured)).unwrap(), expected);
        std::fs::write(root.join("plugin.json"), b"{\"name\":\"missing-version\"}").unwrap();
        assert!(plugin_launch_args(Some(root.as_os_str())).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn extension_contract_refuses_unknown_operations_and_credential_fields() {
        assert!(serde_json::from_value::<ExtensionRequest>(
            json!({"schemaVersion":1,"action":{"kind":"shell","command":"anything"}})
        )
        .is_err());
        assert!(serde_json::from_value::<ExtensionRequest>(json!({"schemaVersion":1,"action":{"kind":"connect","cwd":"C:\\project","memoryEnabled":false,"apiKey":"not-a-real-key"}})).is_err());
        let request: ExtensionRequest = serde_json::from_value(json!({"schemaVersion":1,"action":{"kind":"connect","cwd":"C:\\project","memoryEnabled":false}})).unwrap();
        assert_eq!(request.schema_version, SCHEMA_VERSION);
        assert!(
            validate_details(&json!({"nested":{"accessToken":"not-a-real-token"}}), 0).is_err()
        );
    }

    #[test]
    fn explicit_installed_stdio_runtime_keeps_authentication_and_remote_export_cli_owned() {
        let default_program = sdk_program(None).unwrap();
        assert!(sdk_program(Some("copilot".into())).is_err());
        assert!(sdk_program(Some(std::env::current_exe().unwrap())).is_err());
        let options = client_options(Path::new("C:\\project"), &default_program).unwrap();
        let Some(RuntimeConnection::SpawnedStdio(process)) = options.runtime else {
            panic!("SDK runtime must use the explicitly installed stdio process");
        };
        assert!(matches!(process.program, CliProgram::Path(ref path) if path == &default_program));
        assert_eq!(process.working_directory, Path::new("C:\\project"));
        assert!(options.github.token.is_none());
        assert_eq!(options.github.use_logged_in_user, Some(true));
        assert_eq!(options.mode, ClientMode::CodingAgent);
        assert_eq!(process.args, [std::ffi::OsString::from("--no-auto-update")]);
        const { assert!(!github_copilot_sdk::HAS_BUNDLED_CLI) };
    }

    #[test]
    fn task_projection_bounds_output_and_does_not_copy_extra_metadata() {
        let task = project_task(json!({"id":"task-one","type":"agent","status":"completed","displayName":"One task","latestResponse":"x".repeat(MAX_TEXT+5),"accessToken":"must-not-leave-native"})).unwrap();
        assert!(task.output_truncated);
        assert_eq!(task.output.as_ref().unwrap().len(), MAX_TEXT);
        assert!(!serde_json::to_string(&task)
            .unwrap()
            .contains("must-not-leave-native"));
        assert!(project_task(json!({"type":"agent"})).is_err());
    }

    #[test]
    fn plugin_inventory_preserves_origin_without_exposing_manifest_paths() {
        let project = |value| project_plugin(serde_json::from_value(value).unwrap());
        let builtin = project(
            json!({"name":"browser-use","marketplace":"","enabled":false,"source":"builtin"}),
        )
        .unwrap();
        assert_eq!(builtin.id, "builtin:browser-use");
        assert!(!builtin.can_toggle);
        let direct = project(
            json!({"name":"browser-use","marketplace":"","enabled":true,"directSourceId":"abcd"}),
        )
        .unwrap();
        assert_ne!(direct.id, builtin.id);
        assert!(!direct.can_toggle);
        let market = project(json!({"name":"tool","marketplace":"local","enabled":false,"installedFrom":"private/path","version":"https://private/path"})).unwrap();
        assert!(market.version.is_none() && market.can_toggle);
        assert!(!serde_json::to_string(&market).unwrap().contains("private"));
        let rows = Feature::Available { data: vec![market] };
        assert!(verify_toggle(&rows, "tool@local", false).is_ok());
        assert!(verify_toggle(&rows, "tool@local", true).is_err());
        assert!(verify_toggle(&rows, "missing@local", false).is_err());
        assert!(project(json!({"name":"bad\nname","marketplace":"local","enabled":true})).is_err());
    }

    #[test]
    fn sdk_unsupported_errors_keep_only_the_code() {
        let error = github_copilot_sdk::Error::with_message(
            github_copilot_sdk::ErrorKind::Rpc { code: -32601 },
            "private",
        );
        let problem = sdk_problem("plugin listing", error);
        assert!(problem.said.contains("does not support"));
        assert_eq!(problem.detail.as_deref(), Some("JSON-RPC -32601"));
    }

    #[test]
    #[ignore = "Requires the installed real Copilot CLI; reads status only, never starts a session or changes authentication."]
    fn installed_sdk_transport_handshake() {
        tauri::async_runtime::block_on(async {
            let cwd = std::env::current_dir().unwrap();
            let client = start_client(&cwd).await.expect("installed CLI SDK startup");
            let result = tokio::time::timeout(RPC_TIMEOUT, async {
                let status = client
                    .get_status()
                    .await
                    .map_err(|e| sdk_problem("status", e))?;
                let auth = client
                    .get_auth_status()
                    .await
                    .map_err(|e| sdk_problem("auth status", e))?;
                let plugins = client
                    .rpc()
                    .plugins()
                    .list()
                    .await
                    .map_err(|e| sdk_problem("plugin listing", e))?;
                Ok::<_, Problem>((status, auth.is_authenticated, plugins.plugins.len()))
            })
            .await;
            client.force_stop();
            let (status, authenticated, plugins) = result
                .expect("bounded SDK status")
                .expect("real SDK status");
            assert!(!status.version.is_empty());
            assert!(inventory_atom(&status.version));
            assert!(status.protocol_version >= 3);
            println!(
                "Installed CLI SDK handshake: version {}, protocol {}, authenticated {authenticated}, {plugins} inventory rows.",
                status.version, status.protocol_version
            );
        });
    }
}
