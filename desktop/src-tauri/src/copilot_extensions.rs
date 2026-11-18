//! Versioned SDK operations in a separately owned background workspace, never an ACP session.

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
    CliProgram, Client, ClientInfo, ClientOptions, MemoryConfiguration, PermissionRequestData,
    PermissionRequestKind, RequestId, SessionConfig, SessionId, Transport,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Runtime};

use crate::copilot::{self, record_transport, CopilotAgent, CopilotPermissionOrigin, Operation};
use crate::copilot_client::{ClientConsent, ConsentDecision};
use crate::problem::Problem;

pub const SCHEMA_VERSION: u8 = 1;
const SDK_VERSION: &str = "1.0.14";
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
            let config = SessionConfig::default()
                .with_working_directory(&cwd)
                .with_memory(MemoryConfiguration::disabled().with_enabled(memory_enabled))
                .with_permission_handler(Arc::new(permissions));
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

fn client_options(cwd: &Path, program: &Path) -> ClientOptions {
    ClientOptions::new()
        .with_program(CliProgram::Path(program.to_owned()))
        .with_transport(Transport::Stdio)
        .with_cwd(cwd)
        .with_extra_args(["--no-auto-update"])
        .with_use_logged_in_user(true)
        .with_client_info(
            ClientInfo::new()
                .with_application_name("Darbot")
                .with_application_version(env!("CARGO_PKG_VERSION"))
                .with_integration_name("native-sdk-extensions")
                .with_integration_version(SCHEMA_VERSION.to_string()),
        )
}

async fn start_client(cwd: &Path) -> Result<Client, Problem> {
    let program = sdk_program(std::env::var_os("DARBOT_COPILOT_SDK_CLI").map(PathBuf::from))?;
    let started = Instant::now();
    let result =
        tokio::time::timeout(RPC_TIMEOUT, Client::start(client_options(cwd, &program))).await;
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
        let options = client_options(Path::new("C:\\project"), &default_program);
        assert!(
            matches!(options.program, CliProgram::Path(ref path) if path == Path::new("copilot"))
        );
        assert!(matches!(options.transport, Transport::Stdio));
        assert!(options.github_token.is_none());
        assert!(!options.enable_remote_sessions);
        assert_eq!(options.use_logged_in_user, Some(true));
        assert_eq!(options.extra_args, ["--no-auto-update"]);
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
