//! Darbot Fleet/1: durable task exchange with a pinned A2A 0.3 text-task profile.
//! Only explicitly submitted tasks cross the wire. Pairing never authorizes execution.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;

mod a2a;
#[cfg(test)]
mod process_tests;
pub mod runtime;
mod store;
#[cfg(test)]
mod tests;
mod transport;
pub use runtime::FleetAction;
pub use runtime::{fleet_dispatch, FleetState};

pub const VERSION: u32 = 1;
const MAX_PEERS: usize = 16;
const MAX_TASKS: usize = 512;
const MAX_QUEUE: usize = 1024;
const MAX_OUTPUT: usize = 128 * 1024;
const PRESENCE_MS: u64 = 20_000;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FleetError {
    pub code: &'static str,
    pub message: String,
}
type Result<T> = std::result::Result<T, FleetError>;
fn fail(code: &'static str, message: impl Into<String>) -> FleetError {
    FleetError {
        code,
        message: message.into(),
    }
}
fn io(error: std::io::Error) -> FleetError {
    fail("storage", error.to_string())
}
fn id() -> String {
    format!("{:032x}", rand::random::<u128>())
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn fingerprint(cert: &[u8]) -> String {
    format!("{:x}", Sha256::digest(cert))
}

#[derive(Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[derive(PartialEq, Eq)]
pub struct PairingCard {
    pub version: u32,
    pub node_id: String,
    pub label: String,
    pub endpoint: String,
    pub certificate: Vec<u8>,
}
impl PairingCard {
    fn validate(&self) -> Result<()> {
        if self.version != VERSION
            || self.node_id != fingerprint(&self.certificate)
            || self.certificate.len() > 8192
            || self.label.is_empty()
            || self.label.len() > 100
            || self.label.chars().any(char::is_control)
        {
            return Err(fail("invalid_pairing", "Invalid Fleet/1 identity card"));
        }
        self.endpoint.parse::<std::net::SocketAddr>().map_err(|_| {
            fail(
                "invalid_endpoint",
                "Use an explicit IP:port (IPv6 in brackets)",
            )
        })?;
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Profile {
    pub id: String,
    pub agent: String,
    pub model: String,
    pub root: String,
    pub permission_profile: String,
}
impl Profile {
    fn validate(&self) -> Result<()> {
        if self.id.is_empty()
            || self.id.len() > 80
            || self.id.chars().any(char::is_control)
            || self.agent.len() > 128
            || (!self.agent.is_empty() && !self.agent.as_bytes()[0].is_ascii_alphanumeric())
            || self.model.is_empty()
            || self.model.len() > 128
            || self.model.chars().any(char::is_control)
            || self.root.len() > 1024
            || self.permission_profile != "ask-every-tool"
            || !self
                .agent
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        {
            return Err(fail("invalid_profile", "Use a bounded profile, local agent ID, explicit CLI model and ask-every-tool permissions"));
        }
        let root = Path::new(&self.root);
        if !root.is_absolute() || !root.is_dir() {
            return Err(fail(
                "invalid_root",
                "Choose an existing absolute repository root",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Queued,
    InputRequired,
    Running,
    Completed,
    Failed,
    Cancelled,
    ReconciliationRequired,
}
impl Status {
    fn terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::ReconciliationRequired
        )
    }
}
#[derive(Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Target {
    pub node_id: String,
    pub profile_id: String,
}
#[derive(Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Submission {
    pub request_id: String,
    pub prompt: String,
    pub targets: Vec<Target>,
    pub expires_at: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[derive(PartialEq, Eq)]
pub struct Artifact {
    pub name: String,
    pub media_type: String,
    pub sha256: String,
    pub bytes: usize,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[derive(PartialEq, Eq)]
pub struct Task {
    pub id: String,
    pub request_id: String,
    pub origin: String,
    pub executor: String,
    pub profile: Profile,
    pub prompt: String,
    pub expires_at: u64,
    pub status: Status,
    pub revision: u64,
    pub session_id: Option<String>,
    pub output: String,
    pub detail: String,
    pub artifacts: Vec<Artifact>,
    pub cancel_requested: bool,
    pub prompt_dispatched: bool,
    #[serde(default)]
    pub a2a: bool,
    #[serde(default)]
    pub messages: Vec<TaskMessage>,
}
#[derive(Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[derive(PartialEq, Eq)]
pub struct TaskMessage {
    pub id: String,
    pub author: String,
    pub text: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
#[derive(PartialEq, Eq)]
enum Event {
    Submit(Task),
    Update(Task),
    Cancel {
        task_id: String,
    },
    Message {
        task_id: String,
        message: TaskMessage,
    },
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[derive(PartialEq, Eq)]
struct Envelope {
    id: String,
    origin: String,
    destination: String,
    sequence: u64,
    event: Event,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[derive(PartialEq, Eq)]
struct Peer {
    card: PairingCard,
    next_sequence: u64,
    received: u64,
    acknowledged: u64,
    outbox: VecDeque<Envelope>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[derive(PartialEq, Eq)]
struct Record {
    version: u32,
    node_id: String,
    certificate: Vec<u8>,
    private_key: Vec<u8>,
    enabled: bool,
    listen: String,
    label: String,
    peers: BTreeMap<String, Peer>,
    profiles: Vec<Profile>,
    tasks: BTreeMap<String, Task>,
    round_robin: usize,
    #[serde(default)]
    published: BTreeMap<String, u64>,
    #[serde(default)]
    update_hint: Option<crate::desktop_updates::UpdateHint>,
    #[serde(default)]
    advertised_updates: BTreeMap<String, crate::desktop_updates::UpdateHint>,
}
impl Record {
    fn new() -> Result<Self> {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["darbot-fleet".into()])
                .map_err(|e| fail("identity", e.to_string()))?;
        let certificate = cert.der().to_vec();
        Ok(Self {
            version: VERSION,
            node_id: fingerprint(&certificate),
            certificate,
            private_key: signing_key.serialize_der(),
            enabled: false,
            listen: "127.0.0.1:0".into(),
            label: "My Darbot".into(),
            peers: BTreeMap::new(),
            profiles: vec![],
            tasks: BTreeMap::new(),
            round_robin: 0,
            published: BTreeMap::new(),
            update_hint: None,
            advertised_updates: BTreeMap::new(),
        })
    }
    fn validate(&self) -> Result<()> {
        if self.advertised_updates.len() > MAX_PEERS
            || self
                .update_hint
                .iter()
                .chain(self.advertised_updates.values())
                .any(|h| h.validate().is_err())
        {
            return Err(fail("corrupt_store", "Invalid release hint"));
        }
        if self.version != VERSION
            || self.node_id != fingerprint(&self.certificate)
            || self.peers.len() > MAX_PEERS
            || self.tasks.len() > MAX_TASKS
        {
            return Err(fail(
                "corrupt_store",
                "Unsupported or inconsistent fleet store; restore it, do not reset trust silently",
            ));
        }
        for (id, peer) in &self.peers {
            peer.card.validate()?;
            if id != &peer.card.node_id
                || peer.outbox.len() > MAX_QUEUE
                || peer.acknowledged >= peer.next_sequence
            {
                return Err(fail("corrupt_store", "Invalid peer sequence state"));
            }
        }
        transport::identity(self)?;
        Ok(())
    }
    fn card(&self) -> PairingCard {
        PairingCard {
            version: VERSION,
            node_id: self.node_id.clone(),
            label: self.label.clone(),
            endpoint: self.listen.clone(),
            certificate: self.certificate.clone(),
        }
    }
    fn enqueue(&mut self, peer_id: &str, event: Event) -> Result<()> {
        if peer_id == self.node_id {
            return Ok(());
        }
        let peer = self
            .peers
            .get_mut(peer_id)
            .ok_or_else(|| fail("revoked", "Peer is no longer paired"))?;
        if peer.outbox.len() >= MAX_QUEUE {
            return Err(fail("queue_full", "Durable peer queue is full"));
        }
        peer.outbox.push_back(Envelope {
            id: id(),
            origin: self.node_id.clone(),
            destination: peer_id.into(),
            sequence: peer.next_sequence,
            event,
        });
        peer.next_sequence = peer
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| fail("sequence", "Sequence exhausted"))?;
        Ok(())
    }
    fn publish(&mut self, task: Task) -> Result<()> {
        self.tasks.insert(task.id.clone(), task);
        self.flush_updates()
    }
    // Never rewrite an envelope which may already have been received. Keep one immutable
    // snapshot in flight per task, then send the newest durable revision after its ACK.
    fn flush_updates(&mut self) -> Result<()> {
        let pending: Vec<_> = self
            .tasks
            .values()
            .filter(|t| {
                !t.a2a
                    && t.executor == self.node_id
                    && t.origin != self.node_id
                    && t.revision > *self.published.get(&t.id).unwrap_or(&0)
                    && self.peers.get(&t.origin).is_some_and(|p| {
                        !p.outbox
                            .iter()
                            .any(|e| matches!(&e.event, Event::Update(old) if old.id == t.id))
                    })
            })
            .cloned()
            .collect();
        for task in pending {
            let mut snapshot = task.clone();
            // Notes have independent origin-bound envelopes and must not be replayed
            // inside every assistant-output snapshot.
            snapshot.messages.clear();
            self.enqueue(&task.origin, Event::Update(snapshot))?;
            self.published.insert(task.id, task.revision);
        }
        Ok(())
    }
}

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Presence {
    seen_at: u64,
    profiles: Vec<Profile>,
    busy: bool,
    error: Option<String>,
}

pub(super) struct Node {
    state: Mutex<Record>,
    store: store::Store,
    presence: Mutex<BTreeMap<String, Presence>>,
    stop: CancellationToken,
    execution: Mutex<Option<(String, CancellationToken)>>,
    failure: Mutex<Option<FleetError>>,
}
impl Node {
    fn open(root: &Path) -> Result<Arc<Self>> {
        let (store, state) = store::Store::open(root)?;
        Ok(Arc::new(Self {
            state: Mutex::new(state),
            store,
            presence: Mutex::default(),
            stop: CancellationToken::new(),
            execution: Mutex::default(),
            failure: Mutex::default(),
        }))
    }
    fn transaction<T>(&self, change: impl FnOnce(&mut Record) -> Result<T>) -> Result<T> {
        let mut current = self
            .state
            .lock()
            .map_err(|_| fail("storage", "Fleet state lock failed"))?;
        let mut next = current.clone();
        let result = change(&mut next)?;
        if next == *current {
            return Ok(result);
        }
        self.store.save(&next)?;
        *current = next;
        Ok(result)
    }
    fn pair(&self, card: PairingCard) -> Result<()> {
        card.validate()?;
        self.transaction(|record| {
            if card.node_id == record.node_id
                || record.peers.contains_key(&card.node_id)
                || record.peers.len() >= MAX_PEERS
            {
                return Err(fail("invalid_pairing", "Self, duplicate or excessive peer"));
            }
            record.peers.insert(
                card.node_id.clone(),
                Peer {
                    card,
                    next_sequence: 1,
                    received: 0,
                    acknowledged: 0,
                    outbox: VecDeque::new(),
                },
            );
            Ok(())
        })
    }
    fn revoke(&self, peer: &str) -> Result<()> {
        self.transaction(|r| {
            r.peers
                .remove(peer)
                .ok_or_else(|| fail("not_found", "Peer not found"))?;
            for t in r
                .tasks
                .values_mut()
                .filter(|t| (t.origin == peer || t.executor == peer) && !t.status.terminal())
            {
                t.cancel_requested = true;
                t.status = Status::ReconciliationRequired;
                t.detail = "Peer revoked. Remote execution outcome requires reconciliation.".into();
                t.revision += 1;
            }
            Ok(())
        })?;
        self.presence.lock().unwrap().remove(peer);
        self.cancel_revoked_execution();
        Ok(())
    }
    fn cancel_revoked_execution(&self) {
        if let Some((task_id, stop)) = self.execution.lock().unwrap().as_ref() {
            if self
                .state
                .lock()
                .unwrap()
                .tasks
                .get(task_id)
                .is_some_and(|t| t.cancel_requested)
            {
                stop.cancel();
            }
        }
    }
    fn submit(&self, request: Submission) -> Result<Task> {
        if request.request_id.is_empty()
            || request.request_id.len() > 100
            || request.prompt.trim().is_empty()
            || request.prompt.chars().count() > 2000
            || request.prompt.lines().count() > 24
            || request.prompt.trim_start().starts_with('/')
            || request.targets.is_empty()
            || request.targets.len() > 64
            || request.expires_at <= now()
            || request.expires_at > now() + 24 * 60 * 60 * 1000
        {
            return Err(fail("invalid_task", "Bound prompt to 2000 characters/24 lines and expiry to the next 24 hours; select targets"));
        }
        let presence = self.presence.lock().unwrap().clone();
        self.transaction(|r| {
            if !r.enabled {
                return Err(fail("disabled", "Fleet is disabled"));
            }
            if let Some(t) = r
                .tasks
                .values()
                .find(|t| t.origin == r.node_id && t.request_id == request.request_id)
            {
                if t.prompt != request.prompt {
                    return Err(fail("duplicate", "Request ID was already used"));
                }
                return Ok(t.clone());
            }
            if r.tasks.len() >= MAX_TASKS {
                return Err(fail("queue_full", "Task history is full"));
            }
            let mut selected = None;
            for offset in 0..request.targets.len() {
                let index = (r.round_robin + offset) % request.targets.len();
                let target = &request.targets[index];
                if r.tasks
                    .values()
                    .any(|t| t.executor == target.node_id && !t.status.terminal())
                {
                    continue;
                }
                let profile = if target.node_id == r.node_id {
                    r.profiles
                        .iter()
                        .find(|p| p.id == target.profile_id)
                        .cloned()
                } else if r.peers.contains_key(&target.node_id) {
                    presence
                        .get(&target.node_id)
                        .filter(|p| now().saturating_sub(p.seen_at) < PRESENCE_MS && !p.busy)
                        .and_then(|p| p.profiles.iter().find(|p| p.id == target.profile_id))
                        .cloned()
                } else {
                    None
                };
                if let Some(profile) = profile {
                    selected = Some((index, target.node_id.clone(), profile));
                    break;
                }
            }
            let (index, executor, profile) = selected
                .ok_or_else(|| fail("no_capacity", "No online, idle, approved target profile"))?;
            r.round_robin = (index + 1) % request.targets.len();
            let task = Task {
                id: id(),
                request_id: request.request_id,
                origin: r.node_id.clone(),
                executor: executor.clone(),
                profile,
                prompt: request.prompt,
                expires_at: request.expires_at,
                status: Status::Queued,
                revision: 0,
                session_id: None,
                output: String::new(),
                detail: String::new(),
                artifacts: vec![],
                cancel_requested: false,
                prompt_dispatched: false,
                a2a: false,
                messages: vec![],
            };
            r.enqueue(&executor, Event::Submit(task.clone()))?;
            r.tasks.insert(task.id.clone(), task.clone());
            Ok(task)
        })
    }
    fn cancel(&self, task_id: &str) -> Result<()> {
        self.transaction(|r| {
            let mut task = r
                .tasks
                .get(task_id)
                .cloned()
                .ok_or_else(|| fail("not_found", "Task not found"))?;
            if task.origin != r.node_id {
                return Err(fail(
                    "forbidden",
                    "Only the originating node may request cancellation",
                ));
            }
            if task.status.terminal() {
                return Ok(());
            }
            task.cancel_requested = true;
            r.enqueue(
                &task.executor,
                Event::Cancel {
                    task_id: task.id.clone(),
                },
            )?;
            r.tasks.insert(task.id.clone(), task);
            Ok(())
        })?;
        self.cancel_revoked_execution();
        Ok(())
    }
    fn message(&self, task_id: &str, message_id: String, text: String) -> Result<()> {
        if message_id.is_empty()
            || message_id.len() > 100
            || text.trim().is_empty()
            || text.chars().count() > 2000
        {
            return Err(fail(
                "invalid_message",
                "Use a unique message ID and 1–2000 characters",
            ));
        }
        self.transaction(|r| {
            let mut task = r
                .tasks
                .get(task_id)
                .cloned()
                .ok_or_else(|| fail("not_found", "Task not found"))?;
            if task.a2a {
                return Err(fail(
                    "unsupported",
                    "A2A tasks expose assistant results, not private operator notes",
                ));
            }
            if let Some(existing) = task
                .messages
                .iter()
                .find(|m| m.author == r.node_id && m.id == message_id)
            {
                return if existing.text == text {
                    Ok(())
                } else {
                    Err(fail("duplicate", "Message ID conflict"))
                };
            }
            if task
                .messages
                .iter()
                .filter(|m| m.author == r.node_id)
                .count()
                >= 16
            {
                return Err(fail(
                    "queue_full",
                    "Task has reached this author's 16-note limit",
                ));
            }
            let message = TaskMessage {
                id: message_id,
                author: r.node_id.clone(),
                text,
            };
            let destination = if task.executor == r.node_id {
                &task.origin
            } else {
                &task.executor
            };
            r.enqueue(
                destination,
                Event::Message {
                    task_id: task.id.clone(),
                    message: message.clone(),
                },
            )?;
            task.messages.push(message);
            r.tasks.insert(task.id.clone(), task);
            Ok(())
        })
    }
    fn maintain(&self) -> Result<()> {
        self.transaction(|r| {
            let tasks: Vec<_> = r.tasks.values().filter(|t| !t.status.terminal()).cloned().collect();
            for mut t in tasks {
                if t.origin == r.node_id && t.executor != r.node_id && t.expires_at <= now() {
                    // Only the executor advances its revision. A late authenticated terminal
                    // result can reconcile this local uncertainty, but never starts a retry.
                    t.status = Status::ReconciliationRequired;
                    t.detail = "Remote execution lease expired without a terminal result; no automatic reassignment.".into();
                    r.tasks.insert(t.id.clone(), t);
                } else if t.executor == r.node_id && t.session_id.is_none()
                    && matches!(t.status, Status::Queued | Status::InputRequired) {
                    let status = if t.cancel_requested { Status::Cancelled }
                        else if t.expires_at <= now() { Status::Failed }
                        else { Status::InputRequired };
                    if status != t.status {
                        t.status = status;
                        t.revision += 1;
                        t.detail = if t.status == Status::InputRequired {
                            "Awaiting local execution approval"
                        } else { "Cancelled or expired before execution" }.into();
                        r.publish(t)?;
                    }
                }
            }
            r.flush_updates()
        })
    }
    fn shutdown(&self) {
        self.stop.cancel();
        if let Some((_, cancel)) = self.execution.lock().unwrap().as_ref() {
            cancel.cancel();
        }
    }
}
