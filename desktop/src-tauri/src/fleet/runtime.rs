use super::*;
use crate::copilot::{CopilotPermissionRequest, CopilotRuntimeState, RuntimeObserver};
use serde_json::{json, Value};
use std::sync::Weak;
use tauri::{Emitter, Manager};

#[derive(Default)]
pub struct FleetState {
    node: Mutex<Option<Arc<Node>>>,
    failure: Mutex<Option<FleetError>>,
    actions: tokio::sync::Mutex<()>,
}

#[derive(Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(
    tag = "action",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum FleetAction {
    Status {},
    ListTasks {},
    GetTask {
        task_id: String,
    },
    Configure {
        listen: String,
        label: String,
    },
    Enable {},
    Disable {},
    Pair {
        card: PairingCard,
    },
    Revoke {
        node_id: String,
    },
    SetProfiles {
        profiles: Vec<Profile>,
    },
    Submit {
        submission: Submission,
    },
    Cancel {
        task_id: String,
    },
    ApproveTask {
        task_id: String,
    },
    Message {
        task_id: String,
        message_id: String,
        text: String,
    },
}

impl FleetState {
    pub async fn initialize(&self, app: &tauri::AppHandle) -> Result<()> {
        let _guard = self.actions.lock().await;
        if self.node.lock().unwrap().is_some() {
            return Ok(());
        }
        let result = self.open(app).await;
        if let Err(e) = &result {
            *self.failure.lock().unwrap() = Some(e.clone());
        }
        result
    }
    async fn open(&self, app: &tauri::AppHandle) -> Result<()> {
        let root = app
            .path()
            .app_local_data_dir()
            .map_err(|e| fail("storage", e.to_string()))?
            .join("fleet");
        let node = Node::open(&root)?;
        *self.node.lock().unwrap() = Some(node.clone());
        if node.state.lock().unwrap().enabled {
            if let Err(e) = transport::start(node.clone()).await {
                *node.failure.lock().unwrap() = Some(e.clone());
                node.shutdown();
                return Err(e);
            }
            maintenance(node.clone(), app.clone());
        }
        *self.node.lock().unwrap() = Some(node);
        *self.failure.lock().unwrap() = None;
        Ok(())
    }
    fn current(&self) -> Result<Arc<Node>> {
        self.node.lock().unwrap().clone().ok_or_else(|| {
            self.failure
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| fail("not_ready", "Fleet is initializing"))
        })
    }
    pub fn shutdown(&self) {
        if let Some(node) = self.node.lock().unwrap().as_ref() {
            node.shutdown();
        }
    }
    pub async fn dispatch(
        &self,
        app: &tauri::AppHandle,
        action: FleetAction,
        remote: bool,
    ) -> Result<Value> {
        let _guard = self.actions.lock().await;
        let node = self.current()?;
        match action {
            FleetAction::Status {} => {
                let r = node.state.lock().unwrap();
                let presence = node.presence.lock().unwrap();
                Ok(json!({
                    "protocol": "darbot-fleet/1", "enabled": r.enabled && !node.stop.is_cancelled(),
                    "card": r.card(), "profiles": r.profiles, "error": node.failure.lock().unwrap().clone(),
                    "peers": r.peers.iter().map(|(id, p)| {
                        let status = presence.get(id).cloned().unwrap_or_default();
                        json!({"card": p.card, "online": now().saturating_sub(status.seen_at) < PRESENCE_MS,
                            "queued":p.outbox.len(), "presence":status})
                    }).collect::<Vec<_>>()
                }))
            }
            FleetAction::ListTasks {} => {
                let record = node.state.lock().unwrap();
                let rows: Vec<_> = record
                    .tasks
                    .values()
                    .map(|t| {
                        json!({
                            "id":t.id, "origin":t.origin, "executor":t.executor,
                            "status":t.status, "revision":t.revision, "sessionId":t.session_id,
                            "cancelRequested":t.cancel_requested, "profileId":t.profile.id,
                        })
                    })
                    .collect();
                Ok(json!(rows))
            }
            FleetAction::GetTask { task_id } => Ok(json!(node
                .state
                .lock()
                .unwrap()
                .tasks
                .get(&task_id)
                .ok_or_else(|| fail("not_found", "Task not found"))?)),
            FleetAction::Configure { listen, label } => {
                if node.state.lock().unwrap().enabled {
                    return Err(fail("busy", "Disable fleet before changing its listener"));
                }
                listen
                    .parse::<std::net::SocketAddr>()
                    .map_err(|_| fail("invalid_endpoint", "Use an explicit bind IP:port"))?;
                if label.is_empty() || label.len() > 100 || label.chars().any(char::is_control) {
                    return Err(fail("invalid_label", "Use a short readable label"));
                }
                approve(app, format!("Save fleet listener {listen} and label {label}?\nNo firewall rules will be changed. Only mutually pinned peers can connect.")).await?;
                node.transaction(|r| {
                    r.listen = listen;
                    r.label = label;
                    Ok(())
                })?;
                Ok(json!({"saved":true}))
            }
            FleetAction::Enable {} => {
                if node.state.lock().unwrap().enabled && !node.stop.is_cancelled() {
                    return Ok(json!({"enabled":true}));
                }
                approve(app, "Enable authenticated fleet communication whenever this desktop is open? Paired peers may deliver selected tasks; each task still requires local execution approval.".into()).await?;
                if node.stop.is_cancelled() {
                    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
                    while Arc::strong_count(&node) > 2 {
                        if tokio::time::Instant::now() >= deadline {
                            return Err(fail(
                                "busy",
                                "Previous fleet workers are still stopping; retry enabling shortly",
                            ));
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    self.node.lock().unwrap().take();
                    drop(node);
                    self.open(app).await?;
                    if !self.current()?.stop.is_cancelled()
                        && self.current()?.state.lock().unwrap().enabled
                    {
                        return Ok(json!({"enabled":true}));
                    }
                }
                let node = self.current()?;
                node.transaction(|r| {
                    r.enabled = true;
                    Ok(())
                })?;
                if let Err(e) = transport::start(node.clone()).await {
                    node.transaction(|r| {
                        r.enabled = false;
                        Ok(())
                    })?;
                    return Err(e);
                }
                maintenance(node, app.clone());
                Ok(json!({"enabled":true}))
            }
            FleetAction::Disable {} => {
                approve(app, "Disable fleet communication and cancel this node's active fleet work? Durable queued deliveries survive.".into()).await?;
                node.transaction(|r| {
                    r.enabled = false;
                    Ok(())
                })?;
                node.shutdown();
                Ok(json!({"enabled":false}))
            }
            FleetAction::Pair { card } => {
                card.validate()?;
                approve(app, format!("Pair once with {} at {}?\n\nFingerprint:\n{}\n\nCompare this fingerprint with the other Windows user's local Darbot screen through a trusted channel. The key is bound to their OS profile; a matching username is NOT identity proof.\nImport your card on that node too. Pairing does not authorize tasks.", card.label, card.endpoint, card.node_id)).await?;
                node.pair(card)?;
                Ok(json!({"paired":true}))
            }
            FleetAction::Revoke { node_id } => {
                approve(app, format!("Revoke fleet peer {node_id}? Its queued deliveries will be discarded and active outcomes marked for reconciliation.")).await?;
                node.revoke(&node_id)?;
                Ok(json!({"revoked":true}))
            }
            FleetAction::SetProfiles { mut profiles } => {
                if profiles.len() > 16 {
                    return Err(fail("invalid_profile", "At most 16 local profiles"));
                }
                let mut ids = std::collections::HashSet::new();
                for p in &mut profiles {
                    p.validate()?;
                    if !ids.insert(p.id.clone()) {
                        return Err(fail("invalid_profile", "Duplicate profile ID"));
                    }
                    p.root = crate::copilot::session_root(&p.root)
                        .map_err(|e| fail("invalid_profile", e.said))?;
                }
                approve(app, format!("Allow these local fleet execution scopes to be advertised to paired users?\n{}\n\nThe selected agent is the personality. Models are verified against the real CLI session before any prompt is sent. Every task and tool still asks permission.", serde_json::to_string_pretty(&profiles).map_err(|e| fail("profile", e.to_string()))?)).await?;
                node.transaction(|r| {
                    r.profiles = profiles;
                    Ok(())
                })?;
                Ok(json!({"saved":true}))
            }
            FleetAction::Submit { submission } => {
                if submission.prompt.chars().count() > 2000 || submission.targets.len() > 64 {
                    return Err(fail("invalid_task", "Submission exceeds review limits"));
                }
                approve(app, format!("Share ONLY this prompt and its generated result with one eligible round-robin target?\n{}\n\nTargets:\n{}\n\nEach executor requires its own local approval.", submission.prompt, serde_json::to_string(&submission.targets).map_err(|e| fail("task", e.to_string()))?)).await?;
                Ok(json!(node.submit(submission)?))
            }
            FleetAction::Cancel { task_id } => {
                approve(app, format!("Request cancellation of fleet task {task_id}? A running side effect may already have happened.")).await?;
                node.cancel(&task_id)?;
                Ok(json!({"cancellationRequested":true}))
            }
            FleetAction::ApproveTask { task_id } => {
                // MCP/REST and peer traffic cannot manufacture local execution approval.
                if remote {
                    return Err(fail(
                        "forbidden",
                        "Approve fleet execution on this desktop's Fleet panel",
                    ));
                }
                let task = node
                    .state
                    .lock()
                    .unwrap()
                    .tasks
                    .get(&task_id)
                    .cloned()
                    .ok_or_else(|| fail("not_found", "Task not found"))?;
                if let Err(e) = approve(app, format!("Run this fleet task as your signed-in Copilot account?\nOrigin: {}\nAgent/personality: {}\nModel: {}\nRoot: {}\nPermissions: {}\n\n{}\n\nOnly the assistant's text result will be sent back. No private history, screens or credentials are synchronized.", task.origin, task.profile.agent, task.profile.model, task.profile.root, task.profile.permission_profile, task.prompt)).await {
                    if e.code == "denied" {
                        node.transaction(|r| {
                            let mut t = r.tasks[&task_id].clone();
                            if t.executor == r.node_id && t.session_id.is_none()
                                && matches!(t.status, Status::Queued | Status::InputRequired) {
                                t.status = Status::Failed;
                                t.detail = "Local owner declined execution; no prompt was sent".into();
                                t.revision += 1;
                                r.publish(t)?;
                            }
                            Ok(())
                        })?;
                    }
                    return Err(e);
                }
                launch(node, app.clone(), &task_id)?;
                Ok(json!({"accepted":true}))
            }
            FleetAction::Message {
                task_id,
                message_id,
                text,
            } => {
                approve(app, format!("Send this note only to the other participant of fleet task {task_id}?\n\n{text}\n\nNotes do not start agent turns or grant permissions.")).await?;
                node.message(&task_id, message_id, text)?;
                Ok(json!({"queued":true}))
            }
        }
    }
}

pub(super) async fn approve(app: &tauri::AppHandle, message: String) -> Result<()> {
    if message.len() > 12 * 1024 {
        return Err(fail(
            "review_limit",
            "Request is too large for native review",
        ));
    }
    crate::app_control::approve_fleet_owner(app, message).await
}

#[tauri::command]
pub async fn fleet_dispatch(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, FleetState>,
    action: FleetAction,
) -> Result<Value> {
    crate::app_control::main_window(&window)
        .map_err(|_| fail("forbidden", "Fleet controls require the local desktop"))?;
    state.initialize(&app).await?;
    state.dispatch(&app, action, false).await
}

fn maintenance(node: Arc<Node>, app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            let result = node.maintain();
            if let Err(e) = result {
                let _ = app.emit("fleet:problem", e);
            }
            tokio::select! { _ = node.stop.cancelled() => return, _ = tokio::time::sleep(Duration::from_secs(2)) => {} }
        }
    });
}

fn launch(node: Arc<Node>, app: tauri::AppHandle, task_id: &str) -> Result<()> {
    let cancel = node.stop.child_token();
    {
        let mut active = node.execution.lock().unwrap();
        if active.is_some() || node.stop.is_cancelled() {
            return Err(fail("busy", "Fleet executor is busy or stopped"));
        }
        node.transaction(|r| {
            let mut t = r
                .tasks
                .get(task_id)
                .cloned()
                .ok_or_else(|| fail("not_found", "Task not found"))?;
            if !r.enabled
                || t.executor != r.node_id
                || !matches!(t.status, Status::Queued | Status::InputRequired)
                || t.session_id.is_some()
                || t.cancel_requested
                || t.expires_at <= now()
                || !r.profiles.contains(&t.profile)
            {
                return Err(fail(
                    "stale",
                    "Task no longer eligible for this execution approval",
                ));
            }
            t.profile.validate()?;
            if std::fs::canonicalize(&t.profile.root)
                .map_err(io)?
                .to_string_lossy()
                != t.profile.root
            {
                return Err(fail(
                    "root_changed",
                    "Approved root changed; approve a new profile",
                ));
            }
            t.status = Status::Running;
            t.revision += 1;
            t.detail = "Opening the real Copilot CLI session".into();
            r.publish(t)
        })?;
        *active = Some((task_id.into(), cancel.clone()));
    }
    let task_id = task_id.to_owned();
    tauri::async_runtime::spawn(async move {
        let observer = Arc::new(Observer {
            node: Arc::downgrade(&node),
            task_id: task_id.clone(),
            runtime: Mutex::new(Weak::new()),
            app: app.clone(),
            cancel: cancel.clone(),
        });
        let runtime = Arc::new(CopilotRuntimeState::with_observer(observer.clone()));
        *observer.runtime.lock().unwrap() = Arc::downgrade(&runtime);
        let worker_node = node.clone();
        let worker_runtime = runtime.clone();
        let worker_id = task_id.clone();
        let mut work = tauri::async_runtime::spawn_blocking(move || {
            execute(&worker_node, &app, &worker_runtime, &worker_id)
        });
        let remaining = {
            let record = node.state.lock().unwrap();
            Duration::from_millis(record.tasks[&task_id].expires_at.saturating_sub(now()))
        };
        let result = tokio::select! {
            result = &mut work => result.map_err(|e| fail("runtime", e.to_string())).and_then(|v| v),
            _ = cancel.cancelled() => {
                runtime.shutdown();
                let _ = work.await;
                Err(fail("reconciliation_required", "Execution interrupted; side effects may already have occurred"))
            }
            _ = tokio::time::sleep(remaining) => {
                cancel.cancel();
                runtime.shutdown();
                let _ = work.await;
                Err(fail("lease_expired", "Execution lease expired; inspect side effects before any retry"))
            }
        };
        runtime.shutdown();
        let persist = node.transaction(|r| {
            let mut t = r
                .tasks
                .get(&task_id)
                .cloned()
                .ok_or_else(|| fail("not_found", "Task disappeared"))?;
            if t.status.terminal() {
                return Ok(());
            }
            t.status = if result.is_ok() {
                Status::Completed
            } else if t.prompt_dispatched {
                Status::ReconciliationRequired
            } else if t.cancel_requested || cancel.is_cancelled() {
                Status::Cancelled
            } else {
                Status::Failed
            };
            t.detail = match result {
                Ok(reason) => reason,
                Err(e) => e.message,
            };
            t.revision += 1;
            t.artifacts = vec![Artifact {
                name: "assistant-response.txt".into(),
                media_type: "text/plain".into(),
                bytes: t.output.len(),
                sha256: fingerprint(t.output.as_bytes()),
            }];
            r.publish(t)
        });
        if let Err(e) = persist {
            *node.failure.lock().unwrap() = Some(e);
        }
        *node.execution.lock().unwrap() = None;
    });
    Ok(())
}

fn execute(
    node: &Node,
    app: &tauri::AppHandle,
    runtime: &CopilotRuntimeState,
    task_id: &str,
) -> Result<String> {
    let task = node.state.lock().unwrap().tasks[task_id].clone();
    let agent = (!task.profile.agent.is_empty()).then(|| task.profile.agent.clone());
    let session = runtime
        .new_session(app, agent.clone(), task.profile.root.clone())
        .map_err(|e| fail("runtime", e.said))?;
    node.transaction(|r| {
        let mut t = r.tasks[task_id].clone();
        if t.cancel_requested || !r.enabled || node.stop.is_cancelled() {
            return Err(fail("cancelled", "Task was cancelled before prompt"));
        }
        t.session_id = Some(session.session_id.clone());
        t.revision += 1;
        r.tasks.insert(t.id.clone(), t);
        Ok(())
    })?;
    runtime
        .configure(session.session_id.clone(), "allow_all".into(), "off".into())
        .map_err(|e| fail("permissions_unavailable", e.said))?;
    runtime
        .configure(
            session.session_id.clone(),
            "model".into(),
            task.profile.model.clone(),
        )
        .map_err(|e| fail("model_unavailable", e.said))?;
    if node.stop.is_cancelled() || node.state.lock().unwrap().tasks[task_id].cancel_requested {
        return Err(fail("cancelled", "Cancelled before prompt"));
    }
    node.transaction(|r| {
        let mut t = r.tasks[task_id].clone();
        if t.cancel_requested || !r.enabled || t.expires_at <= now() {
            return Err(fail("cancelled", "Task is no longer eligible to send"));
        }
        // Persist intent before the potentially side-effecting ACP write; failures after this
        // point require reconciliation rather than an automatic second prompt.
        t.prompt_dispatched = true;
        t.revision += 1;
        r.publish(t)
    })?;
    let result = runtime
        .fleet_prompt(
            &task.profile.agent,
            &task.profile.model,
            &task.profile.root,
            session.session_id,
            task.prompt,
        )
        .map_err(|e| fail("runtime", e.said))?;
    if result.stop_reason != "end_turn" {
        return Err(fail(
            "not_completed",
            format!("Copilot stopped with {}", result.stop_reason),
        ));
    }
    Ok(format!("Copilot completed: {}", result.stop_reason))
}

struct Observer {
    node: Weak<Node>,
    task_id: String,
    runtime: Mutex<Weak<CopilotRuntimeState>>,
    app: tauri::AppHandle,
    cancel: CancellationToken,
}
impl RuntimeObserver for Observer {
    fn update(&self, session: &str, update: &Value) {
        if update["sessionUpdate"] != "agent_message_chunk" || update["content"]["type"] != "text" {
            return;
        }
        let Some(text) = update["content"]["text"].as_str() else {
            return;
        };
        let Some(node) = self.node.upgrade() else {
            return;
        };
        let result = node.transaction(|r| {
            let mut t = r.tasks[&self.task_id].clone();
            if t.session_id.as_deref() != Some(session) || t.status.terminal() {
                return Ok(());
            }
            if t.output.len() + text.len() > MAX_OUTPUT {
                return Err(fail(
                    "output_limit",
                    "Assistant output exceeded 128 KiB; execution stopped",
                ));
            }
            t.output.push_str(text);
            t.status = Status::Running;
            t.revision += 1;
            r.tasks.insert(t.id.clone(), t);
            Ok(())
        });
        if let Err(e) = result {
            *node.failure.lock().unwrap() = Some(e.clone());
            let _ = self.app.emit("fleet:problem", e);
            self.cancel.cancel();
        }
    }
    fn permission(&self, request: CopilotPermissionRequest) {
        let Some(runtime) = self.runtime.lock().unwrap().upgrade() else {
            return;
        };
        let Some(node) = self.node.upgrade() else {
            return;
        };
        let app = self.app.clone();
        let task_id = self.task_id.clone();
        let cancel = self.cancel.clone();
        tauri::async_runtime::spawn(async move {
            let changed = node.transaction(|r| {
                let mut t = r.tasks[&task_id].clone();
                if t.session_id.as_deref() != Some(&request.session_id) || t.status.terminal() {
                    return Err(fail(
                        "stale",
                        "Permission belongs to another task incarnation",
                    ));
                }
                t.status = Status::InputRequired;
                t.revision += 1;
                t.detail = "Local tool approval required".into();
                r.publish(t)
            });
            let allowed = if changed.is_ok() {
                tokio::select! {
                    _ = cancel.cancelled() => false,
                    result = approve(&app, format!("Fleet task {task_id} requests a tool.\nApprove only if its paths and actions match the approved repository scope.\n\n{}", request.tool_call)) => result.is_ok(),
                }
            } else {
                false
            };
            let choice = if allowed && !cancel.is_cancelled() {
                request
                    .options
                    .iter()
                    .find(|o| o.kind == "allow_once")
                    .map(|o| o.option_id.clone())
            } else {
                None
            };
            if let Err(e) = runtime.respond_permission(&app, None, request.request_id, choice) {
                let _ = app.emit("fleet:problem", fail("permission", e.said));
            }
        });
    }
}
