//! Opt-in, loopback-only desktop control. Authority is defined here, not by renderer schemas.
//! REST and MCP both enter `dispatch`; neither accepts JavaScript, selectors or IPC command names.
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{header, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;
use tauri::{Emitter, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tokio::sync::{oneshot, Semaphore};
use tokio_util::sync::CancellationToken;

mod capture;

const PROTOCOL: &str = "2025-06-18";
const MAX_BODY: usize = 48 * 1024;
const MAX_RESULT: usize = 256 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONNECTIONS: usize = 4;

fn random_id() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlError {
    code: &'static str,
    message: String,
}

fn error(code: &'static str, message: impl Into<String>) -> ControlError {
    ControlError {
        code,
        message: message.into(),
    }
}

type Result<T> = std::result::Result<T, ControlError>;

impl IntoResponse for ControlError {
    fn into_response(self) -> Response {
        let status = match self.code {
            "unauthorized" => StatusCode::UNAUTHORIZED,
            "forbidden" | "denied" => StatusCode::FORBIDDEN,
            "busy" | "rate_limit" => StatusCode::TOO_MANY_REQUESTS,
            "timeout" => StatusCode::GATEWAY_TIMEOUT,
            "stale" | "duplicate" | "unavailable" | "cancelled" => StatusCode::CONFLICT,
            _ => StatusCode::BAD_REQUEST,
        };
        (status, Json(json!({"error": self}))).into_response()
    }
}

/// Only these operations exist. Security/permission/credential controls are intentionally absent.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "action",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Action {
    Snapshot {},
    Screenshot {},
    #[serde(rename = "workspace_select_surface")]
    WorkspaceSurface {
        surface: Surface,
    },
    WorkspaceOpenDialog {
        dialog: Dialog,
    },
    WorkspaceCloseDialog {},
    WorkspaceEditDraft {
        text: String,
    },
    #[serde(rename = "workspace_new_chat")]
    WorkspaceSelectAgent {
        agent_id: String,
    },
    #[serde(rename = "workspace_open_chat")]
    WorkspaceSelectConversation {
        conversation_id: String,
    },
    #[serde(rename = "workspace_send_prompt")]
    WorkspaceSend {
        text: String,
        agent_id: String,
    },
    #[serde(rename = "workspace_cancel_prompt")]
    WorkspaceCancel {},
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Surface {
    Workspace,
    Canvas,
    Cli,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Dialog {
    Settings,
    Resources,
    Conversations,
}

impl Action {
    fn validate(&self) -> Result<()> {
        match self {
            Self::WorkspaceEditDraft { text } if text.len() > 32_000 => Err(error(
                "invalid_arguments",
                "Draft is limited to 32000 UTF-8 bytes",
            )),
            Self::WorkspaceSend { text, agent_id } => {
                // Native approval shows every byte, never a truncated authorization.
                if text.is_empty()
                    || text.trim_start().starts_with('/')
                    || text.chars().count() > 2000
                    || text.lines().count() > 24
                    || text
                        .chars()
                        .any(|c| c.is_control() && c != '\n' && c != '\t')
                    || agent_id.len() > 256
                    || agent_id.chars().any(char::is_control)
                {
                    return Err(error("invalid_arguments", "Send must be plain text (no CLI slash commands), fit native review (2000 characters, 24 lines), with a bounded agent ID"));
                }
                Ok(())
            }
            Self::WorkspaceSelectAgent { agent_id } if agent_id.len() > 256 => {
                Err(error("invalid_arguments", "Agent ID is too long"))
            }
            Self::WorkspaceSelectConversation { conversation_id }
                if conversation_id.len() > 256 =>
            {
                Err(error("invalid_arguments", "Conversation ID is too long"))
            }
            _ => Ok(()),
        }
    }

    fn protected(&self) -> bool {
        matches!(
            self,
            Self::WorkspaceSend { .. }
                | Self::WorkspaceCancel { .. }
                | Self::WorkspaceSelectAgent { .. }
                | Self::WorkspaceSelectConversation { .. }
        )
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Expected {
    pub instance_id: String,
    pub revision: u64,
    #[serde(deserialize_with = "required_nullable")]
    pub conversation_id: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub session_id: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub active_turn_id: Option<String>,
}

fn required_nullable<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Operation {
    pub request_id: String,
    pub expected: Option<Expected>,
    pub operation: Action,
}

impl Operation {
    fn validate(&self) -> Result<()> {
        if self.request_id.is_empty()
            || self.request_id.len() > 80
            || !self
                .request_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return Err(error(
                "invalid_arguments",
                "requestId must be 1–80 ASCII letters, digits or hyphens",
            ));
        }
        if !matches!(self.operation, Action::Snapshot { .. }) && self.expected.is_none() {
            return Err(error(
                "invalid_arguments",
                "An exact snapshot identity is required",
            ));
        }
        if let Some(expected) = &self.expected {
            if expected.instance_id.len() > 80
                || expected
                    .conversation_id
                    .as_ref()
                    .is_some_and(|s| s.len() > 256)
                || expected.session_id.as_ref().is_some_and(|s| s.len() > 256)
                || expected.active_turn_id.as_ref().is_some_and(|s| {
                    s.is_empty()
                        || s.len() > 80
                        || !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                })
            {
                return Err(error("invalid_arguments", "Invalid target identity"));
            }
            if matches!(self.operation, Action::WorkspaceCancel {})
                && (expected.active_turn_id.is_none()
                    || expected.session_id.as_ref().is_none_or(|s| s.is_empty())
                    || expected
                        .conversation_id
                        .as_ref()
                        .is_none_or(|s| s.is_empty()))
            {
                return Err(error(
                    "invalid_arguments",
                    "Cancellation requires an active turn, session and conversation identity",
                ));
            }
        }
        self.operation.validate()
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Envelope {
    ticket: String,
    expires_at: u64,
    request: Operation,
}

struct Pending {
    connection: String,
    rpc_id: Option<Value>,
    expires: Instant,
    claimed: bool,
    operation: Operation,
    executed: bool,
    response: oneshot::Sender<Result<Value>>,
}

struct Connection {
    initialized: bool,
    mcp: bool,
    expires: Instant,
    // At-most-once IDs are retained for the connection's bounded lifetime.
    seen: HashSet<String>,
}

struct Service {
    host: Arc<dyn ControlHost>,
    secret: String,
    authority: String,
    discovery: PathBuf,
    stop: CancellationToken,
    connections: Mutex<HashMap<String, Connection>>,
    pending: Mutex<HashMap<String, Pending>>,
    serial: Semaphore,
    observations: Semaphore,
    approvals: Semaphore,
    rate: Mutex<(Instant, usize)>,
    renderer_seen: Mutex<Instant>,
}

#[async_trait::async_trait]
trait ControlHost: Send + Sync {
    fn request(&self, envelope: Envelope) -> Result<()>;
    fn cancel(&self, ticket: &str, session: Option<String>);
    async fn approve(&self, message: String) -> Result<()>;
    async fn capture(&self) -> Result<Value>;
}

struct NativeHost(tauri::AppHandle);

#[async_trait::async_trait]
impl ControlHost for NativeHost {
    fn request(&self, envelope: Envelope) -> Result<()> {
        self.0
            .emit_to("main", "app-control:request", envelope)
            .map_err(|_| error("unavailable", "Renderer could not be reached"))
    }

    fn cancel(&self, ticket: &str, session: Option<String>) {
        let _ = self.0.emit_to("main", "app-control:cancel", ticket);
        if let Some(session_id) = session {
            if let Some(runtime) = self
                .0
                .try_state::<Arc<crate::copilot::CopilotRuntimeState>>()
            {
                let runtime = Arc::clone(&runtime);
                let app = self.0.clone();
                let ticket = ticket.to_owned();
                tauri::async_runtime::spawn_blocking(move || {
                    if runtime.cancel_controlled(&session_id, &ticket).is_err() {
                        // Do not leak SDK errors or conversation content to logs.
                        let _ = app.emit_to("main", "app-control:problem", "The expired app-control turn could not be cancelled. Check the conversation.");
                    }
                });
            }
        }
    }

    async fn approve(&self, message: String) -> Result<()> {
        approve(&self.0, message).await
    }

    async fn capture(&self) -> Result<Value> {
        let window = self
            .0
            .get_webview_window("main")
            .ok_or_else(|| error("unavailable", "Window detached"))?;
        tauri::async_runtime::spawn_blocking(move || capture::app_window(&window))
            .await
            .map_err(|_| error("unavailable", "App capture failed"))?
    }
}

#[derive(Default)]
pub struct AppControl(Mutex<Option<Arc<Service>>>, AtomicU64, Arc<AtomicBool>);

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceStatus {
    enabled: bool,
    endpoint: Option<String>,
    discovery_file: Option<String>,
}

impl AppControl {
    pub fn shutdown(&self) {
        self.1.fetch_add(1, Ordering::SeqCst);
        if let Some(service) = self.0.lock().unwrap().take() {
            service.stop.cancel();
            service.connections.lock().unwrap().clear();
            let tickets: Vec<_> = service.pending.lock().unwrap().drain().collect();
            for (ticket, pending) in tickets {
                cancel_pending(&service, ticket, pending);
            }
            let _ = std::fs::remove_file(&service.discovery);
        }
    }

    fn service(&self) -> Result<Arc<Service>> {
        self.0
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| error("unavailable", "App control is disabled"))
    }

    /// Called inside the real ACP worker, immediately before submitting a controlled prompt.
    /// A renderer cannot reuse a navigation ticket or change the approved content/recipient.
    pub fn authorize_prompt(
        &self,
        ticket: &str,
        agent: &str,
        session: &str,
        text: &str,
    ) -> Result<()> {
        let service = self.service()?;
        let mut pending = service.pending.lock().unwrap();
        let p = pending
            .get_mut(ticket)
            .ok_or_else(|| error("cancelled", "Prompt authorization expired"))?;
        let matches = match &p.operation.operation {
            Action::WorkspaceSend {
                text: approved,
                agent_id,
            } => {
                approved == text
                    && agent_id == agent
                    && p.operation
                        .expected
                        .as_ref()
                        .and_then(|e| e.session_id.as_deref())
                        == Some(session)
            }
            _ => false,
        };
        if !matches
            || !p.claimed
            || p.executed
            || p.expires <= Instant::now()
            || service.stop.is_cancelled()
        {
            return Err(error(
                "forbidden",
                "Exact prompt authorization is no longer valid",
            ));
        }
        p.executed = true;
        Ok(())
    }
}

#[tauri::command]
pub fn app_control_status(state: tauri::State<'_, AppControl>) -> ServiceStatus {
    let service = state.0.lock().unwrap();
    ServiceStatus {
        enabled: service.is_some(),
        endpoint: service
            .as_ref()
            .map(|s| format!("http://{}/mcp", s.authority)),
        discovery_file: service.as_ref().map(|s| s.discovery.display().to_string()),
    }
}

fn main_window(window: &tauri::WebviewWindow) -> Result<()> {
    if window.label() != "main" {
        return Err(error(
            "forbidden",
            "Only the local main window may control this service",
        ));
    }
    // Tauri capabilities already constrain IPC origins. Reject remote navigation as a second boundary.
    let url = window
        .url()
        .map_err(|_| error("forbidden", "Unknown window origin"))?;
    let trusted = matches!(
        (url.scheme(), url.host_str()),
        ("tauri", Some("localhost")) | ("http" | "https", Some("tauri.localhost"))
    ) || (cfg!(debug_assertions)
        && window
            .app_handle()
            .config()
            .build
            .dev_url
            .as_ref()
            .is_some_and(|dev| dev.origin() == url.origin()));
    if !trusted {
        return Err(error(
            "forbidden",
            "App control requires the local desktop UI",
        ));
    }
    Ok(())
}

async fn approve(app: &tauri::AppHandle, message: String) -> Result<()> {
    let gate = app.state::<AppControl>().2.clone();
    gate.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .map_err(|_| error("busy", "Close the previous native approval dialog first"))?;
    struct DialogGuard(Arc<AtomicBool>);
    impl Drop for DialogGuard {
        fn drop(&mut self) {
            self.0.store(false, Ordering::SeqCst);
        }
    }
    let guard = DialogGuard(gate);
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        // The guard belongs to the actual dialog lifetime, including after caller timeout.
        let _guard = guard;
        let window = app
            .get_webview_window("main")
            .ok_or_else(|| error("unavailable", "Window is closed"))?;
        window
            .show()
            .map_err(|_| error("unavailable", "Window cannot be shown"))?;
        window
            .set_focus()
            .map_err(|_| error("unavailable", "Window cannot be focused"))?;
        if app
            .dialog()
            .message(message)
            .title("Darbot app control — owner approval")
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom(
                "Allow once".into(),
                "Deny".into(),
            ))
            .parent(&window)
            .blocking_show()
        {
            Ok(())
        } else {
            Err(error("denied", "The local owner denied this request"))
        }
    })
    .await
    .map_err(|_| error("unavailable", "Owner approval failed"))?
}

#[tauri::command]
pub async fn app_control_start(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppControl>,
) -> Result<ServiceStatus> {
    main_window(&window)?;
    if state.0.lock().unwrap().is_some() {
        return Ok(app_control_status(state));
    }
    let generation = state.1.fetch_add(1, Ordering::SeqCst) + 1;
    approve(&app, "Enable local app control for this app run?\n\nA random credential will be saved in an owner-only file. Local clients with this credential must also obtain your native connection approval. Approved connections may read bounded conversation text and drafts, navigate this app and request app-window screenshots. Sending and changing sessions ask separately. Passwords, authentication, permissions and host access cannot be controlled. Stop in Settings to revoke all connections.".into()).await?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|_| error("unavailable", "Could not bind loopback listener"))?;
    let authority = listener
        .local_addr()
        .map_err(|_| error("unavailable", "Listener unavailable"))?
        .to_string();
    let directory = app
        .path()
        .app_local_data_dir()
        .map_err(|_| error("unavailable", "Private application directory unavailable"))?
        .join("app-control");
    capture::private_directory(&directory)?;
    let discovery = directory.join("connection.json");
    let service = Arc::new(Service {
        host: Arc::new(NativeHost(app.clone())),
        secret: random_id(),
        authority,
        discovery,
        stop: CancellationToken::new(),
        connections: Mutex::new(HashMap::new()),
        pending: Mutex::new(HashMap::new()),
        serial: Semaphore::new(1),
        observations: Semaphore::new(4),
        approvals: Semaphore::new(1),
        rate: Mutex::new((Instant::now(), 0)),
        renderer_seen: Mutex::new(Instant::now()),
    });
    {
        let mut current = state.0.lock().unwrap();
        if state.1.load(Ordering::SeqCst) != generation {
            return Err(error("cancelled", "Enablement was superseded or revoked"));
        }
        if current.is_some() {
            return Err(error("busy", "Another start completed first"));
        }
        let data = json!({"version":1,"pid":std::process::id(),"endpoint":format!("http://{}/mcp", service.authority),
            "rest":format!("http://{}/v1", service.authority),"bearerToken":service.secret,"protocolVersion":PROTOCOL});
        capture::write_discovery(&service.discovery, &serde_json::to_vec(&data).unwrap())?;
        *current = Some(service.clone());
    }
    let router = router(service.clone());
    tauri::async_runtime::spawn(async move {
        let serving = serve(listener, router, service.stop.clone());
        tokio::pin!(serving);
        loop {
            tokio::select! {
                _ = &mut serving => break,
                _ = tokio::time::sleep(Duration::from_secs(5)) => {
                    if service.renderer_seen.lock().unwrap().elapsed() > Duration::from_secs(20) {
                        break;
                    }
                }
            }
        }
        service.stop.cancel();
        let state = app.state::<AppControl>();
        let mut current = state.0.lock().unwrap();
        if current.as_ref().is_some_and(|s| Arc::ptr_eq(s, &service)) {
            current.take();
            let _ = std::fs::remove_file(&service.discovery);
        }
    });
    Ok(app_control_status(state))
}

async fn serve(listener: tokio::net::TcpListener, router: Router, stop: CancellationToken) {
    let connections = Arc::new(Semaphore::new(16));
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            _ = stop.cancelled() => break,
        };
        let Ok((stream, address)) = accepted else {
            break;
        };
        if !address.ip().is_loopback() {
            continue;
        }
        let Ok(permit) = connections.clone().try_acquire_owned() else {
            continue;
        };
        let router = router.clone();
        let stop = stop.clone();
        tauri::async_runtime::spawn(async move {
            let _permit = permit;
            let io = hyper_util::rt::TokioIo::new(stream);
            let service = hyper_util::service::TowerToHyperService::new(router);
            let mut http = hyper::server::conn::http1::Builder::new();
            http.timer(hyper_util::rt::TokioTimer::new())
                .header_read_timeout(Duration::from_secs(5))
                .max_buf_size(64 * 1024);
            tokio::select! {
                _ = tokio::time::timeout(Duration::from_secs(300), http.serve_connection(io, service)) => {},
                _ = stop.cancelled() => {},
            }
        });
    }
}

#[tauri::command]
pub fn app_control_stop(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppControl>,
) -> Result<()> {
    main_window(&window)?;
    state.shutdown();
    Ok(())
}

#[tauri::command]
pub fn app_control_heartbeat(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppControl>,
) -> Result<()> {
    main_window(&window)?;
    if let Some(service) = state.0.lock().unwrap().as_ref() {
        *service.renderer_seen.lock().unwrap() = Instant::now();
    }
    Ok(())
}

/// The renderer must claim immediately before acting. A timed-out/revoked ticket cannot be claimed.
#[tauri::command]
pub fn app_control_claim(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppControl>,
    ticket: String,
) -> Result<()> {
    main_window(&window)?;
    let service = state.service()?;
    claim_ticket(&service, &ticket)
}

fn claim_ticket(service: &Service, ticket: &str) -> Result<()> {
    let mut pending = service.pending.lock().unwrap();
    let entry = pending
        .get_mut(ticket)
        .ok_or_else(|| error("cancelled", "Request is no longer live"))?;
    if service.stop.is_cancelled() || entry.expires <= Instant::now() || entry.claimed {
        return Err(error("cancelled", "Request expired or was already claimed"));
    }
    entry.claimed = true;
    Ok(())
}

#[tauri::command]
pub fn app_control_ack(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppControl>,
    ticket: String,
    value: Option<Value>,
    failure: Option<String>,
) -> Result<()> {
    main_window(&window)?;
    let service = state.service()?;
    ack_ticket(&service, ticket, value, failure)
}

fn ack_ticket(
    service: &Service,
    ticket: String,
    value: Option<Value>,
    failure: Option<String>,
) -> Result<()> {
    let mut pending = service.pending.lock().unwrap();
    let entry = pending
        .remove(&ticket)
        .ok_or_else(|| error("cancelled", "Request is no longer live"))?;
    if !entry.claimed || entry.expires <= Instant::now() || service.stop.is_cancelled() {
        cancel_pending(service, ticket, entry);
        return Err(error("cancelled", "Request expired or was not claimed"));
    }
    let result = if let Some(message) = failure {
        Err(error(
            "renderer_rejected",
            message.chars().take(240).collect::<String>(),
        ))
    } else {
        value
            .ok_or_else(|| error("invalid_result", "Renderer did not acknowledge a result"))
            .and_then(|v| {
                if serde_json::to_vec(&v).map_or(true, |v| v.len() > MAX_RESULT) {
                    Err(error("invalid_result", "Renderer response exceeded bounds"))
                } else {
                    Ok(v)
                }
            })
    };
    entry
        .response
        .send(result)
        .map_err(|_| error("cancelled", "Caller disconnected"))
}

fn router(service: Arc<Service>) -> Router {
    Router::new()
        .route("/v1/connect", post(rest_connect))
        .route("/v1/disconnect", post(rest_disconnect))
        .route("/v1/dispatch", post(rest_dispatch))
        .route("/v1/capabilities", get(capabilities))
        .route("/openapi.json", get(openapi))
        .route("/mcp", post(mcp).delete(mcp_disconnect))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(middleware::from_fn_with_state(service.clone(), guard))
        .with_state(service)
}

fn valid_headers(headers: &HeaderMap, authority: &str, secret: &str) -> Result<()> {
    for name in [
        "host",
        "authorization",
        "origin",
        "mcp-session-id",
        "x-darbot-connection",
    ] {
        if headers.get_all(name).iter().count() > 1 {
            return Err(error("forbidden", "Ambiguous security headers"));
        }
    }
    if headers.get(header::HOST).and_then(|v| v.to_str().ok()) != Some(authority) {
        return Err(error("forbidden", "Host must match the loopback endpoint"));
    }
    // Non-browser MCP/REST clients omit Origin. Browser callers must be exactly same-origin.
    if let Some(origin) = headers.get(header::ORIGIN) {
        if origin.to_str().ok() != Some(format!("http://{authority}").as_str()) {
            return Err(error("forbidden", "Origin is not allowed"));
        }
    }
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("");
    if bearer.len() != secret.len() || !bool::from(bearer.as_bytes().ct_eq(secret.as_bytes())) {
        return Err(error(
            "unauthorized",
            "Valid bearer authentication is required",
        ));
    }
    Ok(())
}

async fn guard(
    State(service): State<Arc<Service>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if let Err(e) = valid_headers(request.headers(), &service.authority, &service.secret) {
        return e.into_response();
    }
    if service.stop.is_cancelled() {
        return error("unavailable", "Service revoked").into_response();
    }
    {
        let mut rate = service.rate.lock().unwrap();
        if rate.0.elapsed() >= Duration::from_secs(60) {
            *rate = (Instant::now(), 0);
        }
        rate.1 += 1;
        if rate.1 > 120 {
            return error(
                "rate_limit",
                "Maximum 120 authenticated requests per minute",
            )
            .into_response();
        }
    }
    let mut response =
        match tokio::time::timeout(TIMEOUT + Duration::from_secs(2), next.run(request)).await {
            Ok(response) => response,
            Err(_) => error(
                "timeout",
                "Request expired; no pending action remains authorized",
            )
            .into_response(),
        };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert("x-content-type-options", "nosniff".parse().unwrap());
    response
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Connect {
    name: String,
}

async fn connect(service: &Arc<Service>, name: &str, initialized: bool) -> Result<String> {
    if name.is_empty() || name.len() > 100 || name.chars().any(char::is_control) {
        return Err(error(
            "invalid_arguments",
            "Client name must be 1–100 printable UTF-8 bytes",
        ));
    }
    let _approval = service
        .approvals
        .try_acquire()
        .map_err(|_| error("busy", "Another owner approval is pending"))?;
    {
        let mut connections = service.connections.lock().unwrap();
        connections.retain(|_, c| c.expires > Instant::now());
        if connections.len() >= MAX_CONNECTIONS {
            return Err(error("busy", "Maximum four connections"));
        }
    }
    tokio::select! {
        result = tokio::time::timeout(TIMEOUT, service.host.approve(format!(
            "Local client (self-reported name): {name}\n\nAllow this connection for 30 minutes?\nIt may read bounded app state, conversation text and drafts, navigate local surfaces, edit drafts and capture this app window on demand. Sending/cancelling and changing agent/session require separate native approval. It cannot grant permissions or access credentials.\n\nOnly approve a client you just started."
        ))) => result.map_err(|_| error("timeout", "Connection approval expired"))??,
        _ = service.stop.cancelled() => return Err(error("cancelled", "Service revoked")),
    }
    if service.stop.is_cancelled() {
        return Err(error("cancelled", "Service revoked"));
    }
    let id = random_id();
    service.connections.lock().unwrap().insert(
        id.clone(),
        Connection {
            initialized,
            mcp: !initialized,
            expires: Instant::now() + Duration::from_secs(1800),
            seen: HashSet::new(),
        },
    );
    Ok(id)
}

fn connection(
    service: &Service,
    headers: &HeaderMap,
    mcp: bool,
    require_ready: bool,
) -> Result<String> {
    let header = if mcp {
        "mcp-session-id"
    } else {
        "x-darbot-connection"
    };
    let id = headers
        .get(header)
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| {
            error(
                "unauthorized",
                "Connect and obtain owner authorization first",
            )
        })?;
    let connections = service.connections.lock().unwrap();
    let c = connections
        .get(id)
        .ok_or_else(|| error("unauthorized", "Unknown connection"))?;
    if c.mcp != mcp {
        return Err(error(
            "unauthorized",
            "Connection belongs to a different transport",
        ));
    }
    if c.expires <= Instant::now() || service.stop.is_cancelled() {
        return Err(error("unauthorized", "Connection expired or revoked"));
    }
    if require_ready && !c.initialized {
        return Err(error(
            "invalid_request",
            "Send notifications/initialized first",
        ));
    }
    Ok(id.into())
}

async fn rest_connect(
    State(s): State<Arc<Service>>,
    Json(c): Json<Connect>,
) -> Result<Json<Value>> {
    Ok(Json(
        json!({"connectionId":connect(&s, &c.name, true).await?, "expiresInSeconds":1800}),
    ))
}

fn disconnect(service: &Service, id: &str) {
    service.connections.lock().unwrap().remove(id);
    let mut pending = service.pending.lock().unwrap();
    let tickets: Vec<_> = pending
        .iter()
        .filter(|(_, p)| p.connection == id)
        .map(|(t, _)| t.clone())
        .collect();
    for ticket in tickets {
        if let Some(p) = pending.remove(&ticket) {
            cancel_pending(service, ticket, p);
        }
    }
}

async fn rest_disconnect(State(s): State<Arc<Service>>, h: HeaderMap) -> Result<StatusCode> {
    disconnect(&s, &connection(&s, &h, false, false)?);
    Ok(StatusCode::NO_CONTENT)
}
async fn mcp_disconnect(State(s): State<Arc<Service>>, h: HeaderMap) -> Result<StatusCode> {
    if h.get("mcp-protocol-version").and_then(|v| v.to_str().ok()) != Some(PROTOCOL) {
        return Err(error(
            "invalid_request",
            "Unsupported or missing MCP-Protocol-Version",
        ));
    }
    disconnect(&s, &connection(&s, &h, true, false)?);
    Ok(StatusCode::NO_CONTENT)
}
async fn rest_dispatch(
    State(s): State<Arc<Service>>,
    h: HeaderMap,
    Json(op): Json<Operation>,
) -> Result<Json<Value>> {
    let id = connection(&s, &h, false, true)?;
    dispatch(&s, id, op, None).await.map(Json)
}

struct PendingGuard {
    service: Arc<Service>,
    ticket: String,
}
fn cancel_pending(service: &Service, ticket: String, pending: Pending) {
    let session = if pending.executed {
        pending.operation.expected.and_then(|e| e.session_id)
    } else {
        None
    };
    service.host.cancel(&ticket, session);
    let _ = pending.response.send(Err(error(
        "cancelled",
        "Request expired, disconnected or was revoked",
    )));
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Some(pending) = self.service.pending.lock().unwrap().remove(&self.ticket) {
            cancel_pending(&self.service, self.ticket.clone(), pending);
        }
    }
}

async fn dispatch(
    service: &Arc<Service>,
    id: String,
    op: Operation,
    rpc_id: Option<Value>,
) -> Result<Value> {
    op.validate()?;
    // Observations and explicit cancellation must remain usable while a controlled turn runs.
    let gate = if matches!(
        op.operation,
        Action::Snapshot { .. } | Action::WorkspaceCancel { .. }
    ) {
        &service.observations
    } else {
        &service.serial
    };
    let _permit = gate
        .try_acquire()
        .map_err(|_| error("busy", "Another app operation is in progress"))?;
    let deadline = {
        let mut connections = service.connections.lock().unwrap();
        let c = connections
            .get_mut(&id)
            .ok_or_else(|| error("unauthorized", "Connection revoked"))?;
        if c.expires <= Instant::now() {
            return Err(error("unauthorized", "Connection expired"));
        }
        if c.seen.len() >= 512 {
            return Err(error("rate_limit", "Reconnect after 512 operations"));
        }
        if !c.seen.insert(op.request_id.clone()) {
            return Err(error(
                "duplicate",
                "Request ID already used; observe state before retrying",
            ));
        }
        (Instant::now() + TIMEOUT).min(c.expires)
    };
    let screenshot = matches!(op.operation, Action::Screenshot { .. });
    let result = roundtrip(service, &id, op.clone(), rpc_id.clone(), deadline).await?;
    if screenshot {
        let _approval = service
            .approvals
            .try_acquire()
            .map_err(|_| error("busy", "Native approval is active"))?;
        if result.get("captureAllowed") != Some(&Value::Bool(true)) {
            return Err(error(
                "forbidden",
                "Capture unavailable on this protected surface",
            ));
        }
        let image = tokio::time::timeout_at(deadline.into(), service.host.capture())
            .await
            .map_err(|_| error("timeout", "App capture expired"))??;
        // Never return pixels if state/protected surfaces changed while native capture was running.
        // This is a second typed renderer acknowledgment, not a stale cached capture permission.
        let verified = roundtrip(service, &id, op, rpc_id, deadline).await?;
        if verified.get("captureAllowed") != Some(&Value::Bool(true))
            || verified.pointer("/state/revision") != result.pointer("/state/revision")
        {
            return Err(error(
                "stale",
                "App changed during capture; image was discarded",
            ));
        }
        return Ok(
            json!({"requestId":result["requestId"],"outcome":"captured","state":result["state"],"image":image}),
        );
    }
    Ok(result)
}

async fn roundtrip(
    service: &Arc<Service>,
    id: &str,
    op: Operation,
    rpc_id: Option<Value>,
    deadline: Instant,
) -> Result<Value> {
    let (tx, mut rx) = oneshot::channel();
    let ticket = random_id();
    service.pending.lock().unwrap().insert(
        ticket.clone(),
        Pending {
            connection: id.into(),
            rpc_id,
            expires: deadline,
            claimed: false,
            response: tx,
            operation: op.clone(),
            executed: false,
        },
    );
    let _guard = PendingGuard {
        service: service.clone(),
        ticket: ticket.clone(),
    };
    if op.operation.protected() {
        let _approval = service
            .approvals
            .try_acquire()
            .map_err(|_| error("busy", "Owner approval already pending"))?;
        let details = serde_json::to_string_pretty(&op)
            .map_err(|_| error("invalid_arguments", "Unreviewable action"))?;
        tokio::select! {
            result = tokio::time::timeout_at(deadline.into(), service.host.approve(format!("Allow exactly this app action once?\n\n{details}\n\nThe target must still match after approval: cancellation requires the same active turn; other actions require the same revision. Approval does not extend the request deadline."))) =>
                result.map_err(|_| error("timeout", "Action approval expired"))??,
            _ = service.stop.cancelled() => return Err(error("cancelled", "Service revoked")),
            _ = &mut rx => return Err(error("cancelled", "Client disconnected or cancelled during approval")),
        }
    }
    if deadline <= Instant::now()
        || service.stop.is_cancelled()
        || service
            .connections
            .lock()
            .unwrap()
            .get(id)
            .is_none_or(|c| c.expires <= Instant::now())
    {
        return Err(error("cancelled", "Request is no longer authorized"));
    }
    let expires_at = now_ms()
        + deadline
            .saturating_duration_since(Instant::now())
            .as_millis() as u64;
    service.host.request(Envelope {
        ticket,
        expires_at,
        request: op,
    })?;
    let result = tokio::select! {
        result = tokio::time::timeout_at(deadline.into(), rx) =>
            result.map_err(|_| error("timeout", "Renderer did not complete before deadline"))?
                .map_err(|_| error("cancelled", "Renderer disconnected"))??,
        _ = service.stop.cancelled() => return Err(error("cancelled", "Service revoked")),
    };
    Ok(result)
}

fn operation_schema() -> Value {
    let string = json!({"type":"string","maxLength":256});
    let variants = [
        ("snapshot", json!({}), vec![]),
        ("screenshot", json!({}), vec![]),
        (
            "workspace_select_surface",
            json!({"surface":{"type":"string","enum":["workspace","canvas","cli"]}}),
            vec!["surface"],
        ),
        (
            "workspace_open_dialog",
            json!({"dialog":{"type":"string","enum":["settings","resources","conversations"]}}),
            vec!["dialog"],
        ),
        ("workspace_close_dialog", json!({}), vec![]),
        (
            "workspace_edit_draft",
            json!({"text":{"type":"string","maxLength":32000}}),
            vec!["text"],
        ),
        (
            "workspace_new_chat",
            json!({"agentId":string}),
            vec!["agentId"],
        ),
        (
            "workspace_open_chat",
            json!({"conversationId":string}),
            vec!["conversationId"],
        ),
        (
            "workspace_send_prompt",
            json!({"text":{"type":"string","minLength":1,"maxLength":2000},"agentId":string}),
            vec!["text", "agentId"],
        ),
        ("workspace_cancel_prompt", json!({}), vec![]),
    ];
    let variants: Vec<_> = variants.into_iter().map(|(name, mut properties, mut required)| {
        properties["action"] = json!({"const":name,"type":"string"});
        required.push("action");
        json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
    }).collect();
    json!({"type":"object","additionalProperties":false,"required":["requestId","operation"],
    "properties":{
        "requestId":{"type":"string","pattern":"^[A-Za-z0-9-]{1,80}$"},
        "expected":{"type":["object","null"],"additionalProperties":false,
            "required":["instanceId","revision","conversationId","sessionId","activeTurnId"],
            "properties":{"instanceId":{"type":"string","maxLength":80},"revision":{"type":"integer","minimum":0},
                "conversationId":{"type":["string","null"],"maxLength":256},"sessionId":{"type":["string","null"],"maxLength":256},
                "activeTurnId":{"type":["string","null"],"minLength":1,"maxLength":80,"pattern":"^[A-Za-z0-9-]+$"}}},
        "operation":{"oneOf":variants}
    }})
}

fn tools() -> Value {
    json!([{"name":"darbot_ui_dispatch","title":"Control the authorized Darbot app",
        "description":"Real acknowledged app operations. Start with snapshot. Cancellation requires the same instance, conversation, session and non-null activeTurnId, ignoring streaming revision changes. Other operations require the exact snapshot identity including revision. Send/cancel/agent/session selection require native owner consent. No permission, auth, filesystem or arbitrary code operations. A send requires an already-open session and reports completion, not a queued success. Timeout includes owner approval and cancels pending operations; observe state before retrying.",
        "inputSchema":operation_schema(),
        "annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true}}])
}

async fn capabilities(State(s): State<Arc<Service>>, h: HeaderMap) -> Result<Json<Value>> {
    connection(&s, &h, false, true)?;
    Ok(Json(
        json!({"protocolVersion":PROTOCOL,"tools":tools(),"maxSnapshotBytes":MAX_RESULT,"maxScreenshotPngBytes":2097152,"maxRequestBytes":MAX_BODY,
        "operationTimeoutSeconds":30,"screenshot":"on-demand app window only; protected surfaces refused",
        "humanOnly":["permissions","credentials","authentication","service enablement","capture/connection consent"],
        "unsupported":["arbitrary selectors","JavaScript","host access","background orchestration","session configuration","other semantic declarations"]}),
    ))
}

async fn openapi() -> Json<Value> {
    Json(
        json!({"openapi":"3.1.0","info":{"title":"Darbot local app control","version":"1"},
        "security":[{"bearerAuth":[]}],
        "components":{"securitySchemes":{"bearerAuth":{"type":"http","scheme":"bearer"},
            "connection":{"type":"apiKey","in":"header","name":"X-Darbot-Connection"}},
            "schemas":{"Operation":operation_schema()}},
        "paths":{
            "/v1/connect":{"post":{"summary":"Request native owner authorization for a 30-minute connection",
                "requestBody":{"required":true,"content":{"application/json":{"schema":{"type":"object","additionalProperties":false,"required":["name"],"properties":{"name":{"type":"string","minLength":1,"maxLength":100}}}}}},
                "responses":{"200":{"description":"connectionId; use X-Darbot-Connection"},"403":{"description":"Owner denied"},"504":{"description":"Approval timed out"}}}},
            "/v1/dispatch":{"post":{"summary":"Execute and acknowledge a typed operation; unique requestId, no retries after timeout without observing state",
                "security":[{"bearerAuth":[],"connection":[]}],"requestBody":{"required":true,"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Operation"}}}},
                "responses":{"200":{"description":"Acknowledged requestId, outcome and bounded committed state; cancel_requested means ACP notification sent, not stopped; screenshot includes base64 PNG"},
                    "400":{"description":"Invalid arguments or renderer rejection"},"409":{"description":"Stale/duplicate/unavailable"},
                    "429":{"description":"Bounded concurrency/rate exceeded"},"504":{"description":"Expired; late requests cannot execute"}}}},
            "/v1/capabilities":{"get":{"security":[{"bearerAuth":[],"connection":[]}],"responses":{"200":{"description":"Executed operations only"}}}},
            "/v1/disconnect":{"post":{"security":[{"bearerAuth":[],"connection":[]}],"responses":{"204":{"description":"Connection and pending actions revoked"}}}},
            "/mcp":{"post":{"summary":"MCP Streamable HTTP, JSON response mode, initialize then notifications/initialized; use Mcp-Session-Id and MCP-Protocol-Version",
                "responses":{"200":{"description":"JSON-RPC response"},"202":{"description":"Accepted notification"}}},"delete":{"responses":{"204":{"description":"Revoke MCP session"}}}}
        }}),
    )
}

fn rpc_error(id: Value, code: i64, message: &str) -> Response {
    Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rpc {
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

async fn mcp(State(s): State<Arc<Service>>, h: HeaderMap, body: Bytes) -> Response {
    let accept = h
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !accept.contains("application/json") || !accept.contains("text/event-stream") {
        return (
            StatusCode::NOT_ACCEPTABLE,
            "Accept must include application/json and text/event-stream",
        )
            .into_response();
    }
    if h.get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_none_or(|v| v.split(';').next() != Some("application/json"))
    {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    let value: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return rpc_error(Value::Null, -32700, "Parse error"),
    };
    if value.get("id").is_some_and(Value::is_null) {
        return rpc_error(Value::Null, -32600, "MCP request ID must not be null");
    }
    let request: Rpc = match serde_json::from_value(value) {
        Ok(r) => r,
        Err(_) => return rpc_error(Value::Null, -32600, "Invalid Request"),
    };
    let id = request.id.clone().unwrap_or(Value::Null);
    if request.jsonrpc != "2.0"
        || request
            .id
            .as_ref()
            .is_some_and(|v| !v.is_string() && !v.is_number())
    {
        return rpc_error(Value::Null, -32600, "Invalid Request");
    }
    if request.method == "initialize" {
        if request.id.is_none() {
            return StatusCode::ACCEPTED.into_response();
        }
        if h.contains_key("mcp-session-id") {
            return rpc_error(id, -32600, "Already initialized");
        }
        let name = request
            .params
            .pointer("/clientInfo/name")
            .and_then(Value::as_str);
        if request
            .params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .is_none()
            || !request
                .params
                .get("capabilities")
                .is_some_and(Value::is_object)
            || name.is_none()
        {
            return rpc_error(id, -32602, "Invalid initialize parameters");
        }
        let session = match connect(&s, name.unwrap(), false).await {
            Ok(v) => v,
            Err(e) => return e.into_response(),
        };
        let mut response = Json(json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":PROTOCOL,
            "capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"darbot-desktop-control","version":"1"},
            "instructions":"App-only control. Native owner approvals cannot be automated. Start with snapshot; require exact target/revision for mutations."}})).into_response();
        response
            .headers_mut()
            .insert("mcp-session-id", session.parse().unwrap());
        return response;
    }
    if h.get("mcp-protocol-version").and_then(|v| v.to_str().ok()) != Some(PROTOCOL) {
        return (
            StatusCode::BAD_REQUEST,
            "Unsupported or missing MCP-Protocol-Version",
        )
            .into_response();
    }
    let session = match connection(&s, &h, true, false) {
        Ok(id) => id,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    if request.id.is_none() {
        match request.method.as_str() {
            "notifications/initialized" => {
                if let Some(c) = s.connections.lock().unwrap().get_mut(&session) {
                    c.initialized = true;
                }
            }
            "notifications/cancelled" => {
                let request_id = request.params.get("requestId");
                let mut pending = s.pending.lock().unwrap();
                let ticket = pending
                    .iter()
                    .find(|(_, p)| p.connection == session && p.rpc_id.as_ref() == request_id)
                    .map(|(t, _)| t.clone());
                if let Some(ticket) = ticket {
                    if let Some(p) = pending.remove(&ticket) {
                        cancel_pending(&s, ticket, p);
                    }
                }
            }
            _ => {}
        }
        return StatusCode::ACCEPTED.into_response();
    }
    if let Err(e) = connection(&s, &h, true, true) {
        return rpc_error(id, -32600, &e.message);
    }
    let result = match request.method.as_str() {
        "ping" => json!({}),
        "tools/list" => {
            if !request.params.is_null()
                && !request.params.as_object().is_some_and(|p| {
                    p.iter()
                        .all(|(key, value)| key == "_meta" && value.is_object())
                })
            {
                return rpc_error(id, -32602, "No cursor or list parameters supported");
            }
            json!({"tools":tools()})
        }
        "tools/call" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Call {
                name: String,
                arguments: Operation,
                #[serde(default, rename = "_meta")]
                _meta: Option<serde_json::Map<String, Value>>,
            }
            let call: Call = match serde_json::from_value(request.params) {
                Ok(c) => c,
                Err(_) => return rpc_error(id, -32602, "Invalid tool arguments"),
            };
            if call.name != "darbot_ui_dispatch" {
                return rpc_error(id, -32602, "Unknown tool");
            }
            match dispatch(&s, session, call.arguments, Some(id.clone())).await {
                Ok(value) => {
                    let mut content = vec![
                        json!({"type":"text","text":serde_json::to_string(&json!({"requestId":value["requestId"],"outcome":value["outcome"],"state":value["state"]})).unwrap()}),
                    ];
                    if let Some(image) = value.get("image") {
                        content.push(
                            json!({"type":"image","mimeType":"image/png","data":image["data"]}),
                        );
                    }
                    json!({"content":content,"isError":false})
                }
                Err(e) => {
                    json!({"content":[{"type":"text","text":serde_json::to_string(&e).unwrap()}],"isError":true})
                }
            }
        }
        _ => return rpc_error(id, -32601, "Method not found"),
    };
    Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Weak;

    struct FixtureHost {
        service: Mutex<Weak<Service>>,
        requests: Mutex<Vec<Envelope>>,
        cancelled: Mutex<Vec<String>>,
        auto_ack: AtomicBool,
        allow: AtomicBool,
        approval_delay_ms: AtomicU64,
        approvals: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl ControlHost for FixtureHost {
        fn request(&self, envelope: Envelope) -> Result<()> {
            self.requests.lock().unwrap().push(envelope.clone());
            if self.auto_ack.load(Ordering::SeqCst) {
                let service = self.service.lock().unwrap().upgrade().unwrap();
                claim_ticket(&service, &envelope.ticket)?;
                ack_ticket(
                    &service,
                    envelope.ticket,
                    Some(json!({
                        "requestId":envelope.request.request_id,"captureAllowed":false,
                        "state":{"instanceId":"owned-fixture","revision":1,"surface":"workspace"}
                    })),
                    None,
                )?;
            }
            Ok(())
        }
        fn cancel(&self, ticket: &str, _session: Option<String>) {
            self.cancelled.lock().unwrap().push(ticket.into());
        }
        async fn approve(&self, message: String) -> Result<()> {
            self.approvals.lock().unwrap().push(message);
            tokio::time::sleep(Duration::from_millis(
                self.approval_delay_ms.load(Ordering::SeqCst),
            ))
            .await;
            if self.allow.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(error("denied", "Fixture owner refused"))
            }
        }
        async fn capture(&self) -> Result<Value> {
            Err(error(
                "unavailable",
                "Fixture has no native window; never fabricate an image",
            ))
        }
    }

    struct Fixture {
        service: Arc<Service>,
        host: Arc<FixtureHost>,
        server: tokio::task::JoinHandle<()>,
        client: reqwest::Client,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.service.stop.cancel();
            self.server.abort();
        }
    }

    impl Fixture {
        async fn new() -> Self {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            let host = Arc::new(FixtureHost {
                service: Mutex::new(Weak::new()),
                requests: Mutex::new(Vec::new()),
                cancelled: Mutex::new(Vec::new()),
                auto_ack: AtomicBool::new(true),
                allow: AtomicBool::new(true),
                approval_delay_ms: AtomicU64::new(0),
                approvals: Mutex::new(Vec::new()),
            });
            let service = Arc::new(Service {
                host: host.clone(),
                secret: random_id(),
                authority: listener.local_addr().unwrap().to_string(),
                discovery: PathBuf::new(),
                stop: CancellationToken::new(),
                connections: Mutex::new(HashMap::new()),
                pending: Mutex::new(HashMap::new()),
                serial: Semaphore::new(1),
                observations: Semaphore::new(4),
                approvals: Semaphore::new(1),
                rate: Mutex::new((Instant::now(), 0)),
                renderer_seen: Mutex::new(Instant::now()),
            });
            *host.service.lock().unwrap() = Arc::downgrade(&service);
            let server = tokio::spawn(serve(
                listener,
                router(service.clone()),
                service.stop.clone(),
            ));
            let client = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(3))
                .build()
                .unwrap();
            Self {
                service,
                host,
                server,
                client,
            }
        }
        fn url(&self, route: &str) -> String {
            format!("http://{}{route}", self.service.authority)
        }
        fn post(&self, route: &str) -> reqwest::RequestBuilder {
            self.client
                .post(self.url(route))
                .bearer_auth(&self.service.secret)
        }
        fn mcp(&self, session: Option<&str>, body: Value) -> reqwest::RequestBuilder {
            let request = self
                .post("/mcp")
                .header("Accept", "application/json, text/event-stream")
                .header("MCP-Protocol-Version", PROTOCOL)
                .json(&body);
            match session {
                Some(session) => request.header("Mcp-Session-Id", session),
                None => request,
            }
        }
        async fn connect(&self) -> String {
            self.post("/v1/connect")
                .json(&json!({"name":"Owned test fixture"}))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()["connectionId"]
                .as_str()
                .unwrap()
                .into()
        }
    }

    fn snapshot(id: &str) -> Operation {
        Operation {
            request_id: id.into(),
            expected: None,
            operation: Action::Snapshot {},
        }
    }

    fn cancel_turn(id: &str) -> Operation {
        Operation {
            request_id: id.into(),
            expected: Some(Expected {
                instance_id: "owned-instance".into(),
                revision: 1,
                conversation_id: Some("owned-chat".into()),
                session_id: Some("owned-session".into()),
                active_turn_id: Some("owned-turn".into()),
            }),
            operation: Action::WorkspaceCancel {},
        }
    }

    #[tokio::test]
    async fn cancellation_identity_roundtrips_over_rest_and_mcp_and_rejects_open_shapes() {
        let f = Fixture::new().await;
        let rest = f.connect().await;
        let initialized = f.mcp(None, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":PROTOCOL,"capabilities":{},"clientInfo":{"name":"owned-fixture","version":"1"}}}))
            .send().await.unwrap();
        let mcp = initialized.headers()["mcp-session-id"]
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(
            f.mcp(
                Some(&mcp),
                json!({"jsonrpc":"2.0","method":"notifications/initialized"})
            )
            .send()
            .await
            .unwrap()
            .status(),
            202
        );
        let schema = operation_schema();
        assert!(schema["properties"]["expected"]["required"]
            .as_array()
            .unwrap()
            .contains(&json!("activeTurnId")));
        assert_eq!(
            schema["properties"]["expected"]["additionalProperties"],
            false
        );
        assert_eq!(
            f.post("/v1/dispatch")
                .header("X-Darbot-Connection", &rest)
                .json(&cancel_turn("rest-cancel"))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        let response = f
            .mcp(
                Some(&mcp),
                json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"darbot_ui_dispatch","arguments":cancel_turn("mcp-cancel")}}),
            )
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(response["result"]["isError"], false);
        {
            let requests = f.host.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            for request in requests.iter() {
                assert_eq!(
                    request
                        .request
                        .expected
                        .as_ref()
                        .unwrap()
                        .active_turn_id
                        .as_deref(),
                    Some("owned-turn")
                );
            }
        }
        assert_eq!(
            f.host
                .approvals
                .lock()
                .unwrap()
                .iter()
                .filter(|message| message.contains("Allow exactly this app action once?"))
                .count(),
            2
        );

        let valid = serde_json::to_value(cancel_turn("bad-cancel")).unwrap();
        let mut invalid = Vec::new();
        let mut missing = valid.clone();
        missing["expected"]
            .as_object_mut()
            .unwrap()
            .remove("activeTurnId");
        invalid.push(missing);
        for value in [
            Value::Null,
            json!(""),
            json!(42),
            json!("x".repeat(81)),
            json!("turn/other"),
        ] {
            let mut op = valid.clone();
            op["expected"]["activeTurnId"] = value;
            invalid.push(op);
        }
        for field in ["sessionId", "conversationId"] {
            let mut op = valid.clone();
            op["expected"][field] = Value::Null;
            invalid.push(op);
        }
        let mut extra = valid.clone();
        extra["expected"]["ignoreRevision"] = json!(true);
        invalid.push(extra);
        let mut extra_action = valid;
        extra_action["operation"]["activeTurnId"] = json!("owned-turn");
        invalid.push(extra_action);
        for (index, op) in invalid.into_iter().enumerate() {
            let response = f
                .post("/v1/dispatch")
                .header("X-Darbot-Connection", &rest)
                .json(&op)
                .send()
                .await
                .unwrap();
            assert!(
                response.status().is_client_error(),
                "REST accepted invalid identity {index}"
            );
            let response = f
                .mcp(
                    Some(&mcp),
                    json!({"jsonrpc":"2.0","id":index + 3,"method":"tools/call",
                "params":{"name":"darbot_ui_dispatch","arguments":op}}),
                )
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap();
            assert!(
                response["error"]["code"] == -32602 || response["result"]["isError"] == true,
                "MCP accepted invalid identity {index}: {response}"
            );
        }
        assert_eq!(f.host.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn owner_approval_never_extends_the_cancellation_lease() {
        let f = Fixture::new().await;
        let id = f.connect().await;
        f.host.approval_delay_ms.store(50, Ordering::SeqCst);
        let result = roundtrip(
            &f.service,
            &id,
            cancel_turn("approval-expired"),
            None,
            Instant::now() + Duration::from_millis(10),
        )
        .await;
        assert_eq!(result.unwrap_err().code, "timeout");
        assert!(f.host.requests.lock().unwrap().is_empty());
        assert!(f.service.pending.lock().unwrap().is_empty());

        f.host.approval_delay_ms.store(20, Ordering::SeqCst);
        f.host.auto_ack.store(false, Ordering::SeqCst);
        let started = now_ms();
        let result = roundtrip(
            &f.service,
            &id,
            cancel_turn("approval-near-expiry"),
            None,
            Instant::now() + Duration::from_millis(100),
        )
        .await;
        assert_eq!(result.unwrap_err().code, "timeout");
        let envelope = f.host.requests.lock().unwrap()[0].clone();
        assert!(
            envelope.expires_at <= started + 105,
            "Approval renewed the lease"
        );
        assert!(claim_ticket(&f.service, &envelope.ticket).is_err());
        assert!(f.service.pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn native_prompt_authorization_is_exact_single_use_and_revocable() {
        let f = Fixture::new().await;
        let state = AppControl::default();
        *state.0.lock().unwrap() = Some(f.service.clone());
        let (response, _receiver) = oneshot::channel();
        f.service.pending.lock().unwrap().insert(
            "fixture-ticket".into(),
            Pending {
                connection: "fixture-connection".into(),
                rpc_id: None,
                expires: Instant::now() + TIMEOUT,
                claimed: true,
                executed: false,
                response,
                operation: Operation {
                    request_id: "fixture".into(),
                    expected: Some(Expected {
                        instance_id: "owned-instance".into(),
                        revision: 1,
                        session_id: Some("owned-session".into()),
                        conversation_id: Some("owned-chat".into()),
                        active_turn_id: None,
                    }),
                    operation: Action::WorkspaceSend {
                        text: "Never sent by this test".into(),
                        agent_id: "owned-agent".into(),
                    },
                },
            },
        );
        assert!(state
            .authorize_prompt(
                "fixture-ticket",
                "other-agent",
                "owned-session",
                "Never sent by this test"
            )
            .is_err());
        assert!(state
            .authorize_prompt(
                "fixture-ticket",
                "owned-agent",
                "other-session",
                "Never sent by this test"
            )
            .is_err());
        assert!(state
            .authorize_prompt(
                "fixture-ticket",
                "owned-agent",
                "owned-session",
                "Different content"
            )
            .is_err());
        assert!(state
            .authorize_prompt(
                "fixture-ticket",
                "owned-agent",
                "owned-session",
                "Never sent by this test"
            )
            .is_ok());
        assert!(state
            .authorize_prompt(
                "fixture-ticket",
                "owned-agent",
                "owned-session",
                "Never sent by this test"
            )
            .is_err());
        state.shutdown();
        assert!(state
            .authorize_prompt(
                "fixture-ticket",
                "owned-agent",
                "owned-session",
                "Never sent by this test"
            )
            .is_err());
        assert_eq!(f.host.cancelled.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn live_http_denies_unauthenticated_cross_origin_and_rebound_requests() {
        let f = Fixture::new().await;
        assert_eq!(
            f.client
                .get(f.url("/openapi.json"))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        assert_eq!(
            f.post("/v1/connect")
                .header("Origin", "https://untrusted.example")
                .json(&json!({"name":"fixture"}))
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        assert_eq!(
            f.post("/v1/connect")
                .header("Host", "untrusted.example")
                .json(&json!({"name":"fixture"}))
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        assert_eq!(
            f.post("/v1/dispatch")
                .json(&snapshot("no-connection"))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        let contract = f
            .client
            .get(f.url("/openapi.json"))
            .bearer_auth(&f.service.secret)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(contract.contains("3.1.0"));
        assert!(!contract.contains(&f.service.secret));
        let response = f
            .post("/v1/connect")
            .header("Content-Type", "application/json")
            .body("x".repeat(MAX_BODY + 1))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 413);
    }

    #[tokio::test]
    async fn live_rest_waits_for_real_ack_and_refuses_duplicate_and_revoked_requests() {
        let f = Fixture::new().await;
        let connection = f.connect().await;
        assert_eq!(
            f.mcp(
                Some(&connection),
                json!({"jsonrpc":"2.0","id":1,"method":"ping"})
            )
            .send()
            .await
            .unwrap()
            .status(),
            404
        );
        let response = f
            .post("/v1/dispatch")
            .header("X-Darbot-Connection", &connection)
            .json(&snapshot("rest-1"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.json::<Value>().await.unwrap()["state"]["instanceId"],
            "owned-fixture"
        );
        assert_eq!(f.host.requests.lock().unwrap().len(), 1);
        assert_eq!(
            f.post("/v1/dispatch")
                .header("X-Darbot-Connection", &connection)
                .json(&snapshot("rest-1"))
                .send()
                .await
                .unwrap()
                .status(),
            409
        );
        assert_eq!(
            f.post("/v1/disconnect")
                .header("X-Darbot-Connection", &connection)
                .send()
                .await
                .unwrap()
                .status(),
            204
        );
        assert_eq!(
            f.post("/v1/dispatch")
                .header("X-Darbot-Connection", &connection)
                .json(&snapshot("rest-2"))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
    }

    #[tokio::test]
    async fn live_mcp_handshake_and_tool_call_use_the_same_dispatcher() {
        let f = Fixture::new().await;
        let initialized = f.mcp(None, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":PROTOCOL,"capabilities":{},"clientInfo":{"name":"owned-fixture","version":"1"}}}))
            .send().await.unwrap();
        assert_eq!(initialized.status(), 200);
        let session = initialized.headers()["mcp-session-id"]
            .to_str()
            .unwrap()
            .to_string();
        let response = initialized.json::<Value>().await.unwrap();
        assert_eq!(response["result"]["protocolVersion"], PROTOCOL);
        let early = f
            .mcp(
                Some(&session),
                json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            )
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert!(early.get("error").is_some());
        assert_eq!(
            f.mcp(
                Some(&session),
                json!({"jsonrpc":"2.0","method":"notifications/initialized"})
            )
            .send()
            .await
            .unwrap()
            .status(),
            202
        );
        let tools = f
            .mcp(
                Some(&session),
                json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}),
            )
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 1);
        let result = f
            .mcp(
                Some(&session),
                json!({"jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"darbot_ui_dispatch","arguments":snapshot("mcp-1"),"_meta":{"progressToken":"fixture-progress"}}}),
            )
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(result["result"]["isError"], false);
        assert_eq!(f.host.requests.lock().unwrap().len(), 1);
        let bad = f.mcp(Some(&session), json!({"jsonrpc":"2.0","id":5,"method":"tools/call",
            "params":{"name":"darbot_ui_dispatch","arguments":{"requestId":"bad","operation":{"action":"workspace_permission_decide"}}}}))
            .send().await.unwrap().json::<Value>().await.unwrap();
        assert_eq!(bad["error"]["code"], -32602);
        assert_eq!(
            f.client
                .delete(f.url("/mcp"))
                .bearer_auth(&f.service.secret)
                .header("Mcp-Session-Id", &session)
                .header("MCP-Protocol-Version", PROTOCOL)
                .send()
                .await
                .unwrap()
                .status(),
            204
        );
        assert_eq!(
            f.mcp(
                Some(&session),
                json!({"jsonrpc":"2.0","id":6,"method":"ping"})
            )
            .send()
            .await
            .unwrap()
            .status(),
            404
        );
    }

    #[tokio::test]
    async fn expiration_and_disconnect_remove_claim_authority_and_cancel_waiters() {
        let f = Fixture::new().await;
        let id = f.connect().await;
        {
            let _mutation = f.service.serial.acquire().await.unwrap();
            assert!(
                dispatch(&f.service, id.clone(), snapshot("observe-while-busy"), None)
                    .await
                    .is_ok()
            );
        }
        f.host.requests.lock().unwrap().clear();
        f.host.auto_ack.store(false, Ordering::SeqCst);
        let result = roundtrip(
            &f.service,
            &id,
            snapshot("expires"),
            None,
            Instant::now() + Duration::from_millis(10),
        )
        .await;
        assert_eq!(result.unwrap_err().code, "timeout");
        let ticket = f.host.requests.lock().unwrap()[0].ticket.clone();
        assert!(claim_ticket(&f.service, &ticket).is_err());
        assert!(f.host.cancelled.lock().unwrap().contains(&ticket));
        let service = f.service.clone();
        let request_id = id.clone();
        let pending = tokio::spawn(async move {
            dispatch(&service, request_id, snapshot("disconnects"), None).await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while f.host.requests.lock().unwrap().len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        disconnect(&f.service, &id);
        assert_eq!(pending.await.unwrap().unwrap_err().code, "cancelled");
        assert!(f.service.pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn protected_actions_require_native_owner_approval_before_renderer_dispatch() {
        let f = Fixture::new().await;
        let id = f.connect().await;
        f.host.allow.store(false, Ordering::SeqCst);
        let result = dispatch(
            &f.service,
            id,
            Operation {
                request_id: "refused-send".into(),
                expected: Some(Expected {
                    instance_id: "owned-fixture".into(),
                    revision: 1,
                    conversation_id: Some("owned-chat".into()),
                    session_id: Some("owned-session".into()),
                    active_turn_id: None,
                }),
                operation: Action::WorkspaceSend {
                    text: "Fixture only — never sent".into(),
                    agent_id: "owned-agent".into(),
                },
            },
            None,
        )
        .await;
        assert_eq!(result.unwrap_err().code, "denied");
        assert!(f.host.requests.lock().unwrap().is_empty());
        assert!(f.service.pending.lock().unwrap().is_empty());
    }

    #[test]
    fn authorization_rejects_rebinding_cross_origin_and_bad_secrets() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, "127.0.0.1:1234".parse().unwrap());
        h.insert(header::AUTHORIZATION, "Bearer synthetic".parse().unwrap());
        assert!(valid_headers(&h, "127.0.0.1:1234", "synthetic").is_ok());
        assert!(valid_headers(&h, "127.0.0.1:1234", "different").is_err());
        h.insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        assert!(valid_headers(&h, "127.0.0.1:1234", "synthetic").is_err());
        h.remove(header::ORIGIN);
        h.insert(header::HOST, "evil.example:1234".parse().unwrap());
        assert!(valid_headers(&h, "127.0.0.1:1234", "synthetic").is_err());
    }

    #[test]
    fn authority_is_closed_and_never_accepted_from_client() {
        for action in [
            "permission_respond",
            "eval",
            "invoke",
            "app_control_start",
            "sdk_start_task",
        ] {
            assert!(serde_json::from_value::<Action>(json!({"action":action})).is_err());
        }
        assert!(serde_json::from_value::<Action>(
            json!({"action":"workspace_cancel_prompt","humanOnly":false})
        )
        .is_err());
        assert!(serde_json::from_value::<Action>(
            json!({"action":"workspace_open_dialog","dialog":"profile"})
        )
        .is_err());
        let edit: Action = serde_json::from_value(
            json!({"action":"workspace_edit_draft","text":"x".repeat(32001)}),
        )
        .unwrap();
        assert!(edit.validate().is_err());
        assert!(Action::WorkspaceSend {
            text: "x".repeat(2001),
            agent_id: "".into()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn mutation_requires_current_identity_and_bounded_unique_id() {
        let mut op = Operation {
            request_id: "fixture-1".into(),
            expected: None,
            operation: Action::Snapshot {},
        };
        assert!(op.validate().is_ok());
        op.operation = Action::WorkspaceCancel {};
        assert!(op.validate().is_err());
        op.expected = Some(Expected {
            instance_id: "fixture".into(),
            revision: 1,
            conversation_id: None,
            session_id: None,
            active_turn_id: None,
        });
        assert!(op.validate().is_err());
        op.operation = Action::WorkspaceEditDraft { text: "".into() };
        assert!(op.validate().is_ok());
        op.request_id = "../inject".into();
        assert!(op.validate().is_err());
        assert!(Action::WorkspaceCancel {}.protected());
        assert!(!Action::Snapshot {}.protected());
        assert_eq!(tools().as_array().unwrap().len(), 1);
    }
}
