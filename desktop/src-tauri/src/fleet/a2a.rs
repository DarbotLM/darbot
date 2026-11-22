//! A2A v0.3.0 JSON-RPC text-task profile. TLS authentication is shared with Fleet/1;
//! neither a message nor a client-supplied task state is execution authorization.
use super::*;
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{
        sse::{Event as SseEvent, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::convert::Infallible;
use tokio_stream::wrappers::ReceiverStream;

const EXTENSION: &str = "urn:darbot:fleet:1";
#[derive(Clone)]
struct Caller {
    node: Arc<Node>,
    peer: String,
}

pub(super) async fn serve(
    node: Arc<Node>,
    peer: String,
    stream: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
) -> Result<()> {
    let router = Router::new()
        .route("/.well-known/agent-card.json", get(card))
        .route("/a2a", post(rpc))
        .layer(DefaultBodyLimit::max(32 * 1024))
        .with_state(Caller { node, peer });
    hyper::server::conn::http1::Builder::new()
        .keep_alive(false)
        .serve_connection(
            hyper_util::rt::TokioIo::new(stream),
            hyper_util::service::TowerToHyperService::new(router),
        )
        .await
        .map_err(|e| fail("http", e.to_string()))
}

fn authorized(r: &Record, peer: &str) -> Result<()> {
    if !r.enabled || !r.peers.contains_key(peer) {
        return Err(fail("forbidden", "Fleet disabled or certificate revoked"));
    }
    Ok(())
}
fn agent_card(r: &Record) -> Value {
    let port = r
        .listen
        .parse::<std::net::SocketAddr>()
        .map(|a| a.port())
        .unwrap_or(0);
    json!({
        "protocolVersion":"0.3.0", "name":r.label, "version":env!("CARGO_PKG_VERSION"),
        "description":"Owner-approved, repository-scoped Copilot tasks. One new text task per message; no automatic approvals or private session synchronization.",
        "url":format!("https://darbot-fleet:{port}/a2a"), "preferredTransport":"JSONRPC",
        "capabilities":{"streaming":true,"pushNotifications":false,"stateTransitionHistory":false,
            "extensions":[{"uri":EXTENSION,"required":true,
                "description":"Send A2A-Extensions: urn:darbot:fleet:1. Use a per-peer client resolver mapping darbot-fleet to connectAddress (no global DNS changes). Import the pinned certificate and client identity; never disable TLS verification. Select params.metadata[urn:darbot:fleet:1].profileId. Optional expiresAt is Unix milliseconds, at most 24 hours. Reuse messageId to reconcile delivery.",
                "params":{"schemaVersion":1,"tlsServerName":"darbot-fleet","connectAddress":r.listen,"certificateSha256":r.node_id,
                    "maxPromptCharacters":2000,"maxPromptLines":24,"maxOutputBytes":MAX_OUTPUT,
                    "capacity":1,"maxTasks":MAX_TASKS,"maxStreamSeconds":55,
                    "supportedMethods":["message/send","message/stream","tasks/get","tasks/cancel","tasks/resubscribe"],
                    "unsupported":["pushNotifications","extendedAgentCard","fileParts","dataParts","taskContinuation","sharedContexts","stateTransitionHistory"],
                    "reconciliation":"failed with outcomeUnknown=true; never automatically reissue a side-effecting prompt",
                    "statusMetadata":"Each status-update replaces metadata[urn:darbot:fleet:1] with the current revision, including reconciliation state."}}]},
        "defaultInputModes":["text/plain"],"defaultOutputModes":["text/plain"],
        "securitySchemes":{"pinnedPeer":{"type":"mutualTLS","description":"Both owners must import and verify public pairing cards. Exact leaf certificate pin and proof of possession required; username is not identity."}},
        "security":[{"pinnedPeer":[]}], "supportsAuthenticatedExtendedCard":false,
        "skills":r.profiles.iter().map(|p| json!({"id":p.id,"name":p.id,
            "description":format!("Copilot agent {} using model {}. Local execution and each tool require owner approval.",p.agent,p.model),
            "tags":["copilot","owner-approved","text-task"],"inputModes":["text/plain"],"outputModes":["text/plain"]})).collect::<Vec<_>>()
    })
}
async fn card(State(c): State<Caller>) -> Response {
    let r = c.node.state.lock().unwrap();
    if authorized(&r, &c.peer).is_err() {
        return StatusCode::FORBIDDEN.into_response();
    }
    Json(agent_card(&r)).into_response()
}
fn error(id: Value, code: i32, message: impl Into<String>) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message.into()}})
}
fn result(id: &Value, value: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":value})
}
fn mapped(id: Value, e: FleetError) -> Value {
    let code = match e.code {
        "not_found" | "forbidden" => -32001,
        "not_cancelable" => -32002,
        "unsupported" => -32004,
        "content_type" => -32005,
        "storage" | "storage_full" | "protected_storage" => -32603,
        _ => -32602,
    };
    error(id, code, e.message)
}
fn task_message(t: &Task, role: &str, text: &str, suffix: &str) -> Value {
    let message_id = if role == "user" {
        t.request_id.clone()
    } else {
        format!("{}-{suffix}-{}", t.id, t.revision)
    };
    json!({"kind":"message","messageId":message_id,"role":role,
        "taskId":t.id,"contextId":t.id,"parts":[{"kind":"text","text":text}]})
}
fn status(t: &Task) -> Value {
    let state = match t.status {
        Status::Queued => "submitted",
        Status::InputRequired => "input-required",
        Status::Running => "working",
        Status::Completed => "completed",
        Status::Cancelled => "canceled",
        Status::Failed | Status::ReconciliationRequired => "failed",
    };
    json!({"state":state,"message":task_message(t,"agent",&t.detail,"status")})
}
fn artifact(t: &Task) -> Value {
    json!({"artifactId":format!("{}-response",t.id),"name":"assistant-response.txt",
        "parts":[{"kind":"text","text":t.output}],
        "metadata":{EXTENSION:{"sha256":fingerprint(t.output.as_bytes()),"bytes":t.output.len()}}})
}
fn task_metadata(t: &Task) -> Value {
    json!({EXTENSION:{"revision":t.revision,
        "cancelRequested":t.cancel_requested,"outcomeUnknown":t.status == Status::ReconciliationRequired,
        "profileId":t.profile.id,"expiresAt":t.expires_at}})
}
fn task_value(t: &Task, history: Option<usize>) -> Value {
    let mut messages = vec![task_message(t, "user", &t.prompt, "request")];
    if !t.output.is_empty() {
        messages.push(task_message(t, "agent", &t.output, "response"));
    }
    if let Some(limit) = history {
        messages = messages.into_iter().rev().take(limit).collect::<Vec<_>>();
        messages.reverse();
    }
    json!({"kind":"task","id":t.id,"contextId":t.id,"status":status(t),
        "artifacts":if t.output.is_empty() { vec![] } else { vec![artifact(t)] },
        "history":messages,"metadata":task_metadata(t)})
}
fn get_task(c: &Caller, id: &str) -> Result<Task> {
    let r = c.node.state.lock().unwrap();
    authorized(&r, &c.peer)?;
    r.tasks
        .get(id)
        .filter(|t| t.a2a && t.origin == c.peer && t.executor == r.node_id)
        .cloned()
        .ok_or_else(|| fail("not_found", "Task not found"))
}
fn send(c: &Caller, params: &Value) -> Result<Task> {
    let m = &params["message"];
    if m["kind"] != "message" || m["role"] != "user" {
        return Err(fail("invalid_message", "Expected an A2A user message"));
    }
    if m.get("taskId").is_some()
        || m.get("contextId").is_some()
        || m.get("referenceTaskIds").is_some()
    {
        return Err(fail(
            "unsupported",
            "This profile accepts new tasks only; shared contexts and continuation are unsupported",
        ));
    }
    let message_id = m["messageId"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 100)
        .ok_or_else(|| fail("invalid_message", "messageId must contain 1–100 bytes"))?;
    let parts = m["parts"]
        .as_array()
        .filter(|p| !p.is_empty() && p.len() <= 24)
        .ok_or_else(|| fail("invalid_message", "Expected 1–24 text parts"))?;
    let mut text = vec![];
    for part in parts {
        if part["kind"] != "text" {
            return Err(fail("content_type", "Only text parts are supported"));
        }
        text.push(
            part["text"]
                .as_str()
                .ok_or_else(|| fail("invalid_message", "Text part requires text"))?,
        );
    }
    let prompt = text.join("\n");
    if prompt.trim().is_empty()
        || prompt.chars().count() > 2000
        || prompt.lines().count() > 24
        || prompt.trim_start().starts_with('/')
    {
        return Err(fail(
            "invalid_message",
            "Prompt limit: 2000 characters / 24 lines, no slash commands",
        ));
    }
    let config = &params["configuration"];
    if config.get("pushNotificationConfig").is_some() {
        return Err(fail("unsupported", "Push notifications are unsupported"));
    }
    if let Some(modes) = config.get("acceptedOutputModes") {
        let modes = modes
            .as_array()
            .ok_or_else(|| fail("invalid_message", "Invalid acceptedOutputModes"))?;
        if !modes.iter().any(|m| m == "text/plain") {
            return Err(fail("content_type", "This profile produces text/plain"));
        }
    }
    let extension = &params["metadata"][EXTENSION];
    let profile_id = extension["profileId"].as_str().ok_or_else(|| {
        fail(
            "invalid_message",
            "Select metadata[urn:darbot:fleet:1].profileId from the agent card",
        )
    })?;
    let expires = match extension.get("expiresAt") {
        Some(value) => value
            .as_u64()
            .ok_or_else(|| fail("invalid_message", "expiresAt must be Unix milliseconds"))?,
        None => now() + 60 * 60 * 1000,
    };
    c.node.transaction(|r| {
        authorized(r, &c.peer)?;
        if let Some(old) = r
            .tasks
            .values()
            .find(|t| t.a2a && t.origin == c.peer && t.request_id == message_id)
        {
            if old.prompt != prompt
                || old.profile.id != profile_id
                || extension
                    .get("expiresAt")
                    .is_some_and(|_| old.expires_at != expires)
            {
                return Err(fail(
                    "duplicate",
                    "messageId already identifies different task content",
                ));
            }
            return Ok(old.clone());
        }
        if expires <= now() || expires > now() + 24 * 60 * 60 * 1000 {
            return Err(fail(
                "invalid_message",
                "Expiry must be within the next 24 hours",
            ));
        }
        if r.tasks.len() >= MAX_TASKS
            || r.tasks
                .values()
                .any(|t| t.executor == r.node_id && !t.status.terminal())
        {
            return Err(fail(
                "no_capacity",
                "Executor capacity/history full; nothing was accepted",
            ));
        }
        let profile = r
            .profiles
            .iter()
            .find(|p| p.id == profile_id)
            .cloned()
            .ok_or_else(|| fail("invalid_profile", "Profile is not currently approved"))?;
        let t = Task {
            id: id(),
            request_id: message_id.into(),
            origin: c.peer.clone(),
            executor: r.node_id.clone(),
            profile,
            prompt,
            expires_at: expires,
            status: Status::InputRequired,
            revision: 1,
            session_id: None,
            output: String::new(),
            detail: "Awaiting local execution approval".into(),
            artifacts: vec![],
            cancel_requested: false,
            prompt_dispatched: false,
            a2a: true,
            messages: vec![],
        };
        r.publish(t.clone())?;
        Ok(t)
    })
}
fn cancel(c: &Caller, task_id: &str) -> Result<Task> {
    c.node
        .transaction(|r| {
            authorized(r, &c.peer)?;
            let mut t = r
                .tasks
                .get(task_id)
                .filter(|t| t.a2a && t.origin == c.peer && t.executor == r.node_id)
                .cloned()
                .ok_or_else(|| fail("not_found", "Task not found"))?;
            if t.status.terminal() {
                return Err(fail("not_cancelable", "Task is already terminal"));
            }
            t.cancel_requested = true;
            if t.session_id.is_none() && t.status != Status::Running {
                t.status = Status::Cancelled;
                t.detail = "Cancelled before execution".into();
            } else {
                t.detail = "Cancellation requested; execution outcome pending".into();
            }
            t.revision += 1;
            r.publish(t.clone())?;
            Ok(t)
        })
        .inspect(|_| c.node.cancel_revoked_execution())
}
async fn rpc(State(c): State<Caller>, headers: HeaderMap, body: Bytes) -> Response {
    let mut response = rpc_inner(c, headers, body).await;
    response
        .headers_mut()
        .insert("a2a-extensions", HeaderValue::from_static(EXTENSION));
    response
}
async fn rpc_inner(c: Caller, headers: HeaderMap, body: Bytes) -> Response {
    let request: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return Json(error(Value::Null, -32700, "Parse error")).into_response(),
    };
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    if request["jsonrpc"] != "2.0"
        || !(id.is_string() || id.is_i64() || id.is_u64())
        || !request["method"].is_string()
    {
        return Json(error(
            Value::Null,
            -32600,
            "Expected a single JSON-RPC 2.0 request with an ID",
        ))
        .into_response();
    }
    if authorized(&c.node.state.lock().unwrap(), &c.peer).is_err() {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !headers
        .get_all("a2a-extensions")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|v| v.trim() == EXTENSION)
    {
        return Json(error(
            id,
            -32602,
            "This profile requires A2A-Extensions: urn:darbot:fleet:1",
        ))
        .into_response();
    }
    let method = request["method"].as_str().unwrap();
    if method.starts_with("tasks/pushNotificationConfig/") {
        return Json(error(id, -32003, "Push notifications are not supported")).into_response();
    }
    if method == "agent/getAuthenticatedExtendedCard" {
        return Json(error(
            id,
            -32007,
            "Authenticated extended card is not configured",
        ))
        .into_response();
    }
    if !matches!(
        method,
        "message/send" | "message/stream" | "tasks/get" | "tasks/cancel" | "tasks/resubscribe"
    ) {
        return Json(error(id, -32601, "Method not found")).into_response();
    }
    let params = &request["params"];
    let history = match params
        .get("historyLength")
        .or_else(|| params["configuration"].get("historyLength"))
    {
        None => None,
        Some(v) => match v.as_u64().filter(|n| *n <= 100) {
            Some(v) => Some(v as usize),
            None => return Json(error(id, -32602, "historyLength must be 0–100")).into_response(),
        },
    };
    let task = if method.starts_with("message/") {
        send(&c, params)
    } else {
        match params["id"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 100)
        {
            Some(task_id) if method == "tasks/cancel" => cancel(&c, task_id),
            Some(task_id) => get_task(&c, task_id),
            None => Err(fail("invalid_message", "Task id required")),
        }
    };
    match task {
        Err(e) => Json(mapped(id, e)).into_response(),
        Ok(task) if method == "message/stream" || method == "tasks/resubscribe" => {
            stream(c, id, task, history)
        }
        Ok(task) => Json(result(&id, task_value(&task, history))).into_response(),
    }
}
fn stream(c: Caller, id: Value, initial: Task, history: Option<usize>) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<std::result::Result<SseEvent, Infallible>>(4);
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(55);
        let mut previous = None;
        let mut task = initial;
        loop {
            if previous != Some(task.revision) {
                let mut events = vec![];
                if previous.is_none() {
                    events.push(task_value(&task, history));
                } else if !task.output.is_empty() {
                    events.push(json!({"kind":"artifact-update","taskId":task.id,"contextId":task.id,
                        "artifact":artifact(&task),"append":false,"lastChunk":task.status.terminal()}));
                }
                let final_event = task.status.terminal() || task.status == Status::InputRequired;
                events.push(
                    json!({"kind":"status-update","taskId":task.id,"contextId":task.id,
                    "status":status(&task),"metadata":task_metadata(&task),"final":final_event}),
                );
                for value in events {
                    let event = SseEvent::default().data(result(&id, value).to_string());
                    tokio::select! {
                        _ = c.node.stop.cancelled() => return,
                        _ = tokio::time::sleep_until(deadline) => return,
                        sent = tx.send(Ok(event)) => if sent.is_err() { return; },
                    }
                }
                previous = Some(task.revision);
                if final_event {
                    return;
                }
            }
            tokio::select! {
                _ = c.node.stop.cancelled() => return,
                _ = tx.closed() => return,
                _ = tokio::time::sleep_until(deadline) => return,
                _ = tokio::time::sleep(Duration::from_millis(200)) => {},
            }
            match get_task(&c, &task.id) {
                Ok(next) => task = next,
                Err(_) => return,
            }
        }
    });
    Sse::new(ReceiverStream::new(rx))
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(10))
                .text("resubscribe after disconnect"),
        )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine};

    fn client(node: &Node, server: &Node) -> reqwest::Client {
        let r = node.state.lock().unwrap();
        let pem = format!("-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            STANDARD.encode(&r.certificate), STANDARD.encode(&r.private_key));
        let server = server.state.lock().unwrap();
        reqwest::Client::builder()
            .no_proxy()
            .default_headers(HeaderMap::from_iter([(
                axum::http::HeaderName::from_static("a2a-extensions"),
                HeaderValue::from_static(EXTENSION),
            )]))
            .identity(reqwest::Identity::from_pem(pem.as_bytes()).unwrap())
            .add_root_certificate(reqwest::Certificate::from_der(&server.certificate).unwrap())
            .resolve("darbot-fleet", server.listen.parse().unwrap())
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap()
    }
    fn request(method: &str, params: Value) -> Value {
        json!({"jsonrpc":"2.0","id":"fixture","method":method,"params":params})
    }
    async fn call(client: &reqwest::Client, url: &str, method: &str, params: Value) -> Value {
        client
            .post(url)
            .json(&request(method, params))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    #[tokio::test]
    async fn pinned_http_profile_delivers_real_tasks_streams_artifacts_and_enforces_ownership() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("a2a-{}", id()));
        std::fs::create_dir_all(&root).unwrap();
        let a = Node::open(&root.join("a")).unwrap();
        let b = Node::open(&root.join("b")).unwrap();
        let stranger = Node::open(&root.join("stranger")).unwrap();
        for node in [&a, &b, &stranger] {
            node.transaction(|r| {
                r.enabled = true;
                r.profiles = vec![Profile {
                    id: "fixture".into(),
                    agent: String::new(),
                    model: "fixture-only".into(),
                    root: root.to_string_lossy().into_owned(),
                    permission_profile: "ask-every-tool".into(),
                }];
                Ok(())
            })
            .unwrap();
        }
        transport::start(b.clone()).await.unwrap();
        for node in [&a, &stranger] {
            b.pair(node.state.lock().unwrap().card()).unwrap();
            node.pair(b.state.lock().unwrap().card()).unwrap();
        }
        let port = b
            .state
            .lock()
            .unwrap()
            .listen
            .parse::<std::net::SocketAddr>()
            .unwrap()
            .port();
        let url = format!("https://darbot-fleet:{port}/a2a");
        let client = client(&a, &b);
        let card: Value = client
            .get(format!(
                "https://darbot-fleet:{port}/.well-known/agent-card.json"
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(card["protocolVersion"], "0.3.0");
        assert_eq!(card["capabilities"]["streaming"], true);
        assert_eq!(card["securitySchemes"]["pinnedPeer"]["type"], "mutualTLS");
        let params = json!({"message":{"kind":"message","messageId":"fixed-fixture","role":"user",
            "parts":[{"kind":"text","text":"Deterministic fixture task, never a model prompt"}]},
            "metadata":{EXTENSION:{"profileId":"fixture"}}});
        let accepted = call(&client, &url, "message/send", params.clone()).await;
        assert_eq!(accepted["result"]["status"]["state"], "input-required");
        let task_id = accepted["result"]["id"].as_str().unwrap().to_owned();
        assert_eq!(
            call(&client, &url, "message/send", params.clone()).await["result"]["id"],
            task_id
        );
        assert_eq!(b.state.lock().unwrap().tasks.len(), 1);
        let mut conflicting = params.clone();
        conflicting["message"]["parts"][0]["text"] = json!("Different task");
        assert_eq!(
            call(&client, &url, "message/send", conflicting).await["error"]["code"],
            -32602
        );
        let unowned = tests::client(&stranger, &b);
        assert_eq!(
            call(&unowned, &url, "tasks/get", json!({"id":task_id})).await["error"]["code"],
            -32001
        );
        assert_eq!(
            call(&client, &url, "tasks/pushNotificationConfig/set", json!({})).await["error"]
                ["code"],
            -32003
        );
        assert_eq!(
            call(
                &client,
                &url,
                "agent/getAuthenticatedExtendedCard",
                json!({})
            )
            .await["error"]["code"],
            -32007
        );
        assert_eq!(
            call(&client, &url, "unknown", json!({})).await["error"]["code"],
            -32601
        );
        let mut invalid_content = params.clone();
        invalid_content["message"]["parts"][0] =
            json!({"kind":"file","file":{"uri":"file:///private"}});
        assert_eq!(
            call(&client, &url, "message/send", invalid_content).await["error"]["code"],
            -32005
        );
        let stream = client
            .post(&url)
            .json(&request("message/stream", params))
            .send()
            .await
            .unwrap();
        assert!(stream.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream"));
        let text = stream.text().await.unwrap();
        assert!(text.contains("\"kind\":\"task\"") && text.contains("\"final\":true"));
        // A fixture executor transition, not simulated production Copilot success.
        b.transaction(|r| {
            let mut t = r.tasks[&task_id].clone();
            t.status = Status::Running;
            t.prompt_dispatched = true;
            t.revision += 1;
            r.publish(t)
        })
        .unwrap();
        let http = client.clone();
        let endpoint = url.clone();
        let tid = task_id.clone();
        let subscription = tokio::spawn(async move {
            http.post(endpoint)
                .json(&request("tasks/resubscribe", json!({"id":tid})))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        });
        tokio::time::sleep(Duration::from_millis(350)).await;
        b.transaction(|r| {
            let mut t = r.tasks[&task_id].clone();
            t.status = Status::Completed;
            t.revision += 1;
            t.output = "Known fixture artifact".into();
            t.detail = "Fixture completed".into();
            r.publish(t)
        })
        .unwrap();
        let streamed = subscription.await.unwrap();
        assert!(
            streamed.contains("\"kind\":\"artifact-update\"")
                && streamed.contains("\"state\":\"completed\"")
        );
        let got = call(
            &client,
            &url,
            "tasks/get",
            json!({"id":task_id,"historyLength":0}),
        )
        .await;
        assert_eq!(
            got["result"]["artifacts"][0]["parts"][0]["text"],
            "Known fixture artifact"
        );
        assert_eq!(got["result"]["history"], json!([]));
        assert_eq!(
            call(&client, &url, "tasks/cancel", json!({"id":task_id})).await["error"]["code"],
            -32002
        );
        assert!(b
            .state
            .lock()
            .unwrap()
            .peers
            .values()
            .all(|p| p.outbox.is_empty()));
        let cancel_params = json!({"message":{"kind":"message","messageId":"cancel-fixture","role":"user",
            "parts":[{"kind":"text","text":"Cancel before executing"}]},"metadata":{EXTENSION:{"profileId":"fixture"}}});
        let pending = call(&client, &url, "message/send", cancel_params).await;
        assert_eq!(
            call(
                &client,
                &url,
                "tasks/cancel",
                json!({"id":pending["result"]["id"]})
            )
            .await["result"]["status"]["state"],
            "canceled"
        );
        let unknown_params = json!({"message":{"kind":"message","messageId":"unknown-fixture","role":"user",
            "parts":[{"kind":"text","text":"Owned reconciliation stream fixture"}]},
            "metadata":{EXTENSION:{"profileId":"fixture"}}});
        let unknown = call(&client, &url, "message/send", unknown_params).await;
        let unknown_id = unknown["result"]["id"].as_str().unwrap().to_owned();
        b.transaction(|r| {
            let mut t = r.tasks[&unknown_id].clone();
            t.status = Status::Running;
            t.prompt_dispatched = true;
            t.revision += 1;
            r.publish(t)
        })
        .unwrap();
        let mut subscription = client
            .post(&url)
            .json(&request("tasks/resubscribe", json!({"id":unknown_id})))
            .send()
            .await
            .unwrap();
        let mut initial_bytes = Vec::new();
        while !String::from_utf8_lossy(&initial_bytes).contains("\"outcomeUnknown\":false") {
            initial_bytes.extend_from_slice(&subscription.chunk().await.unwrap().unwrap());
        }
        b.transaction(|r| {
            let mut t = r.tasks[&unknown_id].clone();
            t.status = Status::ReconciliationRequired;
            t.cancel_requested = true;
            t.detail = "Fixture execution outcome requires reconciliation".into();
            t.revision += 1;
            r.publish(t)
        })
        .unwrap();
        let remaining = subscription.text().await.unwrap();
        let final_status: Value = remaining
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
            .find(|event| {
                event["result"]["kind"] == "status-update" && event["result"]["final"] == true
            })
            .expect("terminal SSE status");
        assert_eq!(final_status["result"]["status"]["state"], "failed");
        assert_eq!(
            final_status["result"]["metadata"][EXTENSION]["outcomeUnknown"],
            true
        );
        assert_eq!(
            final_status["result"]["metadata"][EXTENSION]["cancelRequested"],
            true
        );
        b.revoke(&a.state.lock().unwrap().node_id).unwrap();
        assert!(client
            .post(&url)
            .json(&request("tasks/get", json!({"id":task_id})))
            .send()
            .await
            .is_err());
        b.shutdown();
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop((a, b, stranger, client, unowned));
        std::fs::remove_dir_all(root).unwrap();
    }
}
