//! Read-only local archive census. These keys are evidence of history, not runtime handles.
//! `indexed` is source-local unique keys, `duplicates` is repeat archive observations
//! (including earlier pools), and `unattributed` is source-local missing attribution.
//! `unreadable` counts rejected entries/records or inaccessible stores, not lost sessions.
//! Globally unknown sessions have an empty `agentIds`, never a fabricated agent identifier.
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use tauri::webview::cookie::time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tokio::sync::watch;

const CLI: &str = "copilot-cli";
const CLAUDE: &str = "claude-code";
const VSC: &str = "vscode-chat";
const CODEX: &str = "codex";
const CLIPPY: &str = "clippyclaw";
const STUDIO: &str = "agentstudio";
const RECORD_BYTES: usize = 4 << 20;
const DOCUMENT_BYTES: usize = 16 << 20;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryIndex {
    schema_version: u8,
    scanned_at: String,
    sources: Vec<Source>,
    sessions: Vec<Session>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Session {
    id: String,
    provider: &'static str,
    native_id: String,
    agent_ids: BTreeSet<String>,
    source_ids: BTreeSet<&'static str>,
    updated_at: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Source {
    id: &'static str,
    label: &'static str,
    indexed: usize,
    duplicates: usize,
    unreadable: usize,
    unattributed: usize,
    complete: bool,
    status: String,
}

type Snapshot = Result<Arc<HistoryIndex>, &'static str>;
type Flight = watch::Receiver<Option<Snapshot>>;

#[derive(Default)]
pub struct IndexCache(Mutex<Option<Flight>>);

impl IndexCache {
    fn flight(
        &self,
        refresh: bool,
        scan: impl FnOnce() -> HistoryIndex + Send + 'static,
    ) -> Flight {
        let mut slot = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(rx) = slot.as_ref() {
            let result = rx.borrow();
            if result.is_none() || (!refresh && matches!(*result, Some(Ok(_)))) {
                return rx.clone();
            }
        }
        let (tx, rx) = watch::channel(None);
        *slot = Some(rx.clone());
        // The producer belongs to the flight, not the first (possibly cancelled) invocation.
        tauri::async_runtime::spawn(async move {
            let result = tauri::async_runtime::spawn_blocking(scan)
                .await
                .map(Arc::new)
                .map_err(|_| "Local archive scan did not finish");
            tx.send_replace(Some(result));
        });
        rx
    }

    pub async fn index(&self, refresh: bool) -> Result<HistoryIndex, &'static str> {
        let mut rx = self.flight(refresh, live_scan);
        let value = rx
            .wait_for(Option::is_some)
            .await
            .map_err(|_| "Local archive scan stopped")?;
        value
            .as_ref()
            .unwrap()
            .as_ref()
            .map(|v| (**v).clone())
            .map_err(|e| *e)
    }
}

fn token(s: &str, namespace: bool) -> bool {
    (1..=160).contains(&s.len())
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b) || namespace && b == b':')
}

fn uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

fn native(s: &str) -> Option<String> {
    token(s, false).then(|| {
        if uuid(s) {
            s.to_ascii_lowercase()
        } else {
            s.to_owned()
        }
    })
}

#[derive(Default)]
struct Evidence {
    agents: BTreeSet<String>,
    updated: Option<OffsetDateTime>,
}

impl Evidence {
    fn agent(&mut self, value: &str, cli: bool) -> bool {
        // This is the CLI's rendered-agent namespace, not a generic "strip the prefix" rule.
        let value = if cli {
            value
                .split_once(':')
                .filter(|(ns, _)| {
                    ns.strip_prefix("agency-rendered-")
                        .is_some_and(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
                })
                .map_or(value, |(_, agent)| agent)
        } else {
            value
        };
        if !token(value, true) {
            return false;
        }
        self.agents.insert(value.to_owned());
        true
    }

    fn date(&mut self, s: &str) {
        if s.len() <= 40 {
            if let Ok(t) = OffsetDateTime::parse(s, &Rfc3339) {
                self.updated = self.updated.max(Some(t));
            }
        }
    }

    fn millis(&mut self, n: i64) {
        if let Ok(t) = OffsetDateTime::from_unix_timestamp_nanos(i128::from(n) * 1_000_000) {
            self.updated = self.updated.max(Some(t));
        }
    }

    fn merge(&mut self, other: Self) {
        self.agents.extend(other.agents);
        self.updated = self.updated.max(other.updated);
    }
}

type Key = (&'static str, String);

#[derive(Deserialize)]
struct Workspace {
    id: String,
    updated_at: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CliEvent<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    #[serde(borrow)]
    data: &'a serde_json::value::RawValue,
    timestamp: Option<&'a str>,
}

fn text<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)?.as_str()
}

fn cli_session(p: &mut Pool, dir: &Arc<Dir>) {
    let mut workspace = None;
    let mut evidence = Evidence::default();
    if let Some(f) = p.file(dir, "workspace.yaml", true) {
        if let Some(bytes) = p.bytes(f, 128 << 10) {
            if let Some(w) = serde_yaml_ng::from_slice::<Workspace>(&bytes)
                .ok()
                .filter(|w| native(&w.id).is_some())
            {
                workspace = native(&w.id);
                if let Some(t) = w.updated_at {
                    evidence.date(&t);
                }
            } else {
                p.error("malformed workspace metadata");
            }
        }
    }

    let mut event_id = None;
    let mut conflict = false;
    let mut journal = Evidence::default();
    if let Some(f) = p.file(dir, "events.jsonl", true) {
        p.lines(f, |p, line| {
            let Ok(event) = serde_json::from_slice::<CliEvent>(line) else {
                p.error("malformed CLI event");
                return;
            };
            if event_id.is_none() && event.kind != "session.start" {
                p.reasons.insert("events before session identity");
                return;
            }
            if let Some(t) = event.timestamp {
                journal.date(t);
            }
            match event.kind {
                "session.start" | "subagent.selected" | "subagent.started"
                | "subagent.completed" | "subagent.failed" | "session.shutdown" => {
                    let Ok(data) = serde_json::from_str::<Value>(event.data.get()) else {
                        p.error("malformed CLI metadata");
                        return;
                    };
                    if event.kind == "session.start" {
                        let Some(id) = text(&data, "sessionId").and_then(native) else {
                            conflict = true;
                            return;
                        };
                        if event_id.as_ref().is_some_and(|prev| prev != &id) {
                            conflict = true;
                        }
                        event_id = Some(id);
                    } else if event.kind == "session.shutdown" {
                        if let Some(metrics) = data["agentMetrics"].as_object() {
                            for metric in metrics.values() {
                                p.add_agent(&mut journal, text(metric, "agentName"), true);
                            }
                        }
                    } else if let Some(name) = text(&data, "agentName") {
                        p.add_agent(&mut journal, Some(name), true);
                    } else {
                        p.error("missing CLI agent identifier");
                    }
                }
                "session.truncation" => {
                    p.reasons.insert("truncation recorded");
                }
                _ => (),
            }
        });
        if event_id.is_none() {
            p.error("journal lacks session start");
        }
    } else {
        p.reasons.insert("missing event journal");
    }
    // A migrated directory can contain stale metadata. Never move agent evidence across IDs.
    if conflict || matches!((&workspace,&event_id), (Some(a),Some(b)) if a != b) {
        p.error("conflicting CLI identity");
        return;
    }
    if event_id.is_some() {
        evidence.merge(journal);
    }
    if let Some(id) = event_id.or(workspace) {
        p.record((CLI, id), evidence);
    } else {
        p.error("missing CLI identity");
    }
    p.sqlite(dir, "session.db");
}

fn cli(p: &mut Pool, base: &Arc<Dir>) {
    p.sqlite(base, "session-store.db");
    p.sqlite(base, "data.db");
    if let Some(root) = p.dir(base, "session-state", false) {
        p.list(&root, |p, name, kind| {
            if kind.is_dir() || kind.is_symlink() {
                if let Some(d) = p.dir(&root, name, false) {
                    cli_session(p, &d);
                }
            }
        });
    }
}

fn app_row(p: &mut Pool, provider: &'static str, row: &Value) {
    let Some(id) = text(row, "id").and_then(native) else {
        p.error("invalid app session ID");
        return;
    };
    let mut e = Evidence::default();
    if let Some(t) = text(row, "updatedAt") {
        e.date(t);
    }
    p.add_agent(&mut e, text(row, "agentId"), false);
    // Local app IDs are not SDK IDs. A gateway/other backend, a clone's externalSession
    // and a similarly named workspace do not establish that this is a Copilot CLI session.
    let local = (provider, id);
    if matches!(text(row, "backendOrigin"), None | Some("copilot")) {
        let links = [
            text(row, "sdkSessionId"),
            (text(row, "backendOrigin") == Some("copilot"))
                .then(|| text(row, "backendId"))
                .flatten(),
        ];
        for id in links.into_iter().flatten() {
            if let Some(id) = native(id) {
                p.links.entry(local.clone()).or_default().insert((CLI, id));
            } else {
                p.error("invalid backend ID");
            }
        }
    }
    p.record(local, e);
}

// A session *index* is not one record. Stream its array; reset the byte allowance at
// each element. In particular, never deserialize NativeStore's settings or message maps.
struct IndexRows<'a> {
    p: &'a mut Pool,
    provider: &'static str,
    allowance: &'a Cell<usize>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct IndexMeta {
    id: Option<String>,
    session_id: Option<String>,
    modified: Option<String>,
    agent_id: Option<String>,
    updated_at: Option<String>,
    backend_origin: Option<String>,
    backend_id: Option<String>,
    sdk_session_id: Option<String>,
}

impl<'de> Visitor<'de> for IndexRows<'_> {
    type Value = ();
    fn expecting(&self, out: &mut std::fmt::Formatter) -> std::fmt::Result {
        out.write_str("session index")
    }
    fn visit_map<A: MapAccess<'de>>(mut self, mut map: A) -> Result<(), A::Error> {
        let target = if self.provider == CLAUDE {
            "entries"
        } else {
            "sessions"
        };
        let mut found = false;
        loop {
            self.allowance.set(RECORD_BYTES);
            let Some(key) = map.next_key::<String>()? else {
                break;
            };
            if key == target {
                if found {
                    return Err(serde::de::Error::custom("duplicate index"));
                }
                map.next_value_seed(&mut self)?;
                found = true;
            } else {
                self.allowance.set(DOCUMENT_BYTES);
                map.next_value::<IgnoredAny>()?;
            }
        }
        if !found {
            return Err(serde::de::Error::custom("missing index"));
        }
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        loop {
            self.allowance.set(RECORD_BYTES);
            let Some(row) = seq.next_element::<Box<serde_json::value::RawValue>>()? else {
                break;
            };
            match serde_json::from_str::<IndexMeta>(row.get()) {
                Ok(row) if self.provider == CLAUDE => {
                    if let Some(id) = row.session_id.as_deref().and_then(native) {
                        let mut e = Evidence::default();
                        if let Some(t) = row.modified {
                            e.date(&t);
                        }
                        self.p.record((CLAUDE, id), e);
                    } else {
                        self.p.error("invalid Claude index ID");
                    }
                }
                Ok(row) => app_row(self.p, self.provider, &serde_json::to_value(row).unwrap()),
                Err(_) => self.p.error("invalid session index row"),
            }
        }
        Ok(())
    }
}

impl<'de> DeserializeSeed<'de> for &mut IndexRows<'_> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_seq(IndexRows {
            p: self.p,
            provider: self.provider,
            allowance: self.allowance,
        })
    }
}

fn index_doc(p: &mut Pool, provider: &'static str, f: File) {
    struct Meter<'a>(BufReader<File>, &'a Cell<usize>);
    impl Read for Meter<'_> {
        fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
            let n = dst.len().min(self.1.get());
            if n == 0 {
                return Err(io::Error::other("oversize index record"));
            }
            let n = self.0.read(&mut dst[..n])?;
            self.1.set(self.1.get() - n);
            Ok(n)
        }
    }
    let remaining = Cell::new(RECORD_BYTES);
    let mut de = serde_json::Deserializer::from_reader(Meter(BufReader::new(f), &remaining));
    let rows = IndexRows {
        p,
        provider,
        allowance: &remaining,
    };
    if (&mut de)
        .deserialize_map(rows)
        .and_then(|_| de.end())
        .is_err()
    {
        p.error(if remaining.get() == 0 {
            "oversize index record"
        } else {
            "encrypted or malformed index"
        });
    }
}

fn app_dir(p: &mut Pool, provider: &'static str, dir: &Arc<Dir>) {
    p.list(dir, |p, name, _| {
        let Some(stem) = name.strip_suffix(".json") else {
            if name.ends_with(".enc") {
                p.error("encrypted app archive");
            }
            return;
        };
        let index = stem == "sessions-index";
        if !index && !uuid(stem) {
            // Unknown filenames are not an invitation to read arbitrary JSON.
            p.error("unrecognized app archive entry");
            return;
        }
        if let Some(f) = p.file(dir, name, false) {
            if index {
                index_doc(p, provider, f);
            } else if let Some(doc) = p.json(f) {
                app_row(p, provider, &doc);
            }
        }
    });
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeRow {
    #[serde(rename = "type")]
    kind: Option<String>,
    session_id: Option<String>,
    agent_id: Option<String>,
    timestamp: Option<String>,
    // Only assistant tool_use blocks are examined. Never use user tool inputs or tool results.
    message: Option<ClaudeMessage>,
}

#[derive(Deserialize)]
struct ClaudeMessage {
    content: Option<Box<serde_json::value::RawValue>>,
}

fn claude_log(p: &mut Pool, f: File) {
    let mut rows = BTreeMap::<String, Evidence>::new();
    p.lines(f, |p, line| {
        let Ok(r) = serde_json::from_slice::<ClaudeRow>(line) else {
            p.error("malformed Claude record");
            return;
        };
        // Some auxiliary records (e.g. file-history-snapshot) carry no session identity.
        let Some(s) = r.session_id else { return };
        let Some(id) = native(&s) else {
            p.error("invalid Claude session ID");
            return;
        };
        let e = rows.entry(id).or_default();
        if let Some(t) = r.timestamp {
            e.date(&t);
        }
        if matches!(
            r.kind.as_deref(),
            Some("assistant" | "user" | "system" | "progress")
        ) {
            p.add_agent(e, r.agent_id.as_deref(), false);
        }
        if r.kind.as_deref() == Some("assistant") {
            if let Some(raw) = r.message.and_then(|m| m.content) {
                #[derive(Deserialize)]
                struct Block {
                    #[serde(rename = "type")]
                    kind: String,
                    name: Option<String>,
                    input: Option<Box<serde_json::value::RawValue>>,
                }
                if let Ok(blocks) = serde_json::from_str::<Vec<Block>>(raw.get()) {
                    for b in blocks {
                        if b.kind == "tool_use"
                            && matches!(b.name.as_deref(), Some("Agent" | "Task"))
                        {
                            if let Some(input) = b
                                .input
                                .and_then(|s| serde_json::from_str::<Value>(s.get()).ok())
                            {
                                p.add_agent(e, text(&input, "subagent_type"), false);
                            }
                        }
                    }
                }
            }
        }
    });
    if rows.is_empty() {
        p.error("missing Claude session ID");
    }
    for (id, e) in rows {
        p.record((CLAUDE, id), e);
    }
}

fn claude_files(p: &mut Pool, dir: &Arc<Dir>, sub: bool) {
    p.list(dir, |p, name, kind| {
        if name == "sessions-index.json" && !sub {
            if let Some(f) = p.file(dir, name, false) {
                index_doc(p, CLAUDE, f);
            }
        } else if name
            .strip_suffix(".jsonl")
            .is_some_and(|s| uuid(s) || s.strip_prefix("agent-").is_some_and(|x| token(x, false)))
        {
            if let Some(f) = p.file(dir, name, false) {
                claude_log(p, f);
            }
        } else if !sub && uuid(name) && (kind.is_dir() || kind.is_symlink()) {
            if let Some(d) = p
                .dir(dir, name, false)
                .and_then(|d| p.dir(&d, "subagents", true))
            {
                claude_files(p, &d, true);
            }
        } else if name.ends_with(".jsonl") {
            p.error("unrecognized Claude archive");
        }
    });
}

fn claude(p: &mut Pool, base: &Arc<Dir>) {
    if let Some(projects) = p.dir(base, "projects", false) {
        p.list(&projects, |p, name, kind| {
            if kind.is_dir() || kind.is_symlink() {
                if let Some(d) = p.dir(&projects, name, false) {
                    claude_files(p, &d, false);
                }
            }
        });
    }
    if let Some(f) = p.file(base, "history.jsonl", true) {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Entry {
            session_id: Option<String>,
            timestamp: Option<i64>,
        }
        let mut rows = BTreeMap::<String, Evidence>::new();
        p.lines(f, |p, line| match serde_json::from_slice::<Entry>(line) {
            Ok(r) if r.session_id.as_deref().and_then(native).is_some() => {
                let e = rows
                    .entry(native(r.session_id.as_deref().unwrap()).unwrap())
                    .or_default();
                if let Some(t) = r.timestamp {
                    e.millis(t);
                }
            }
            _ => p.error("Claude history lacks valid ID"),
        });
        for (id, e) in rows {
            p.record((CLAUDE, id), e);
        }
    }
}

fn codex_log(p: &mut Pool, f: File) {
    let mut current = None;
    let mut evidence = Evidence::default();
    let mut conflict = false;
    #[derive(Deserialize)]
    struct Row<'a> {
        #[serde(rename = "type")]
        kind: &'a str,
        #[serde(borrow)]
        payload: &'a serde_json::value::RawValue,
        timestamp: Option<&'a str>,
    }
    p.lines(f, |p, line| {
        let Ok(r) = serde_json::from_slice::<Row>(line) else {
            p.error("malformed Codex record");
            return;
        };
        if let Some(t) = r.timestamp {
            evidence.date(t);
        }
        if r.kind == "session_meta" {
            let Ok(meta) = serde_json::from_str::<Value>(r.payload.get()) else {
                p.error("malformed Codex metadata");
                return;
            };
            if let Some(id) = text(&meta, "id").and_then(native) {
                if current.as_ref().is_some_and(|s| s != &id) {
                    conflict = true;
                }
                current = Some(id);
            } else {
                conflict = true;
            }
            for role in [
                text(&meta, "agent_role"),
                meta.pointer("/source/subagent/thread_spawn/agent_role")
                    .and_then(Value::as_str),
            ] {
                p.add_agent(&mut evidence, role, false);
            }
        }
    });
    if let Some(id) = current.filter(|_| !conflict) {
        p.record((CODEX, id), evidence);
    } else {
        p.error("missing or conflicting Codex identity");
    }
}

fn codex_files(p: &mut Pool, dir: &Arc<Dir>, depth: usize) {
    p.list(dir, |p, name, kind| {
        if name.starts_with("rollout-") && name.ends_with(".jsonl") {
            if let Some(f) = p.file(dir, name, false) {
                codex_log(p, f);
            }
        } else if depth < 3
            && name.len() == if depth == 0 { 4 } else { 2 }
            && name.bytes().all(|b| b.is_ascii_digit())
        {
            if let Some(d) = p.dir(dir, name, false) {
                codex_files(p, &d, depth + 1);
            }
        } else if name.ends_with(".jsonl") || kind.is_symlink() || kind.is_dir() {
            p.error("unsupported Codex archive layout");
        }
    });
}

fn codex_index(p: &mut Pool, base: &Dir) {
    for (name, field) in [
        ("session_index.jsonl", "id"),
        ("history.jsonl", "session_id"),
    ] {
        if let Some(f) = p.file(base, name, true) {
            let mut rows = BTreeMap::<String, Evidence>::new();
            p.lines(f, |p, line| {
                let Ok(row) = serde_json::from_slice::<Value>(line) else {
                    p.error("malformed Codex index");
                    return;
                };
                let Some(id) = text(&row, field).and_then(native) else {
                    p.error("invalid Codex index ID");
                    return;
                };
                let e = rows.entry(id).or_default();
                if let Some(t) = text(&row, "updated_at") {
                    e.date(t);
                }
                if let Some(t) = row["ts"].as_i64().and_then(|t| t.checked_mul(1000)) {
                    e.millis(t);
                }
            });
            for (id, e) in rows {
                p.record((CODEX, id), e);
            }
        }
    }
    p.sqlite(base, "state_5.sqlite");
    p.sqlite(base, "thread_history_1.sqlite");
}

fn vs_request(p: &mut Pool, e: &mut Evidence, req: &Value) {
    p.add_agent(e, req.pointer("/agent/id").and_then(Value::as_str), false);
    if let Some(t) = req["timestamp"].as_i64() {
        e.millis(t);
    }
}

fn vs_snapshot(p: &mut Pool, e: &mut Evidence, doc: &Value) -> Option<String> {
    let id = native(text(doc, "sessionId")?)?;
    for key in ["lastMessageDate", "creationDate"] {
        if let Some(t) = doc[key].as_i64() {
            e.millis(t);
        }
    }
    if let Some(rows) = doc["requests"].as_array() {
        for req in rows {
            vs_request(p, e, req);
        }
    } else {
        p.error("missing VS Code requests");
    }
    Some(id)
}

fn vs_files(p: &mut Pool, dir: &Arc<Dir>) {
    p.list(dir, |p, name, _| {
        let Some((stem, ext @ ("json" | "jsonl"))) = name.rsplit_once('.') else {
            return;
        };
        if !uuid(stem) {
            p.error("unrecognized VS Code archive entry");
            return;
        }
        let Some(f) = p.file(dir, name, false) else {
            return;
        };
        let mut e = Evidence::default();
        let mut id = None;
        if ext == "json" {
            if let Some(doc) = p.json(f) {
                id = vs_snapshot(p, &mut e, &doc);
            }
        } else {
            let mut conflict = false;
            p.lines(f, |p, line| {
                let Ok(doc) = serde_json::from_slice::<Value>(line) else {
                    p.error("malformed VS Code edit");
                    return;
                };
                let v = &doc["v"];
                match doc["kind"].as_u64() {
                    Some(0) => {
                        let new = vs_snapshot(p, &mut e, v);
                        if new.is_none() || id.is_some() && new != id {
                            conflict = true;
                        }
                        id = new;
                    }
                    Some(1..=3) if id.is_some() => {
                        let Some(path) = doc["k"].as_array() else {
                            p.error("invalid VS Code edit path");
                            return;
                        };
                        let keys = path.as_slice();
                        // Keep historical participation, even after a request is removed. Never
                        // recurse through tool results, message parts, inputState or response text.
                        match keys {
                            [field] if field == "sessionId" => {
                                if v.as_str().and_then(native) != id {
                                    conflict = true;
                                }
                            }
                            [field] if field == "requests" => {
                                if let Some(rows) = v.as_array() {
                                    for req in rows {
                                        vs_request(p, &mut e, req);
                                    }
                                }
                            }
                            [field, n] if field == "requests" && n.is_u64() => {
                                vs_request(p, &mut e, v)
                            }
                            [field, n, a] if field == "requests" && n.is_u64() && a == "agent" => {
                                p.add_agent(&mut e, text(v, "id"), false)
                            }
                            [field, n, a, k]
                                if field == "requests"
                                    && n.is_u64()
                                    && a == "agent"
                                    && k == "id" =>
                            {
                                p.add_agent(&mut e, v.as_str(), false)
                            }
                            [field] if field == "lastMessageDate" => {
                                if let Some(t) = v.as_i64() {
                                    e.millis(t);
                                }
                            }
                            _ => (),
                        }
                    }
                    _ => p.error("unsupported VS Code edit"),
                }
            });
            if conflict {
                id = None;
            }
        }
        if let Some(id) = id {
            p.record((VSC, id), e);
        } else {
            p.error("missing or conflicting VS Code identity");
        }
    });
}

fn vs_profile(p: &mut Pool, user: &Arc<Dir>) {
    if let Some(ws) = p.dir(user, "workspaceStorage", true) {
        p.list(&ws, |p, name, kind| {
            if kind.is_dir() || kind.is_symlink() {
                if let Some(chat) = p
                    .dir(&ws, name, false)
                    .and_then(|d| p.dir(&d, "chatSessions", true))
                {
                    vs_files(p, &chat);
                }
            }
        });
    }
    if let Some(gs) = p.dir(user, "globalStorage", true) {
        if let Some(chat) = p.dir(&gs, "emptyWindowChatSessions", true) {
            vs_files(p, &chat);
        }
        if let Some(chat) = p.dir(&gs, "github.copilot-chat", true) {
            p.sqlite(&chat, "session-store.db");
        }
    }
}

// The 3DKG Copilot inventory combines database IDs AND directory-only observations.
// Consume only session_id and the two database-provenance flags. Respect quoted
// fields/newlines: text elsewhere in a CSV must never manufacture a row.
fn references(p: &mut Pool, f: File) {
    let mut reader = BufReader::new(f);
    let mut header = true;
    let mut quoted = false;
    let mut post_quote = false;
    let mut field_start = true;
    let mut col = 0usize;
    let mut size = 0usize;
    let mut bad = false;
    let mut field = Vec::new();
    let (mut indexed, mut app, mut width) = (None, None, 0);
    let mut id = None;
    let mut backed = false;
    loop {
        let buf = match reader.fill_buf() {
            Ok(buf) => buf,
            Err(_) => {
                p.error("reference read failed");
                break;
            }
        };
        let at_eof = buf.is_empty();
        if at_eof && size == 0 {
            break;
        }
        let chunk = if at_eof { b"\n".as_slice() } else { buf };
        for &b in chunk {
            let capture = header || col == 0 || Some(col) == indexed || Some(col) == app;
            size = size.saturating_add(1);
            if size > RECORD_BYTES {
                bad = true;
            }
            if quoted {
                if b == b'"' {
                    quoted = false;
                    post_quote = true;
                } else if capture && field.len() < 200 {
                    field.push(b);
                }
            } else if matches!(b, b'\n' | b',') {
                let val = std::str::from_utf8(&field)
                    .unwrap_or("")
                    .trim_start_matches('\u{feff}');
                if header {
                    match (col, val) {
                        (0, "session_id") => (),
                        (0, _) => {
                            p.error("unsupported reference schema");
                            return;
                        }
                        (_, "indexed") => indexed = Some(col),
                        (_, "app_session") => app = Some(col),
                        _ => (),
                    }
                } else if col == 0 {
                    id = native(val);
                } else if capture {
                    if val.eq_ignore_ascii_case("true") {
                        backed = true;
                    } else if !val.eq_ignore_ascii_case("false") {
                        bad = true;
                    }
                }
                col += 1;
                field.clear();
                field_start = true;
                post_quote = false;
                if b == b'\n' {
                    if header {
                        if bad || indexed.is_none() || app.is_none() {
                            p.error("unsupported reference schema");
                            return;
                        }
                        header = false;
                        width = col;
                    } else if bad || col != width || id.is_none() {
                        p.error("invalid reference record");
                    } else if backed {
                        p.record((CLI, id.take().unwrap()), Evidence::default());
                    }
                    col = 0;
                    size = 0;
                    bad = false;
                    backed = false;
                    id = None;
                }
            } else if b == b'"' && (field_start || post_quote) {
                if post_quote && capture && field.len() < 200 {
                    field.push(b);
                }
                quoted = true;
                field_start = false;
                post_quote = false;
            } else if b != b'\r' {
                if post_quote || b == b'"' {
                    bad = true;
                }
                field_start = false;
                if capture && field.len() < 200 {
                    field.push(b);
                }
            }
        }
        if at_eof {
            break;
        }
        let n = buf.len();
        reader.consume(n);
    }
    if quoted || header {
        p.error("truncated reference inventory");
    }
}

fn live_scan() -> HistoryIndex {
    let home =
        std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
    let roaming = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        home.as_ref().map(|h| h.join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".config")))
    };
    let graph = std::env::var_os("DARBOT_3DKG_ROOT").map(PathBuf::from);
    scan(home.as_deref(), roaming.as_deref(), graph.as_deref())
}

fn scan(home: Option<&Path>, roaming: Option<&Path>, graph: Option<&Path>) -> HistoryIndex {
    let mut pools = Vec::new();
    let mut p = Pool::new(CLI, "Copilot CLI");
    let cli_root = home.map(|h| h.join(".copilot"));
    if let Some(d) = p.root(cli_root.as_deref(), false) {
        cli(&mut p, &d);
    }
    if let Some(d) = p.root(home.map(|h| h.join(".scout/copilot")).as_deref(), true) {
        cli(&mut p, &d);
    }
    pools.push(p);

    let mut p = Pool::new("clippyclaw-legacy", "ClippyClaw legacy");
    if let Some(d) = p.root(
        home.map(|h| h.join(".copilot/clippyclaw-sessions"))
            .as_deref(),
        false,
    ) {
        app_dir(&mut p, CLIPPY, &d);
    }
    pools.push(p);
    let mut p = Pool::new(CLIPPY, "ClippyClaw");
    for path in [".copilot/m-sessions", ".copilot-dev/m-sessions"] {
        if let Some(d) = p.root(home.map(|h| h.join(path)).as_deref(), true) {
            app_dir(&mut p, CLIPPY, &d);
        }
    }
    if let Some(d) = p.root(
        home.map(|h| h.join(".copilot/clippyclaw-tauri")).as_deref(),
        true,
    ) {
        if let Some(f) = p.file(&d, "native-state.json", true) {
            index_doc(&mut p, CLIPPY, f);
        }
    }
    pools.push(p);
    let mut p = Pool::new(CLAUDE, "Claude Code");
    if let Some(d) = p.root(home.map(|h| h.join(".claude")).as_deref(), false) {
        claude(&mut p, &d);
    }
    pools.push(p);
    let mut p = Pool::new(VSC, "VS Code Chat");
    for edition in ["Code", "Code - Insiders"] {
        if let Some(user) = p.root(
            roaming.map(|r| r.join(edition).join("User")).as_deref(),
            true,
        ) {
            vs_profile(&mut p, &user);
            if let Some(profiles) = p.dir(&user, "profiles", true) {
                p.list(&profiles, |p, name, kind| {
                    if kind.is_dir() || kind.is_symlink() {
                        if let Some(profile) = p.dir(&profiles, name, false) {
                            vs_profile(p, &profile);
                        }
                    }
                });
            }
        }
    }
    pools.push(p);
    for (id, label, folder) in [
        (CODEX, "Codex dated sessions", "sessions"),
        (
            "codex-archived",
            "Codex archived sessions",
            "archived_sessions",
        ),
    ] {
        let mut p = Pool::new(id, label);
        if let Some(base) = p.root(home.map(|h| h.join(".codex")).as_deref(), false) {
            if let Some(d) = p.dir(&base, folder, false) {
                codex_files(&mut p, &d, 0);
            }
            if id == CODEX {
                codex_index(&mut p, &base);
            }
        }
        pools.push(p);
    }
    let mut p = Pool::new(STUDIO, "AgentStudio");
    if let Some(base) = p.root(home.map(|h| h.join(".agentstudio")).as_deref(), false) {
        if let Some(d) = p.dir(&base, "as-sessions", true) {
            app_dir(&mut p, STUDIO, &d);
        }
        if let Some(d) = p.dir(&base, "copilot", true) {
            cli(&mut p, &d);
        }
    }
    pools.push(p);
    let mut p = Pool::new("3dkg", "3DKG Copilot inventory (reference only)");
    if let Some(d) = p.root(
        graph
            .map(|r| r.join("graph3d-out/hp11d/copilot-history"))
            .as_deref(),
        false,
    ) {
        if let Some(f) = p.file(&d, "session-inventory.csv", false) {
            references(&mut p, f);
        }
    }
    pools.push(p);
    finish(pools)
}

struct Pool {
    id: &'static str,
    label: &'static str,
    opened: bool,
    errors: usize,
    reasons: BTreeSet<&'static str>,
    records: BTreeMap<Key, Evidence>,
    duplicate: usize,
    // Both sides come from the SAME structured application record. Resolve after all pools,
    // so the order of the app index, app file and CLI journal doesn't affect attribution.
    links: BTreeMap<Key, BTreeSet<Key>>,
}

impl Pool {
    fn new(id: &'static str, label: &'static str) -> Self {
        Self {
            id,
            label,
            opened: false,
            errors: 0,
            reasons: Default::default(),
            records: Default::default(),
            duplicate: 0,
            links: Default::default(),
        }
    }
    fn error(&mut self, reason: &'static str) {
        self.errors += 1;
        self.reasons.insert(reason);
    }
    fn record(&mut self, key: Key, evidence: Evidence) {
        use std::collections::btree_map::Entry;
        match self.records.entry(key) {
            Entry::Vacant(e) => {
                e.insert(evidence);
            }
            Entry::Occupied(mut e) => {
                self.duplicate += 1;
                e.get_mut().merge(evidence);
            }
        }
    }
    fn add_agent(&mut self, e: &mut Evidence, s: Option<&str>, cli: bool) {
        if let Some(s) = s {
            if !e.agent(s, cli) {
                self.error("invalid agent identifier");
            }
        }
    }
}

fn finish(mut pools: Vec<Pool>) -> HistoryIndex {
    let mut links = BTreeMap::<Key, BTreeSet<Key>>::new();
    for p in &mut pools {
        for (k, v) in std::mem::take(&mut p.links) {
            links.entry(k).or_default().extend(v);
        }
    }
    let mut sessions = BTreeMap::<Key, (Evidence, BTreeSet<&'static str>)>::new();
    let mut sources = Vec::new();
    for mut p in pools {
        let mut local = BTreeMap::new();
        let mut unmatched = 0usize;
        for (mut key, evidence) in std::mem::take(&mut p.records) {
            if let Some(targets) = links.get(&key) {
                if targets.len() == 1 {
                    key = targets.first().unwrap().clone();
                } else {
                    p.error("conflicting backend IDs");
                }
            }
            // A reference alone never establishes a session or assigns it to an agent.
            if p.id == "3dkg" && !sessions.contains_key(&key) {
                unmatched += 1;
                continue;
            }
            *local.entry(key.clone()).or_insert(false) |= !evidence.agents.is_empty();
            if sessions.contains_key(&key) {
                p.duplicate += 1;
            }
            let (combined, sources) = sessions.entry(key).or_default();
            combined.merge(evidence);
            sources.insert(p.id);
        }
        let complete = p.opened && p.reasons.is_empty();
        let mut status = if !p.opened {
            "unavailable".to_owned()
        } else if !complete {
            "partial".to_owned()
        } else if p.id == "3dkg" {
            "reference only".to_owned()
        } else {
            "indexed".to_owned()
        };
        if !p.reasons.is_empty() {
            status += ": ";
            status += &p.reasons.into_iter().collect::<Vec<_>>().join("; ");
        }
        if unmatched != 0 {
            status += &format!("; {unmatched} unmatched references ignored");
        }
        sources.push(Source {
            id: p.id,
            label: p.label,
            indexed: local.len(),
            duplicates: p.duplicate,
            unreadable: p.errors,
            unattributed: local.values().filter(|&&has| !has).count(),
            complete,
            status,
        });
    }
    let sessions: Vec<_> = sessions
        .into_iter()
        .map(|((provider, native_id), (e, source_ids))| Session {
            id: format!("{provider}:{native_id}"),
            provider,
            native_id,
            agent_ids: e.agents,
            source_ids,
            updated_at: e.updated.and_then(|t| {
                t.to_offset(tauri::webview::cookie::time::UtcOffset::UTC)
                    .format(&Rfc3339)
                    .ok()
            }),
        })
        .collect();
    HistoryIndex {
        schema_version: 1,
        scanned_at: OffsetDateTime::now_utc().format(&Rfc3339).unwrap(),
        sources,
        sessions,
    }
}

fn refused() -> io::Error {
    io::Error::other("not a local ordinary archive entry")
}

fn allowed(m: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Reparse points, offline placeholders, EFS encrypted files. Do not recall/decrypt.
        if m.file_attributes() & (0x400 | 0x1000 | 0x4000 | 0x40000 | 0x400000) != 0 {
            return false;
        }
    }
    !m.file_type().is_symlink()
}

fn local(path: &Path) -> bool {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return false;
    }
    #[cfg(windows)]
    {
        use std::path::Prefix;
        let Some(Component::Prefix(prefix)) = path.components().next() else {
            return false;
        };
        let Prefix::Disk(letter) = prefix.kind() else {
            return false;
        };
        #[link(name = "kernel32")]
        extern "system" {
            fn GetDriveTypeW(root: *const u16) -> u32;
        }
        let root = [letter as u16, 58, 92, 0];
        // Only a drive root is passed; no UNC/device path is ever probed.
        matches!(unsafe { GetDriveTypeW(root.as_ptr()) }, 2 | 3 | 6)
    }
    #[cfg(not(windows))]
    {
        !path.as_os_str().to_string_lossy().starts_with("//")
    }
}

// Keep ancestors pinned against rename/reparse substitution while enumerating on Windows.
struct Dir {
    path: PathBuf,
    _parent: Option<Arc<Dir>>,
    #[cfg(windows)]
    _pin: File,
}

impl Dir {
    fn open(path: &Path) -> io::Result<Arc<Self>> {
        if !local(path) {
            return Err(refused());
        }
        let mut chain: Vec<_> = path.ancestors().collect();
        let mut dir = None;
        while let Some(p) = chain.pop() {
            dir = Some(Self::at(p.to_owned(), dir)?);
        }
        dir.ok_or_else(refused)
    }

    fn at(path: PathBuf, parent: Option<Arc<Self>>) -> io::Result<Arc<Self>> {
        let m = fs::symlink_metadata(&path)?;
        if !allowed(&m) || !m.is_dir() {
            return Err(refused());
        }
        #[cfg(windows)]
        let pin = {
            use std::os::windows::fs::OpenOptionsExt;
            let f = OpenOptions::new()
                .access_mode(0x80)
                .share_mode(3)
                .custom_flags(0x0200_0000 | 0x0020_0000)
                .open(&path)?;
            let m = f.metadata()?;
            if !allowed(&m) || !m.is_dir() {
                return Err(refused());
            }
            f
        };
        Ok(Arc::new(Self {
            path,
            _parent: parent,
            #[cfg(windows)]
            _pin: pin,
        }))
    }

    fn child(self: &Arc<Self>, name: &str) -> io::Result<Arc<Self>> {
        Self::at(self.path.join(name), Some(self.clone()))
    }
    fn file(&self, name: &str) -> io::Result<File> {
        let path = self.path.join(name);
        let m = fs::symlink_metadata(&path)?;
        if !allowed(&m) || !m.is_file() {
            return Err(refused());
        }
        let mut opts = OpenOptions::new();
        opts.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            opts.custom_flags(0x0020_0000 | 0x0800_0000);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let f = opts.open(&path)?;
        let m = f.metadata()?;
        if !allowed(&m) || !m.is_file() {
            return Err(refused());
        }
        Ok(f)
    }
}

impl Pool {
    fn root(&mut self, path: Option<&Path>, optional: bool) -> Option<Arc<Dir>> {
        match path.map(Dir::open) {
            Some(Ok(d)) => Some(d),
            Some(Err(e)) if optional && e.kind() == io::ErrorKind::NotFound => None,
            _ => {
                self.error("archive root unavailable");
                None
            }
        }
    }
    fn list(&mut self, dir: &Arc<Dir>, mut each: impl FnMut(&mut Self, &str, fs::FileType)) {
        match fs::read_dir(&dir.path) {
            Ok(entries) => {
                self.opened = true;
                for entry in entries {
                    if let Ok(entry) = entry {
                        if let (Some(name), Ok(kind)) =
                            (entry.file_name().to_str(), entry.file_type())
                        {
                            each(self, name, kind);
                            continue;
                        }
                    }
                    self.error("unreadable directory entry");
                }
            }
            Err(_) => self.error("unreadable directory"),
        }
    }
    fn dir(&mut self, parent: &Arc<Dir>, name: &str, optional: bool) -> Option<Arc<Dir>> {
        match parent.child(name) {
            Ok(d) => Some(d),
            Err(e) if optional && e.kind() == io::ErrorKind::NotFound => None,
            Err(_) => {
                self.error("unreadable or linked directory");
                None
            }
        }
    }
    fn file(&mut self, dir: &Dir, name: &str, optional: bool) -> Option<File> {
        match dir.file(name) {
            Ok(f) => {
                self.opened = true;
                Some(f)
            }
            Err(e) if optional && e.kind() == io::ErrorKind::NotFound => None,
            Err(_) => {
                self.error("unreadable, encrypted or linked file");
                None
            }
        }
    }
    fn sqlite(&mut self, dir: &Dir, name: &str) {
        match fs::symlink_metadata(dir.path.join(name)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            _ => self.error("SQLite not read (no locked SQLite reader)"),
        }
    }
    fn bytes(&mut self, file: File, max: usize) -> Option<Vec<u8>> {
        if file.metadata().is_ok_and(|m| m.len() > max as u64) {
            self.error("oversize document");
            return None;
        }
        let mut buf = Vec::new();
        match file.take(max as u64 + 1).read_to_end(&mut buf) {
            Ok(n) if n <= max => Some(buf),
            Ok(_) => {
                self.error("oversize document");
                None
            }
            Err(_) => {
                self.error("unreadable document");
                None
            }
        }
    }
    fn json(&mut self, file: File) -> Option<Value> {
        let buf = self.bytes(file, DOCUMENT_BYTES)?;
        match serde_json::from_slice(&buf) {
            Ok(v) => Some(v),
            Err(_) => {
                self.error("encrypted or malformed JSON");
                None
            }
        }
    }

    // Scan a finite snapshot, not a byte prefix or a perpetually advancing writer. Bound
    // read_until *before* allocating; skip a long record without losing later records.
    fn lines(&mut self, file: File, mut record: impl FnMut(&mut Pool, &[u8])) {
        let Ok(start) = file.metadata() else {
            self.error("archive stat failed");
            return;
        };
        let mut reader = BufReader::with_capacity(128 << 10, (&file).take(start.len()));
        let mut line = Vec::new();
        loop {
            line.clear();
            match (&mut reader)
                .take(RECORD_BYTES as u64 + 1)
                .read_until(b'\n', &mut line)
            {
                Ok(0) => break,
                Ok(_) if line.last() == Some(&b'\n') => {
                    line.pop();
                }
                Ok(n) if n > RECORD_BYTES => {
                    self.error("oversize record");
                    if reader.skip_until(b'\n').is_err() {
                        self.error("archive read failed");
                        break;
                    }
                    continue;
                }
                Ok(_) => (),
                Err(_) => {
                    self.error("archive read failed");
                    break;
                }
            }
            if !line.is_empty() {
                record(self, &line);
            }
        }
        if reader.get_ref().limit() != 0
            || !file.metadata().is_ok_and(|end| {
                end.len() == start.len() && end.modified().ok() == start.modified().ok()
            })
        {
            self.error("archive changed during scan");
        }
    }
}

/// Darbot-owned curation and metadata audit. Archive census paths are never write targets.
pub mod ledger {
    use crate::problem::Problem;
    use serde::{Deserialize, Deserializer, Serialize};
    use sha2::{Digest, Sha256};
    use std::collections::BTreeSet;
    use std::ffi::OsStr;
    use std::fs::{self, File, OpenOptions};
    use std::io::{self, Read, Write};
    use std::path::Path;
    use std::sync::{Mutex, OnceLock};

    const MAX_UNITS: usize = 2_000_000;
    const MAX_BYTES: usize = MAX_UNITS * 3;
    const MAX_DECISIONS: usize = 20_000;
    const MAX_REFERENCES: usize = 32;
    const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
    // A single gate also covers aliases of the same app directory, without an unbounded lock map.
    static STORE: OnceLock<Mutex<()>> = OnceLock::new();

    /// Schema v1 keeps the newest 1024 events per runtime/session scope in one atomic file.
    /// `droppedRecords` counts evicted events; each event's `droppedBefore` records the
    /// cumulative eviction count at its commit. Sequence numbers never restart on eviction.
    pub const PROTOCOL_AUDIT_MAX_RECORDS: usize = 1024;
    const PROTOCOL_AUDIT_SCHEMA: &str = "darbot.protocol-audit";

    /// Closed vocabulary: never add payloads, commands, paths, prompts, or error strings.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
    #[serde(rename_all = "kebab-case")]
    pub enum ProtocolOperation {
        TerminalCreate,
        TerminalOutput,
        TerminalWait,
        TerminalKill,
        TerminalRelease,
        FileRead,
        FileWrite,
        Permission,
        Elicitation,
        Prompt,
        SessionClose,
        PluginLoad,
        Lifecycle,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
    #[serde(rename_all = "kebab-case")]
    pub enum AuditOutcome {
        Requested,
        Approved,
        Rejected,
        Cancelled,
        Succeeded,
        Failed,
    }

    #[derive(Debug, Deserialize, Serialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct ProtocolAuditRecord {
        sequence: u64,
        timestamp_unix_ms: u64,
        operation: ProtocolOperation,
        outcome: AuditOutcome,
        dropped_before: u64,
    }

    #[derive(Debug, Deserialize, Serialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct ProtocolAudit {
        schema: String,
        version: u8,
        runtime_hash: String,
        #[serde(deserialize_with = "nullable")]
        session_hash: Option<String>,
        max_records: usize,
        total_records: u64,
        dropped_records: u64,
        records: Vec<ProtocolAuditRecord>,
    }

    fn invalid_audit() -> Problem {
        Problem::plain("Invalid or unsupported protocol audit. Existing data was kept.")
    }

    fn audit_storage_problem() -> Problem {
        Problem::plain(
            "Protocol audit could not be durably recorded. Inspect app-local storage before retrying.",
        )
    }

    fn audit_identity(value: &str) -> Result<String, Problem> {
        if value.trim().is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
            return Err(Problem::plain("Invalid protocol audit identity."));
        }
        Ok(format!("{:x}", Sha256::digest(value.as_bytes())))
    }

    fn audit_filename(runtime_hash: &str, session_hash: Option<&str>) -> String {
        // Fixed-width runtime digest plus an explicit optional-session discriminator.
        let scope = format!("{runtime_hash}:{}", session_hash.unwrap_or("none"));
        format!("{:x}.json", Sha256::digest(scope.as_bytes()))
    }

    impl ProtocolAudit {
        fn parse(
            raw: &str,
            runtime_hash: &str,
            session_hash: Option<&str>,
        ) -> Result<Self, Problem> {
            let audit: Self = serde_json::from_str(raw).map_err(|_| invalid_audit())?;
            if audit.schema != PROTOCOL_AUDIT_SCHEMA
                || audit.version != 1
                || audit.runtime_hash != runtime_hash
                || audit.session_hash.as_deref() != session_hash
                || audit.max_records != PROTOCOL_AUDIT_MAX_RECORDS
                || audit.total_records == 0
                || audit.total_records > MAX_SAFE_INTEGER
                || audit.records.len()
                    != audit.total_records.min(PROTOCOL_AUDIT_MAX_RECORDS as u64) as usize
                || audit.dropped_records
                    != audit
                        .total_records
                        .saturating_sub(PROTOCOL_AUDIT_MAX_RECORDS as u64)
            {
                return Err(invalid_audit());
            }
            for (index, record) in audit.records.iter().enumerate() {
                if record.sequence != audit.dropped_records + index as u64 + 1
                    || record.timestamp_unix_ms > MAX_SAFE_INTEGER
                    || record.dropped_before
                        != record
                            .sequence
                            .saturating_sub(PROTOCOL_AUDIT_MAX_RECORDS as u64)
                {
                    return Err(invalid_audit());
                }
            }
            Ok(audit)
        }
    }

    /// Persist one metadata-only event under Darbot's app-local-data `protocol-audit` child.
    /// `app_root` MUST come from the native app-local-data resolver, never IPC or archive paths.
    /// Only SHA-256 identity digests, closed enums, counters and timestamps reach disk.
    /// Calls in this process are serialized (including aliased roots); existing filesystem
    /// guards reject links and nonlocal storage. Success means the atomic replacement completed.
    /// Record permission approval and operation completion separately: approval is not success.
    pub fn record_protocol_audit(
        app_root: &Path,
        runtime_id: &str,
        session_id: Option<&str>,
        operation: ProtocolOperation,
        outcome: AuditOutcome,
    ) -> Result<(), Problem> {
        let runtime_hash = audit_identity(runtime_id)?;
        let session_hash = session_id.map(audit_identity).transpose()?;
        let name = audit_filename(&runtime_hash, session_hash.as_deref());
        let _guard = STORE
            .get_or_init(|| Mutex::new(()))
            .lock()
            .map_err(|_| audit_storage_problem())?;
        let dir = owned_directory(app_root, "protocol-audit", true)
            .map_err(|_| audit_storage_problem())?
            .ok_or_else(audit_storage_problem)?;
        let previous = contents(&dir, &name, true).map_err(|_| audit_storage_problem())?;
        let mut audit = match previous.as_deref() {
            Some(raw) => ProtocolAudit::parse(raw, &runtime_hash, session_hash.as_deref())?,
            None => ProtocolAudit {
                schema: PROTOCOL_AUDIT_SCHEMA.into(),
                version: 1,
                runtime_hash,
                session_hash,
                max_records: PROTOCOL_AUDIT_MAX_RECORDS,
                total_records: 0,
                dropped_records: 0,
                records: Vec::new(),
            },
        };
        if audit.total_records == MAX_SAFE_INTEGER {
            return Err(Problem::plain("Protocol audit sequence limit reached."));
        }
        let timestamp_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|duration| u64::try_from(duration.as_millis()).ok())
            .filter(|value| *value <= MAX_SAFE_INTEGER)
            .ok_or_else(audit_storage_problem)?;
        audit.total_records += 1;
        if audit.records.len() == PROTOCOL_AUDIT_MAX_RECORDS {
            audit.records.remove(0);
            audit.dropped_records += 1;
        }
        audit.records.push(ProtocolAuditRecord {
            sequence: audit.total_records,
            timestamp_unix_ms,
            operation,
            outcome,
            dropped_before: audit.dropped_records,
        });
        let json = serde_json::to_string(&audit).map_err(|_| invalid_audit())?;
        replace(&dir, &name, previous.as_deref(), |file| {
            file.write_all(json.as_bytes())
        })
        .map_err(|_| audit_storage_problem())
    }

    #[derive(Debug, Deserialize)]
    #[serde(
        tag = "kind",
        rename_all = "kebab-case",
        rename_all_fields = "camelCase",
        deny_unknown_fields
    )]
    pub enum Mutation {
        Migrate {
            ledger: String,
        },
        ToggleDecision {
            row: String,
            field: DecisionField,
        },
        PromoteDraft {
            conversation: String,
            canonical_session: String,
        },
        KeepReference {
            card: String,
        },
        ForgetReference {
            card: String,
        },
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub enum DecisionField {
        Pinned,
        Reviewed,
        Dismissed,
    }

    pub struct Outcome {
        pub json: String,
        pub committed: bool,
    }

    #[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct Feedback {
        pinned: bool,
        reviewed: bool,
        dismissed: bool,
    }

    #[derive(Debug, Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct Ledger {
        #[serde(deserialize_with = "version")]
        version: u8,
        decisions: Vec<(String, Feedback)>,
        references: Vec<Flashcard>,
    }

    impl Default for Ledger {
        fn default() -> Self {
            Self {
                version: 1,
                decisions: Vec::new(),
                references: Vec::new(),
            }
        }
    }

    // deserialize_with makes these fields required even though their values may be null.
    fn nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Option::<T>::deserialize(deserializer)
    }

    fn integer<E: serde::de::Error>(number: serde_json::Number) -> Result<u64, E> {
        if let Some(value) = number.as_u64().filter(|n| *n <= MAX_SAFE_INTEGER) {
            return Ok(value);
        }
        if let Some(value) = number.as_f64() {
            if value >= 0.0 && value <= MAX_SAFE_INTEGER as f64 && value.fract() == 0.0 {
                return Ok(value as u64);
            }
        }
        Err(E::custom("expected a nonnegative JavaScript safe integer"))
    }

    fn count<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        integer(serde_json::Number::deserialize(deserializer)?)
    }

    fn nullable_count<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u64>, D::Error> {
        Option::<serde_json::Number>::deserialize(deserializer)?
            .map(integer)
            .transpose()
    }

    fn version<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u8, D::Error> {
        if count(deserializer)? != 1 {
            return Err(serde::de::Error::custom("unsupported version"));
        }
        Ok(1)
    }

    #[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct Flashcard {
        schema: String,
        #[serde(deserialize_with = "version")]
        version: u8,
        agent: String,
        glyph: String,
        sessions: Summary,
        #[serde(deserialize_with = "nullable")]
        observation: Option<Observation>,
    }

    #[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Summary {
        #[serde(deserialize_with = "nullable_count")]
        known: Option<u64>,
        #[serde(deserialize_with = "nullable_count")]
        acp: Option<u64>,
        #[serde(deserialize_with = "nullable_count")]
        archive_only: Option<u64>,
        coverage: Coverage,
    }

    #[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
    #[serde(rename_all = "lowercase")]
    enum Coverage {
        Complete,
        Partial,
        Uncollected,
    }

    #[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Observation {
        kind: Kind,
        session: String,
        #[serde(deserialize_with = "nullable")]
        updated_at: Option<String>,
        #[serde(deserialize_with = "nullable")]
        source_feedback: Option<Feedback>,
        #[serde(deserialize_with = "nullable")]
        source_evidence: Option<Evidence>,
    }

    #[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
    #[serde(rename_all = "lowercase")]
    enum Kind {
        Acp,
        Archive,
        Draft,
    }

    #[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Evidence {
        #[serde(deserialize_with = "count")]
        verified_points: u64,
        #[serde(deserialize_with = "count")]
        verified_commits: u64,
    }

    fn invalid() -> Problem {
        Problem::plain("Invalid or unsupported Harvest ledger. Existing data was kept.")
    }

    fn bounded(text: &str, units: usize) -> bool {
        text.len() <= units * 3 && text.encode_utf16().count() <= units
    }

    fn identifier(text: &str) -> Result<(), Problem> {
        if text.is_empty() || !bounded(text, MAX_UNITS) {
            return Err(Problem::plain(
                "The Harvest identifier is empty or exceeds its size limit.",
            ));
        }
        Ok(())
    }

    fn fingerprint(text: &str) -> bool {
        text.len() == 64
            && text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }

    fn timestamp(text: &str) -> bool {
        use tauri::webview::cookie::time::{
            format_description::well_known::Rfc3339, OffsetDateTime,
        };
        if text.len() != 24 || !text.ends_with('Z') {
            return false;
        }
        let Ok(at) = OffsetDateTime::parse(text, &Rfc3339) else {
            return false;
        };
        text == format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            at.year(),
            u8::from(at.month()),
            at.day(),
            at.hour(),
            at.minute(),
            at.second(),
            at.millisecond()
        )
    }

    impl Flashcard {
        fn validate(&self) -> Result<(), Problem> {
            if self.schema != "darbot.agent-flashcard"
                || self.version != 1
                || !fingerprint(&self.agent)
            {
                return Err(invalid());
            }
            let first = u8::from_str_radix(&self.agent[..2], 16).map_err(|_| invalid())?;
            if self.glyph
                != char::from_u32(0x2800 + u32::from(first))
                    .unwrap()
                    .to_string()
            {
                return Err(invalid());
            }
            let s = &self.sessions;
            if [s.known, s.acp, s.archive_only]
                .into_iter()
                .flatten()
                .any(|n| n > MAX_SAFE_INTEGER)
                || (s.coverage == Coverage::Complete
                    && [s.known, s.acp, s.archive_only].contains(&None))
                || s.known.unwrap_or(0) != s.acp.unwrap_or(0) + s.archive_only.unwrap_or(0)
                || (s.coverage != Coverage::Complete && s.known == Some(0))
                || (s.coverage == Coverage::Uncollected && s.known.is_some())
            {
                return Err(invalid());
            }
            if let Some(o) = &self.observation {
                if !fingerprint(&o.session)
                    || o.updated_at.as_deref().is_some_and(|at| !timestamp(at))
                {
                    return Err(invalid());
                }
                if let Some(e) = &o.source_evidence {
                    if o.kind != Kind::Acp
                        || e.verified_points > MAX_SAFE_INTEGER
                        || e.verified_commits > MAX_SAFE_INTEGER
                        || (e.verified_commits == 0 && e.verified_points != 0)
                    {
                        return Err(invalid());
                    }
                }
            }
            Ok(())
        }

        fn parse(text: &str) -> Result<Self, Problem> {
            if !bounded(text, 8192) || text.contains('\\') {
                return Err(invalid());
            }
            // Legacy ledgers have already been through JSON.parse/stringify in the old store, but
            // pasted cards must also be lossless. Valid card strings never need JSON escapes.
            for outside in text.split('"').step_by(2) {
                for token in
                    outside.split(|c: char| c.is_ascii_whitespace() || "{}[],:".contains(c))
                {
                    if token.starts_with(|c: char| c.is_ascii_digit() || c == '-')
                        && token
                            .parse::<u64>()
                            .map_or(true, |n| n.to_string() != token)
                    {
                        return Err(invalid());
                    }
                }
            }
            let card: Self = serde_json::from_str(text).map_err(|_| invalid())?;
            card.validate()?;
            Ok(card)
        }
    }

    impl Ledger {
        fn parse(text: &str) -> Result<Self, Problem> {
            if !bounded(text, MAX_UNITS) {
                return Err(invalid());
            }
            let ledger: Self = serde_json::from_str(text).map_err(|_| invalid())?;
            ledger.validate()?;
            Ok(ledger)
        }

        fn validate(&self) -> Result<(), Problem> {
            if self.version != 1
                || self.decisions.len() > MAX_DECISIONS
                || self.references.len() > MAX_REFERENCES
            {
                return Err(invalid());
            }
            let mut rows = BTreeSet::new();
            for (row, _) in &self.decisions {
                if row.is_empty() || !rows.insert(row) {
                    return Err(invalid());
                }
            }
            for card in &self.references {
                card.validate()?;
            }
            Ok(())
        }

        fn json(&self) -> Result<String, Problem> {
            self.validate()?;
            // Struct order is the Flashcard wire contract; a generic Value sorts its fields.
            let json = serde_json::to_string(self).map_err(|_| invalid())?;
            if !bounded(&json, MAX_UNITS) {
                return Err(invalid());
            }
            Ok(json)
        }
    }

    fn filename(agent: &str) -> Result<String, Problem> {
        identifier(agent)?;
        Ok(format!("{:x}.json", Sha256::digest(agent.as_bytes())))
    }

    fn io_problem(error: io::Error) -> Problem {
        Problem::with(
            "Harvest storage could not complete the request. Inspect the error before retrying.",
            error.to_string(),
        )
    }

    fn refused() -> io::Error {
        io::Error::other("Harvest requires an ordinary private local file or directory")
    }

    struct Directory {
        #[cfg(windows)]
        inner: std::sync::Arc<super::Dir>,
        #[cfg(unix)]
        file: File,
    }

    impl Directory {
        fn open(path: &Path) -> io::Result<Self> {
            if !super::local(path) {
                return Err(refused());
            }
            #[cfg(windows)]
            {
                Ok(Self {
                    inner: super::Dir::open(path)?,
                })
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                let mut dir = Self {
                    file: OpenOptions::new()
                        .read(true)
                        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                        .open(Path::new("/"))?,
                };
                for component in path.components() {
                    if let std::path::Component::Normal(name) = component {
                        dir = dir.child(name, false)?;
                    }
                }
                Ok(dir)
            }
        }

        fn child(&self, name: &OsStr, create: bool) -> io::Result<Self> {
            #[cfg(windows)]
            {
                let path = self.inner.path.join(name);
                let open = || super::Dir::at(path.clone(), Some(self.inner.clone()));
                let inner = match open() {
                    Ok(inner) => inner,
                    Err(e) if create && e.kind() == io::ErrorKind::NotFound => {
                        match fs::create_dir(&path) {
                            Ok(()) => (),
                            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
                            Err(e) => return Err(e),
                        }
                        open()?
                    }
                    Err(e) => return Err(e),
                };
                if fs::canonicalize(&path)? != fs::canonicalize(&self.inner.path)?.join(name) {
                    return Err(refused());
                }
                Ok(Self { inner })
            }
            #[cfg(unix)]
            {
                use std::os::fd::{AsRawFd, FromRawFd};
                let name = c_name(name)?;
                let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
                let mut fd = unsafe { libc::openat(self.file.as_raw_fd(), name.as_ptr(), flags) };
                if fd < 0 && create && io::Error::last_os_error().kind() == io::ErrorKind::NotFound
                {
                    let result =
                        unsafe { libc::mkdirat(self.file.as_raw_fd(), name.as_ptr(), 0o700) };
                    if result < 0
                        && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists
                    {
                        return Err(io::Error::last_os_error());
                    }
                    self.file.sync_all()?;
                    fd = unsafe { libc::openat(self.file.as_raw_fd(), name.as_ptr(), flags) };
                }
                if fd < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(Self {
                    file: unsafe { File::from_raw_fd(fd) },
                })
            }
        }

        fn file(&self, name: &str, create: bool) -> io::Result<File> {
            #[cfg(windows)]
            let file = {
                use std::os::windows::fs::OpenOptionsExt;
                let mut options = OpenOptions::new();
                options
                    .custom_flags(0x0020_0000 | 0x0800_0000)
                    .share_mode(1);
                if create {
                    options.write(true).create_new(true);
                } else {
                    options.read(true);
                }
                options.open(self.inner.path.join(name))?
            };
            #[cfg(unix)]
            let file = {
                use std::os::fd::{AsRawFd, FromRawFd};
                let name = c_name(OsStr::new(name))?;
                let flags = libc::O_NOFOLLOW
                    | libc::O_NONBLOCK
                    | libc::O_CLOEXEC
                    | if create {
                        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL
                    } else {
                        libc::O_RDONLY
                    };
                let fd =
                    unsafe { libc::openat(self.file.as_raw_fd(), name.as_ptr(), flags, 0o600) };
                if fd < 0 {
                    return Err(io::Error::last_os_error());
                }
                unsafe { File::from_raw_fd(fd) }
            };
            if let Err(error) = ordinary(&file) {
                drop(file);
                return Err(if create {
                    cleanup(self, name, error)
                } else {
                    error
                });
            }
            Ok(file)
        }

        fn rename(&self, from: &str, to: &str) -> io::Result<()> {
            #[cfg(windows)]
            {
                use std::os::windows::ffi::OsStrExt;
                #[link(name = "kernel32")]
                extern "system" {
                    fn MoveFileExW(
                        existing: *const u16,
                        replacement: *const u16,
                        flags: u32,
                    ) -> i32;
                }
                let directory = fs::canonicalize(&self.inner.path)?;
                let source: Vec<u16> = directory
                    .join(from)
                    .as_os_str()
                    .encode_wide()
                    .chain(Some(0))
                    .collect();
                let destination: Vec<u16> = directory
                    .join(to)
                    .as_os_str()
                    .encode_wide()
                    .chain(Some(0))
                    .collect();
                // Replacement must be committed before an approved acting call proceeds.
                if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), 0x1 | 0x8) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            }
            #[cfg(unix)]
            {
                use std::os::fd::AsRawFd;
                let from = c_name(OsStr::new(from))?;
                let to = c_name(OsStr::new(to))?;
                let fd = self.file.as_raw_fd();
                if unsafe { libc::renameat(fd, from.as_ptr(), fd, to.as_ptr()) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            }
        }

        fn remove(&self, name: &str) -> io::Result<()> {
            #[cfg(windows)]
            {
                fs::remove_file(self.inner.path.join(name))
            }
            #[cfg(unix)]
            {
                use std::os::fd::AsRawFd;
                let name = c_name(OsStr::new(name))?;
                if unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), 0) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            }
        }

        fn sync(&self) -> io::Result<()> {
            #[cfg(unix)]
            self.file.sync_all()?;
            Ok(())
        }
    }

    #[cfg(unix)]
    fn c_name(name: &OsStr) -> io::Result<std::ffi::CString> {
        use std::os::unix::ffi::OsStrExt;
        std::ffi::CString::new(name.as_bytes()).map_err(|_| refused())
    }

    fn ordinary(file: &File) -> io::Result<fs::Metadata> {
        let metadata = file.metadata()?;
        if !super::allowed(&metadata) || !metadata.is_file() {
            return Err(refused());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.nlink() != 1 {
                return Err(refused());
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            #[repr(C)]
            #[derive(Default)]
            struct Information {
                attributes: u32,
                creation: [u32; 2],
                access: [u32; 2],
                write: [u32; 2],
                volume: u32,
                size_high: u32,
                size_low: u32,
                links: u32,
                index_high: u32,
                index_low: u32,
            }
            #[link(name = "kernel32")]
            extern "system" {
                fn GetFileInformationByHandle(
                    handle: *mut std::ffi::c_void,
                    info: *mut Information,
                ) -> i32;
            }
            let mut info = Information::default();
            if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
                return Err(io::Error::last_os_error());
            }
            if info.links != 1 {
                return Err(refused());
            }
        }
        Ok(metadata)
    }

    // Only the app directory itself and a fixed owned child may be created. Ancestors are never
    // created, followed through links, or taken from an IPC argument. Unix operations are dirfd-
    // relative; Windows ancestors stay pinned against rename/reparse substitution.
    fn directory(app_root: &Path, create: bool) -> io::Result<Option<Directory>> {
        owned_directory(app_root, "harvest", create)
    }

    fn owned_directory(
        app_root: &Path,
        child: &str,
        create: bool,
    ) -> io::Result<Option<Directory>> {
        let parent = Directory::open(app_root.parent().ok_or_else(refused)?)?;
        let result = parent
            .child(app_root.file_name().ok_or_else(refused)?, create)
            .and_then(|root| root.child(OsStr::new(child), create));
        match result {
            Ok(dir) => Ok(Some(dir)),
            Err(e) if !create && e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn contents(dir: &Directory, name: &str, writing: bool) -> io::Result<Option<String>> {
        let file = match dir.file(name, false) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let before = ordinary(&file)?;
        if writing && before.permissions().readonly() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Harvest ledger is read-only",
            ));
        }
        if before.len() > MAX_BYTES as u64 {
            return Err(refused());
        }
        let mut bytes = Vec::new();
        (&file)
            .take((MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        let after = ordinary(&file)?;
        if bytes.len() > MAX_BYTES
            || before.len() != after.len()
            || before.modified()? != after.modified()?
        {
            return Err(io::Error::other(
                "Harvest ledger changed while it was being read",
            ));
        }
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| io::Error::other("Harvest ledger is not UTF-8"))
    }

    fn cleanup(dir: &Directory, name: &str, error: io::Error) -> io::Error {
        match dir.remove(name) {
            Ok(()) => error,
            Err(e) if e.kind() == io::ErrorKind::NotFound => error,
            Err(e) => io::Error::other(format!("{error}; staging cleanup also failed: {e}")),
        }
    }

    fn staging(
        dir: &Directory,
        write: impl FnOnce(&mut File) -> io::Result<()>,
    ) -> io::Result<String> {
        let name = format!(".harvest-write-{:032x}", rand::random::<u128>());
        // A collision is not ours to clean up.
        let mut file = dir.file(&name, true)?;
        let result = write(&mut file).and_then(|()| file.sync_all());
        drop(file);
        match result {
            Ok(()) => Ok(name),
            Err(error) => Err(cleanup(dir, &name, error)),
        }
    }

    fn replace(
        dir: &Directory,
        name: &str,
        previous: Option<&str>,
        write: impl FnOnce(&mut File) -> io::Result<()>,
    ) -> io::Result<()> {
        let stage = staging(dir, write)?;
        let result = (|| {
            if contents(dir, name, true)?.as_deref() != previous {
                return Err(io::Error::other(
                    "Harvest ledger changed before commit; reload before retrying",
                ));
            }
            dir.sync()?;
            dir.rename(&stage, name)?;
            if let Err(error) = dir.sync() {
                // A failed directory flush must not be reported as a successful durable update.
                let rollback = if let Some(old) = previous {
                    staging(dir, |file| file.write_all(old.as_bytes())).and_then(|backup| {
                        dir.rename(&backup, name)
                            .map_err(|e| cleanup(dir, &backup, e))
                    })
                } else {
                    dir.remove(name)
                }
                .and_then(|()| dir.sync());
                return Err(match rollback {
                    Ok(()) => io::Error::other(format!("Harvest commit was rolled back: {error}")),
                    Err(rollback) => io::Error::other(format!(
                        "Harvest durability failed: {error}; rollback also failed: {rollback}; inspect storage before retrying"
                    )),
                });
            }
            Ok(())
        })();
        result.map_err(|error| cleanup(dir, &stage, error))
    }

    pub fn read(app_root: &Path, agent: &str) -> Result<Option<String>, Problem> {
        let name = filename(agent)?;
        let _guard = STORE.get_or_init(|| Mutex::new(())).lock().map_err(|_| {
            Problem::plain("Harvest storage stopped; restart Darbot before retrying.")
        })?;
        let Some(dir) = directory(app_root, false).map_err(io_problem)? else {
            return Ok(None);
        };
        contents(&dir, &name, false)
            .map_err(io_problem)?
            .map(|raw| Ledger::parse(&raw)?.json())
            .transpose()
    }

    pub fn mutate(app_root: &Path, agent: &str, mutation: Mutation) -> Result<Outcome, Problem> {
        let name = filename(agent)?;
        let migrating = matches!(&mutation, Mutation::Migrate { .. });
        let _guard = STORE.get_or_init(|| Mutex::new(())).lock().map_err(|_| {
            Problem::plain("Harvest storage stopped; restart Darbot before retrying.")
        })?;
        let mut dir = directory(app_root, false).map_err(io_problem)?;
        let previous = dir
            .as_ref()
            .map(|dir| contents(dir, &name, false))
            .transpose()
            .map_err(io_problem)?
            .flatten();
        let mut ledger = previous
            .as_deref()
            .map(Ledger::parse)
            .transpose()?
            .unwrap_or_default();
        let before = ledger.json()?;
        match mutation {
            Mutation::Migrate { ledger: raw } => {
                let migrated = Ledger::parse(&raw)?;
                if previous.is_some() {
                    return Ok(Outcome {
                        json: before,
                        committed: false,
                    });
                }
                ledger = migrated;
            }
            Mutation::ToggleDecision { row, field } => {
                identifier(&row)?;
                let index = ledger.decisions.iter().position(|(key, _)| key == &row);
                let mut next = index
                    .map(|i| ledger.decisions[i].1.clone())
                    .unwrap_or_default();
                let value = match field {
                    DecisionField::Pinned => &mut next.pinned,
                    DecisionField::Reviewed => &mut next.reviewed,
                    DecisionField::Dismissed => &mut next.dismissed,
                };
                *value = !*value;
                if next == Feedback::default() {
                    if let Some(i) = index {
                        ledger.decisions.remove(i);
                    }
                } else if let Some(i) = index {
                    ledger.decisions[i].1 = next;
                } else {
                    ledger.decisions.push((row, next));
                }
            }
            Mutation::PromoteDraft {
                conversation,
                canonical_session,
            } => {
                identifier(&conversation)?;
                identifier(&canonical_session)?;
                let from =
                    serde_json::to_string(&["draft", &conversation]).map_err(|_| invalid())?;
                let to = serde_json::to_string(&["session", &canonical_session])
                    .map_err(|_| invalid())?;
                if let Some(index) = ledger.decisions.iter().position(|(row, _)| row == &from) {
                    let pending = ledger.decisions[index].1.clone();
                    if !ledger.decisions.iter().any(|(row, _)| row == &to) {
                        ledger.decisions.push((to, pending));
                    }
                    ledger.decisions.remove(index);
                }
            }
            Mutation::KeepReference { card } => {
                let card = Flashcard::parse(&card)?;
                if !ledger.references.contains(&card) {
                    if ledger.references.len() == MAX_REFERENCES {
                        return Err(Problem::plain("Remove a reference before saving another."));
                    }
                    ledger.references.push(card);
                }
            }
            Mutation::ForgetReference { card } => {
                let card = Flashcard::parse(&card)?;
                ledger.references.retain(|saved| saved != &card);
            }
        }
        let json = ledger.json()?;
        // Promotion/forget/dedup no-ops do not fabricate an empty store.
        if json == before && !(previous.is_none() && migrating) {
            return Ok(Outcome {
                json,
                committed: false,
            });
        }
        if dir.is_none() {
            dir = directory(app_root, true).map_err(io_problem)?;
        }
        let dir = dir.ok_or_else(invalid)?;
        replace(&dir, &name, previous.as_deref(), |file| {
            file.write_all(json.as_bytes())
        })
        .map_err(io_problem)?;
        Ok(Outcome {
            json,
            committed: true,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde_json::json;
        use std::path::PathBuf;

        struct Fixture {
            base: PathBuf,
            app: PathBuf,
        }

        impl Fixture {
            fn new() -> Self {
                let base = crate::test_support::temp_root("harvest-ledger");
                fs::create_dir_all(&base).unwrap();
                Self {
                    app: base.join("darbot"),
                    base,
                }
            }

            fn path(&self, agent: &str) -> PathBuf {
                self.app.join("harvest").join(filename(agent).unwrap())
            }

            fn seed(&self, agent: &str, text: &str) {
                directory(&self.app, true).unwrap();
                fs::write(self.path(agent), text).unwrap();
            }

            fn edit(&self, mutation: Mutation) -> Outcome {
                mutate(&self.app, "agent", mutation).unwrap()
            }

            fn ledger(&self) -> Ledger {
                Ledger::parse(&read(&self.app, "agent").unwrap().unwrap()).unwrap()
            }

            fn audit_path(&self, runtime: &str, session: Option<&str>) -> PathBuf {
                self.app.join("protocol-audit").join(audit_filename(
                    &audit_identity(runtime).unwrap(),
                    session.map(audit_identity).transpose().unwrap().as_deref(),
                ))
            }

            fn audit(&self, runtime: &str, session: Option<&str>) -> ProtocolAudit {
                ProtocolAudit::parse(
                    &fs::read_to_string(self.audit_path(runtime, session)).unwrap(),
                    &audit_identity(runtime).unwrap(),
                    session.map(audit_identity).transpose().unwrap().as_deref(),
                )
                .unwrap()
            }
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                fs::remove_dir_all(&self.base).expect("remove owned Harvest fixture");
            }
        }

        fn toggle(row: &str, field: DecisionField) -> Mutation {
            Mutation::ToggleDecision {
                row: row.into(),
                field,
            }
        }

        #[test]
        fn protocol_audit_reloads_durable_rows_and_distinguishes_approval_from_failure() {
            let f = Fixture::new();
            for (operation, outcome) in [
                (ProtocolOperation::Permission, AuditOutcome::Approved),
                (ProtocolOperation::FileWrite, AuditOutcome::Failed),
            ] {
                record_protocol_audit(&f.app, "runtime", Some("session"), operation, outcome)
                    .unwrap();
                // Every call reopens and validates disk state; no cached records supply the result.
                let audit = f.audit("runtime", Some("session"));
                assert_eq!(audit.records.last().unwrap().operation, operation);
                assert_eq!(audit.records.last().unwrap().outcome, outcome);
            }
            let audit = f.audit("runtime", Some("session"));
            assert_eq!(audit.total_records, 2);
            assert_eq!(audit.dropped_records, 0);
            assert_eq!(audit.records[0].sequence, 1);
            assert_eq!(audit.records[1].sequence, 2);
            assert!(audit.records.iter().all(|row| row.timestamp_unix_ms > 0));
            let entries: Vec<_> = fs::read_dir(f.app.join("protocol-audit"))
                .unwrap()
                .collect();
            assert_eq!(entries.len(), 1, "no staging files remain after success");
        }

        #[test]
        #[cfg(windows)]
        fn protocol_audit_replaces_records_beyond_windows_legacy_path_limit() {
            let mut f = Fixture::new();
            f.app = f.app.join("a".repeat(90)).join("b".repeat(90));
            fs::create_dir_all(f.app.parent().unwrap()).unwrap();
            assert!(f.audit_path("runtime", Some("session")).as_os_str().len() > 260);
            for outcome in [AuditOutcome::Requested, AuditOutcome::Succeeded] {
                record_protocol_audit(
                    &f.app,
                    "runtime",
                    Some("session"),
                    ProtocolOperation::TerminalCreate,
                    outcome,
                )
                .unwrap();
            }
            let audit = f.audit("runtime", Some("session"));
            assert_eq!(audit.total_records, 2);
            assert_eq!(audit.records[1].outcome, AuditOutcome::Succeeded);
        }

        #[test]
        fn protocol_audit_isolates_roots_runtime_session_and_harvest() {
            let f = Fixture::new();
            let other = Fixture::new();
            f.edit(toggle("curation", DecisionField::Pinned));
            let harvest = fs::read(f.path("agent")).unwrap();
            for (runtime, session) in [
                ("runtime", None),
                ("runtime", Some("one")),
                ("runtime", Some("two")),
                ("another", Some("one")),
            ] {
                record_protocol_audit(
                    &f.app,
                    runtime,
                    session,
                    ProtocolOperation::Lifecycle,
                    AuditOutcome::Requested,
                )
                .unwrap();
                assert_eq!(f.audit(runtime, session).total_records, 1);
            }
            record_protocol_audit(
                &other.app,
                "runtime",
                Some("one"),
                ProtocolOperation::Prompt,
                AuditOutcome::Cancelled,
            )
            .unwrap();
            assert_eq!(
                other.audit("runtime", Some("one")).records[0].outcome,
                AuditOutcome::Cancelled
            );
            assert_eq!(
                f.audit("runtime", Some("one")).records[0].outcome,
                AuditOutcome::Requested
            );
            assert_eq!(fs::read(f.path("agent")).unwrap(), harvest);
            assert_eq!(
                fs::read_dir(f.app.join("protocol-audit")).unwrap().count(),
                4
            );
        }

        #[test]
        fn protocol_audit_bad_identity_creates_nothing() {
            let f = Fixture::new();
            for value in ["", " ", "line\nbreak", "\0", &"x".repeat(4097)] {
                assert!(record_protocol_audit(
                    &f.app,
                    value,
                    None,
                    ProtocolOperation::Lifecycle,
                    AuditOutcome::Requested,
                )
                .is_err());
                assert!(record_protocol_audit(
                    &f.app,
                    "runtime",
                    Some(value),
                    ProtocolOperation::Lifecycle,
                    AuditOutcome::Requested,
                )
                .is_err());
            }
            assert!(!f.app.exists());
        }

        #[test]
        fn protocol_audit_preserves_corrupt_or_unsupported_previous_bytes() {
            let f = Fixture::new();
            record_protocol_audit(
                &f.app,
                "runtime",
                None,
                ProtocolOperation::PluginLoad,
                AuditOutcome::Requested,
            )
            .unwrap();
            let path = f.audit_path("runtime", None);
            let valid: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            let mut cases = vec![b"broken json".to_vec(), vec![0xff, 0xfe]];
            for (field, value) in [
                ("version", json!(2)),
                ("runtimeHash", json!("wrong-scope")),
                ("sessionHash", json!("wrong-session")),
                ("totalRecords", json!(9)),
                ("droppedRecords", json!(1)),
                ("maxRecords", json!(999)),
                ("payload", json!("MUST-NOT-BE-IMPORTED")),
                ("records", json!([])),
            ] {
                let mut invalid = valid.clone();
                invalid[field] = value;
                cases.push(serde_json::to_vec(&invalid).unwrap());
            }
            for (field, value) in [
                ("sequence", json!(2)),
                ("droppedBefore", json!(1)),
                ("operation", json!("execute-secret")),
                ("outcome", json!("unknown")),
                ("prompt", json!("MUST-NOT-BE-IMPORTED")),
            ] {
                let mut invalid = valid.clone();
                invalid["records"][0][field] = value;
                cases.push(serde_json::to_vec(&invalid).unwrap());
            }
            for bytes in cases {
                fs::write(&path, &bytes).unwrap();
                assert!(record_protocol_audit(
                    &f.app,
                    "runtime",
                    None,
                    ProtocolOperation::PluginLoad,
                    AuditOutcome::Succeeded,
                )
                .is_err());
                assert_eq!(fs::read(&path).unwrap(), bytes);
            }
            assert_eq!(
                fs::read_dir(f.app.join("protocol-audit")).unwrap().count(),
                1
            );
        }

        #[test]
        fn protocol_audit_retention_is_bounded_and_eviction_is_explicit() {
            let f = Fixture::new();
            record_protocol_audit(
                &f.app,
                "runtime",
                None,
                ProtocolOperation::TerminalOutput,
                AuditOutcome::Succeeded,
            )
            .unwrap();
            let mut audit = f.audit("runtime", None);
            // Seed a validated full window to exercise the exact boundary without 1024 fsyncs.
            for sequence in 2..=PROTOCOL_AUDIT_MAX_RECORDS as u64 {
                audit.records.push(ProtocolAuditRecord {
                    sequence,
                    timestamp_unix_ms: audit.records[0].timestamp_unix_ms,
                    operation: ProtocolOperation::TerminalOutput,
                    outcome: AuditOutcome::Succeeded,
                    dropped_before: 0,
                });
            }
            audit.total_records = PROTOCOL_AUDIT_MAX_RECORDS as u64;
            fs::write(
                f.audit_path("runtime", None),
                serde_json::to_vec(&audit).unwrap(),
            )
            .unwrap();
            assert_eq!(
                f.audit("runtime", None).records.len(),
                PROTOCOL_AUDIT_MAX_RECORDS
            );
            for _ in 0..3 {
                record_protocol_audit(
                    &f.app,
                    "runtime",
                    None,
                    ProtocolOperation::TerminalOutput,
                    AuditOutcome::Succeeded,
                )
                .unwrap();
            }
            let audit = f.audit("runtime", None);
            assert_eq!(audit.records.len(), PROTOCOL_AUDIT_MAX_RECORDS);
            assert_eq!(audit.total_records, PROTOCOL_AUDIT_MAX_RECORDS as u64 + 3);
            assert_eq!(audit.dropped_records, 3);
            assert_eq!(audit.records[0].sequence, 4);
            assert_eq!(audit.records[0].dropped_before, 0);
            assert_eq!(audit.records.last().unwrap().dropped_before, 3);
            assert!(fs::metadata(f.audit_path("runtime", None)).unwrap().len() < 256 * 1024);
            assert_eq!(
                fs::read_dir(f.app.join("protocol-audit")).unwrap().count(),
                1
            );
        }

        #[test]
        fn protocol_audit_concurrent_calls_do_not_lose_rows() {
            let f = Fixture::new();
            std::thread::scope(|scope| {
                for _ in 0..4 {
                    let app = &f.app;
                    scope.spawn(move || {
                        for _ in 0..12 {
                            record_protocol_audit(
                                app,
                                "runtime",
                                Some("session"),
                                ProtocolOperation::Elicitation,
                                AuditOutcome::Rejected,
                            )
                            .unwrap();
                        }
                    });
                }
            });
            let audit = f.audit("runtime", Some("session"));
            assert_eq!(audit.total_records, 48);
            assert_eq!(audit.records.len(), 48);
            assert!(audit
                .records
                .iter()
                .enumerate()
                .all(|(i, row)| row.sequence == i as u64 + 1));
        }

        #[test]
        fn protocol_audit_has_closed_vocabulary_and_no_identity_plaintext() {
            let f = Fixture::new();
            let runtime = "https://private.example/SECRET-RUNTIME-TOKEN";
            let session = "SECRET-SESSION-ID";
            let operations = [
                (ProtocolOperation::TerminalCreate, "terminal-create"),
                (ProtocolOperation::TerminalOutput, "terminal-output"),
                (ProtocolOperation::TerminalWait, "terminal-wait"),
                (ProtocolOperation::TerminalKill, "terminal-kill"),
                (ProtocolOperation::TerminalRelease, "terminal-release"),
                (ProtocolOperation::FileRead, "file-read"),
                (ProtocolOperation::FileWrite, "file-write"),
                (ProtocolOperation::Permission, "permission"),
                (ProtocolOperation::Elicitation, "elicitation"),
                (ProtocolOperation::Prompt, "prompt"),
                (ProtocolOperation::SessionClose, "session-close"),
                (ProtocolOperation::PluginLoad, "plugin-load"),
                (ProtocolOperation::Lifecycle, "lifecycle"),
            ];
            for (operation, wire) in operations {
                assert_eq!(serde_json::to_value(operation).unwrap(), json!(wire));
                record_protocol_audit(
                    &f.app,
                    runtime,
                    Some(session),
                    operation,
                    AuditOutcome::Requested,
                )
                .unwrap();
            }
            for (outcome, wire) in [
                (AuditOutcome::Requested, "requested"),
                (AuditOutcome::Approved, "approved"),
                (AuditOutcome::Rejected, "rejected"),
                (AuditOutcome::Cancelled, "cancelled"),
                (AuditOutcome::Succeeded, "succeeded"),
                (AuditOutcome::Failed, "failed"),
            ] {
                assert_eq!(serde_json::to_value(outcome).unwrap(), json!(wire));
            }
            for payload in [
                json!("SECRET"),
                json!({"file-write": {"command": "SECRET"}}),
            ] {
                assert!(serde_json::from_value::<ProtocolOperation>(payload.clone()).is_err());
                assert!(serde_json::from_value::<AuditOutcome>(payload).is_err());
            }
            let path = f.audit_path(runtime, Some(session));
            assert!(fingerprint(path.file_stem().unwrap().to_str().unwrap()));
            let raw = fs::read_to_string(path).unwrap();
            assert!(!raw.contains("SECRET"));
            assert!(!raw.contains("private.example"));
            assert!(!raw.contains(runtime));
            assert!(!raw.contains(session));
            let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(json.as_object().unwrap().len(), 8);
            for row in json["records"].as_array().unwrap() {
                assert_eq!(
                    row.as_object()
                        .unwrap()
                        .keys()
                        .map(String::as_str)
                        .collect::<BTreeSet<_>>(),
                    BTreeSet::from([
                        "sequence",
                        "timestampUnixMs",
                        "operation",
                        "outcome",
                        "droppedBefore"
                    ]),
                );
            }
        }

        #[test]
        fn protocol_audit_read_only_or_obstructed_storage_never_reports_success() {
            let f = Fixture::new();
            record_protocol_audit(
                &f.app,
                "runtime",
                None,
                ProtocolOperation::Lifecycle,
                AuditOutcome::Requested,
            )
            .unwrap();
            let path = f.audit_path("runtime", None);
            let before = fs::read(&path).unwrap();
            let original_permissions = fs::metadata(&path).unwrap().permissions();
            let mut readonly = original_permissions.clone();
            readonly.set_readonly(true);
            fs::set_permissions(&path, readonly).unwrap();
            let result = record_protocol_audit(
                &f.app,
                "runtime",
                None,
                ProtocolOperation::Lifecycle,
                AuditOutcome::Succeeded,
            );
            fs::set_permissions(&path, original_permissions).unwrap();
            assert!(result.is_err());
            assert_eq!(fs::read(&path).unwrap(), before);
            let other = Fixture::new();
            fs::create_dir(&other.app).unwrap();
            fs::write(other.app.join("protocol-audit"), b"obstruction").unwrap();
            assert!(record_protocol_audit(
                &other.app,
                "runtime",
                None,
                ProtocolOperation::Lifecycle,
                AuditOutcome::Requested,
            )
            .is_err());
            assert_eq!(
                fs::read(other.app.join("protocol-audit")).unwrap(),
                b"obstruction"
            );
        }

        fn card(number: u8) -> String {
            let agent = format!("{number:02x}{}", "a".repeat(62));
            let card = Flashcard {
                schema: "darbot.agent-flashcard".into(),
                version: 1,
                glyph: char::from_u32(0x2800 + u32::from(number))
                    .unwrap()
                    .to_string(),
                agent,
                sessions: Summary {
                    known: Some(3),
                    acp: Some(2),
                    archive_only: Some(1),
                    coverage: Coverage::Complete,
                },
                observation: Some(Observation {
                    kind: Kind::Acp,
                    session: "b".repeat(64),
                    updated_at: Some("2026-11-17T17:12:06.414Z".into()),
                    source_feedback: Some(Feedback {
                        pinned: true,
                        reviewed: false,
                        dismissed: false,
                    }),
                    source_evidence: Some(Evidence {
                        verified_points: 2,
                        verified_commits: 1,
                    }),
                }),
            };
            serde_json::to_string(&card).unwrap()
        }

        #[test]
        fn validated_round_trip_preserves_typed_card_order_and_unicode_rows() {
            let f = Fixture::new();
            let text = card(255);
            let ledger = Ledger {
                decisions: vec![(
                    "row 😀 \" \n".into(),
                    Feedback {
                        pinned: true,
                        ..Feedback::default()
                    },
                )],
                references: vec![Flashcard::parse(&text).unwrap()],
                ..Ledger::default()
            };
            let legacy = serde_json::to_string_pretty(&ledger).unwrap();
            let result = f.edit(Mutation::Migrate { ledger: legacy });
            assert!(result.committed);
            assert_eq!(result.json, ledger.json().unwrap());
            assert!(result.json.contains(&text));
            assert_eq!(read(&f.app, "agent").unwrap().unwrap(), result.json);
            assert_eq!(fs::read_to_string(f.path("agent")).unwrap(), result.json);
            assert_eq!(f.path("agent").file_name().unwrap().len(), 69);
        }

        #[test]
        fn absent_reads_and_missing_promotions_do_not_create_storage() {
            let f = Fixture::new();
            assert!(read(&f.app, "agent").unwrap().is_none());
            let result = f.edit(Mutation::PromoteDraft {
                conversation: "none".into(),
                canonical_session: "session".into(),
            });
            assert!(!result.committed);
            assert_eq!(result.json, Ledger::default().json().unwrap());
            assert!(!f.app.exists());
            assert!(
                !f.edit(Mutation::ForgetReference { card: card(1) })
                    .committed
            );
            assert!(!f.app.exists());
            assert!(
                f.edit(Mutation::Migrate {
                    ledger: Ledger::default().json().unwrap()
                })
                .committed
            );
            assert!(read(&f.app, "agent").unwrap().is_some());
        }

        #[test]
        fn migration_never_overwrites_an_existing_valid_destination() {
            let f = Fixture::new();
            let original = "{ \"references\": [], \"decisions\": [[\"saved\", {\"dismissed\":false,\"reviewed\":true,\"pinned\":false}]], \"version\": 1 }";
            f.seed("agent", original);
            let outcome = f.edit(Mutation::Migrate {
                ledger: Ledger::default().json().unwrap(),
            });
            assert!(!outcome.committed);
            assert_eq!(
                outcome.json,
                Ledger::parse(original).unwrap().json().unwrap()
            );
            assert_eq!(fs::read_to_string(f.path("agent")).unwrap(), original);
            assert!(mutate(
                &f.app,
                "agent",
                Mutation::Migrate {
                    ledger: "{}".into()
                }
            )
            .is_err());
            assert_eq!(fs::read_to_string(f.path("agent")).unwrap(), original);
        }

        #[test]
        fn legacy_numbers_and_escapes_normalize_without_relaxing_card_exchange() {
            let f = Fixture::new();
            let raw = format!(
                "{{\"version\":1e0,\"decisions\":[],\"references\":[{}]}}",
                card(1)
                    .replace("\"known\":3", "\"known\":3.0")
                    .replace("\"verifiedPoints\":2", "\"verifiedPoints\":2e0")
                    .replace("⠁", "\\u2801")
            );
            let outcome = f.edit(Mutation::Migrate { ledger: raw });
            assert!(outcome.committed);
            assert!(outcome.json.contains(&card(1)));
            let g = Fixture::new();
            assert!(mutate(
                &g.app,
                "agent",
                Mutation::Migrate {
                    ledger: "{}".into()
                }
            )
            .is_err());
            assert!(!g.app.exists());
        }

        #[test]
        fn corrupt_future_unknown_and_duplicate_input_preserves_original_bytes() {
            let f = Fixture::new();
            for raw in [
                "{",
                r#"{"version":2,"decisions":[],"references":[]}"#,
                r#"{"version":1,"decisions":[],"references":[],"instructions":"no"}"#,
                r#"{"version":1,"version":1,"decisions":[],"references":[]}"#,
                r#"{"version":1,"decisions":[["a",{"pinned":true,"pinned":false,"reviewed":false,"dismissed":false}]],"references":[]}"#,
                r#"{"version":1,"decisions":[["a",{"pinned":true,"reviewed":false,"dismissed":false}],["a",{"pinned":false,"reviewed":false,"dismissed":false}]],"references":[]}"#,
            ] {
                f.seed("agent", raw);
                assert!(read(&f.app, "agent").is_err());
                assert!(mutate(&f.app, "agent", toggle("row", DecisionField::Pinned)).is_err());
                assert!(mutate(
                    &f.app,
                    "agent",
                    Mutation::Migrate {
                        ledger: Ledger::default().json().unwrap()
                    }
                )
                .is_err());
                assert_eq!(fs::read_to_string(f.path("agent")).unwrap(), raw);
            }
            fs::write(f.path("agent"), [0xff, 0xfe]).unwrap();
            assert!(read(&f.app, "agent").is_err());
            assert!(mutate(&f.app, "agent", toggle("row", DecisionField::Pinned)).is_err());
            assert_eq!(fs::read(f.path("agent")).unwrap(), [0xff, 0xfe]);
        }

        #[test]
        fn pin_review_dismiss_toggle_independently_and_remove_all_false() {
            let f = Fixture::new();
            f.edit(toggle("row", DecisionField::Pinned));
            f.edit(toggle("row", DecisionField::Reviewed));
            f.edit(toggle("row", DecisionField::Dismissed));
            assert_eq!(
                f.ledger().decisions[0].1,
                Feedback {
                    pinned: true,
                    reviewed: true,
                    dismissed: true
                }
            );
            f.edit(toggle("row", DecisionField::Pinned));
            f.edit(toggle("row", DecisionField::Reviewed));
            assert_eq!(
                f.ledger().decisions[0].1,
                Feedback {
                    pinned: false,
                    reviewed: false,
                    dismissed: true
                }
            );
            f.edit(toggle("row", DecisionField::Dismissed));
            assert!(f.ledger().decisions.is_empty());
        }

        #[test]
        fn promotion_uses_tuple_keys_and_target_wins() {
            let f = Fixture::new();
            let conversation = "conversation\"😀";
            let session = "provider:canonical";
            let from = serde_json::to_string(&["draft", conversation]).unwrap();
            let to = serde_json::to_string(&["session", session]).unwrap();
            f.edit(toggle(&from, DecisionField::Pinned));
            f.edit(Mutation::PromoteDraft {
                conversation: conversation.into(),
                canonical_session: session.into(),
            });
            assert_eq!(f.ledger().decisions[0].0, to);
            f.edit(toggle(&from, DecisionField::Dismissed));
            f.edit(Mutation::PromoteDraft {
                conversation: conversation.into(),
                canonical_session: session.into(),
            });
            let ledger = f.ledger();
            assert_eq!(ledger.decisions.len(), 1);
            assert_eq!(
                ledger.decisions[0],
                (
                    to,
                    Feedback {
                        pinned: true,
                        ..Feedback::default()
                    }
                )
            );
        }

        #[test]
        fn keep_canonicalizes_deduplicates_and_forget_removes_exact_match() {
            let f = Fixture::new();
            let text = card(1);
            assert!(
                f.edit(Mutation::KeepReference { card: text.clone() })
                    .committed
            );
            // JSON input may be pretty-printed or reordered, just like parseFlashcard.
            let reordered = serde_json::to_string_pretty(
                &serde_json::from_str::<serde_json::Value>(&text).unwrap(),
            )
            .unwrap();
            assert!(
                !f.edit(Mutation::KeepReference {
                    card: reordered.clone()
                })
                .committed
            );
            f.edit(Mutation::KeepReference { card: card(2) });
            assert_eq!(f.ledger().references.len(), 2);
            f.edit(Mutation::ForgetReference { card: reordered });
            assert_eq!(
                f.ledger().references,
                vec![Flashcard::parse(&card(2)).unwrap()]
            );
            assert!(!f.edit(Mutation::ForgetReference { card: text }).committed);
        }

        #[test]
        fn strict_flashcard_fields_nulls_integers_hashes_and_consistency() {
            let valid = card(1);
            for invalid in [
                valid.replace("\"observation\":", "\"extra\":null,\"observation\":"),
                valid.replace("\"version\":1", "\"version\":1,\"version\":1"),
                valid.replace("\"version\":1", "\"version\":1.0"),
                valid.replace("\"version\":1", "\"version\":1e0"),
                valid.replace("\"known\":3", "\"known\":9007199254740992"),
                valid.replace("\"known\":3", "\"known\":4"),
                valid.replace("\"known\":3", "\"known\":null"),
                valid.replace("\"acp\":2,", ""),
                valid.replace("\"coverage\":\"complete\"", "\"coverage\":\"uncollected\""),
                valid.replace("\"kind\":\"acp\"", "\"kind\":\"archive\""),
                valid.replace("\"verifiedCommits\":1", "\"verifiedCommits\":0"),
                valid.replace("\"verifiedPoints\":2", "\"verifiedPoints\":-0"),
                valid.replace(
                    "\"sourceEvidence\":",
                    "\"instructions\":\"do something\",\"sourceEvidence\":",
                ),
                valid.replace("2026-11-17T17:12:06.414Z", "2026-11-17T17:12:60.414Z"),
                valid.replace("2026-11-17T17:12:06.414Z", "2026-02-30T17:12:06.414Z"),
                valid.replace("2026-11-17T17:12:06.414Z", "2026-11-17T17:12:06.414+00:00"),
                valid.replace("⠁", "⠂"),
                valid.replace("⠁", "\\u2801"),
                valid.replace("aaaaaaaa", "AAAAaaaa"),
            ] {
                assert!(
                    Flashcard::parse(&invalid).is_err(),
                    "accepted invalid Flashcard: {invalid}"
                );
            }
            let mut value: serde_json::Value = serde_json::from_str(&valid).unwrap();
            for field in ["updatedAt", "sourceFeedback", "sourceEvidence"] {
                let mut missing = value.clone();
                missing["observation"]
                    .as_object_mut()
                    .unwrap()
                    .remove(field);
                assert!(Flashcard::parse(&missing.to_string()).is_err());
            }
            value["observation"] = serde_json::Value::Null;
            assert!(Flashcard::parse(&value.to_string()).is_ok());
            value.as_object_mut().unwrap().remove("observation");
            assert!(Flashcard::parse(&value.to_string()).is_err());
            assert!(timestamp("0000-01-01T00:00:00.000Z"));
            assert!(!timestamp("2026-11-17t17:12:06.414Z"));
        }

        #[test]
        fn mutation_wire_contract_rejects_unknown_fields_and_decisions() {
            for raw in [
                r#"{"kind":"toggle-decision","row":"x","field":"pinned","extra":true}"#,
                r#"{"kind":"toggle-decision","row":"x","field":"instructions"}"#,
                r#"{"kind":"keep-reference","card":"{}","path":"outside"}"#,
                r#"{"kind":"promote-draft","conversation":"a","canonical_session":"b"}"#,
            ] {
                assert!(serde_json::from_str::<Mutation>(raw).is_err());
            }
            assert!(serde_json::from_value::<Mutation>(json!({
                "kind": "promote-draft", "conversation": "a", "canonicalSession": "b"
            }))
            .is_ok());
        }

        #[test]
        fn opaque_names_isolate_arbitrary_stable_agent_identities() {
            let f = Fixture::new();
            let agents = ["../foreign/agent", "fleet:😀\0agent", "same", "Same"];
            for (i, agent) in agents.iter().enumerate() {
                mutate(
                    &f.app,
                    agent,
                    toggle(&format!("row-{i}"), DecisionField::Pinned),
                )
                .unwrap();
            }
            for (i, agent) in agents.iter().enumerate() {
                let ledger = Ledger::parse(&read(&f.app, agent).unwrap().unwrap()).unwrap();
                assert_eq!(ledger.decisions[0].0, format!("row-{i}"));
                assert_eq!(ledger.decisions.len(), 1);
            }
            assert_eq!(
                fs::read_dir(f.app.join("harvest")).unwrap().count(),
                agents.len()
            );
            assert!(read(&f.app, "").is_err());
            assert!(read(&f.app, &"x".repeat(MAX_UNITS + 1)).is_err());
        }

        #[test]
        fn concurrent_mutations_do_not_lose_updates() {
            let f = Fixture::new();
            std::thread::scope(|scope| {
                for i in 0..21 {
                    let app = &f.app;
                    scope.spawn(move || {
                        mutate(
                            app,
                            "agent",
                            toggle(&format!("row-{i}"), DecisionField::Reviewed),
                        )
                        .unwrap();
                        mutate(app, "agent", toggle("shared", DecisionField::Pinned)).unwrap();
                    });
                }
            });
            let ledger = f.ledger();
            assert_eq!(ledger.decisions.len(), 22);
            assert!(ledger
                .decisions
                .iter()
                .any(|(row, decision)| row == "shared" && decision.pinned));
        }

        #[test]
        fn decision_reference_and_utf16_limits_preserve_the_store() {
            let f = Fixture::new();
            let mut ledger = Ledger::default();
            for i in 0..MAX_DECISIONS {
                ledger.decisions.push((
                    i.to_string(),
                    Feedback {
                        pinned: true,
                        ..Feedback::default()
                    },
                ));
            }
            f.edit(Mutation::Migrate {
                ledger: ledger.json().unwrap(),
            });
            let before = fs::read(f.path("agent")).unwrap();
            assert!(mutate(&f.app, "agent", toggle("overflow", DecisionField::Pinned)).is_err());
            assert_eq!(fs::read(f.path("agent")).unwrap(), before);
            for i in 0..32 {
                f.edit(Mutation::KeepReference { card: card(i) });
            }
            let before = fs::read(f.path("agent")).unwrap();
            assert!(!f.edit(Mutation::KeepReference { card: card(0) }).committed);
            assert!(mutate(&f.app, "agent", Mutation::KeepReference { card: card(33) }).is_err());
            assert_eq!(fs::read(f.path("agent")).unwrap(), before);
            assert!(mutate(&f.app, "agent", toggle("", DecisionField::Reviewed)).is_err());
            assert!(mutate(
                &f.app,
                "agent",
                toggle(&"x".repeat(MAX_UNITS + 1), DecisionField::Reviewed)
            )
            .is_err());

            let g = Fixture::new();
            let mut ledger = Ledger {
                decisions: vec![(
                    "".into(),
                    Feedback {
                        pinned: true,
                        ..Feedback::default()
                    },
                )],
                ..Ledger::default()
            };
            // Three-byte BMP text exercises the full UTF-8 allowance, not a 2 MB byte cap.
            let overhead = serde_json::to_string(&ledger)
                .unwrap()
                .encode_utf16()
                .count();
            ledger.decisions[0].0 = "界".repeat(MAX_UNITS - overhead);
            let exact = ledger.json().unwrap();
            assert_eq!(exact.encode_utf16().count(), MAX_UNITS);
            assert!(exact.len() > MAX_UNITS * 2);
            g.edit(Mutation::Migrate {
                ledger: exact.clone(),
            });
            assert_eq!(read(&g.app, "agent").unwrap().unwrap(), exact);
            assert!(mutate(&g.app, "agent", toggle("extra", DecisionField::Pinned)).is_err());
            assert_eq!(fs::read_to_string(g.path("agent")).unwrap(), exact);
            assert!(!bounded(&"😀".repeat(MAX_UNITS / 2 + 1), MAX_UNITS));
        }

        #[test]
        fn atomic_replacement_partial_failure_and_readonly_preserve_live_bytes() {
            let f = Fixture::new();
            f.edit(toggle("row", DecisionField::Pinned));
            let dir = directory(&f.app, false).unwrap().unwrap();
            let name = filename("agent").unwrap();
            let original = fs::read_to_string(f.path("agent")).unwrap();
            let error = replace(&dir, &name, Some(&original), |file| {
                file.write_all(b"partial")?;
                Err(io::Error::other("synthetic partial-write failure"))
            })
            .unwrap_err();
            assert!(error.to_string().contains("partial-write"));
            assert_eq!(fs::read_to_string(f.path("agent")).unwrap(), original);
            assert_eq!(fs::read_dir(f.app.join("harvest")).unwrap().count(), 1);
            f.edit(toggle("row", DecisionField::Reviewed));
            let current = fs::read_to_string(f.path("agent")).unwrap();
            assert_ne!(current, original);
            assert!(replace(&dir, &name, Some(&original), |file| file
                .write_all(b"stale"))
            .is_err());
            assert_eq!(fs::read_to_string(f.path("agent")).unwrap(), current);
            let original_permissions = fs::metadata(f.path("agent")).unwrap().permissions();
            let mut readonly = original_permissions.clone();
            readonly.set_readonly(true);
            fs::set_permissions(f.path("agent"), readonly).unwrap();
            let result = mutate(&f.app, "agent", toggle("row", DecisionField::Dismissed));
            fs::set_permissions(f.path("agent"), original_permissions).unwrap();
            assert!(result.is_err());
            assert_eq!(fs::read_to_string(f.path("agent")).unwrap(), current);
            assert_eq!(fs::read_dir(f.app.join("harvest")).unwrap().count(), 1);
        }

        #[test]
        fn directories_and_hardlinked_ledgers_are_rejected_without_writing() {
            let f = Fixture::new();
            directory(&f.app, true).unwrap();
            fs::create_dir(f.path("agent")).unwrap();
            assert!(read(&f.app, "agent").is_err());
            assert!(mutate(&f.app, "agent", toggle("row", DecisionField::Pinned)).is_err());
            fs::remove_dir(f.path("agent")).unwrap();
            let foreign = f.base.join("foreign.json");
            let text = Ledger::default().json().unwrap();
            fs::write(&foreign, &text).unwrap();
            fs::hard_link(&foreign, f.path("agent")).unwrap();
            assert!(read(&f.app, "agent").is_err());
            assert!(mutate(&f.app, "agent", toggle("row", DecisionField::Pinned)).is_err());
            assert_eq!(fs::read_to_string(foreign).unwrap(), text);
        }

        #[test]
        fn ledger_and_directory_symlinks_are_rejected_where_supported() {
            let f = Fixture::new();
            directory(&f.app, true).unwrap();
            let foreign = f.base.join("foreign.json");
            let text = Ledger::default().json().unwrap();
            fs::write(&foreign, &text).unwrap();
            #[cfg(unix)]
            use std::os::unix::fs::{symlink as file_link, symlink as dir_link};
            #[cfg(windows)]
            use std::os::windows::fs::{symlink_dir as dir_link, symlink_file as file_link};
            let result = file_link(&foreign, f.path("agent"));
            #[cfg(windows)]
            if matches!(&result, Err(error) if error.raw_os_error() == Some(1314)) {
                return;
            }
            result.unwrap();
            assert!(read(&f.app, "agent").is_err());
            assert!(mutate(&f.app, "agent", toggle("row", DecisionField::Pinned)).is_err());
            fs::remove_file(f.path("agent")).unwrap();
            fs::remove_dir(f.app.join("harvest")).unwrap();
            let elsewhere = f.base.join("elsewhere");
            fs::create_dir(&elsewhere).unwrap();
            dir_link(&elsewhere, f.app.join("harvest")).unwrap();
            assert!(read(&f.app, "agent").is_err());
            assert!(mutate(&f.app, "agent", toggle("row", DecisionField::Pinned)).is_err());
            #[cfg(windows)]
            fs::remove_dir(f.app.join("harvest")).unwrap();
            #[cfg(unix)]
            fs::remove_file(f.app.join("harvest")).unwrap();
            fs::remove_dir(&f.app).unwrap();
            dir_link(&elsewhere, &f.app).unwrap();
            assert!(read(&f.app, "agent").is_err());
            assert!(mutate(&f.app, "agent", toggle("row", DecisionField::Pinned)).is_err());
            assert_eq!(fs::read_dir(elsewhere).unwrap().count(), 0);
            assert_eq!(fs::read_to_string(foreign).unwrap(), text);
            #[cfg(windows)]
            fs::remove_dir(&f.app).unwrap();
            #[cfg(unix)]
            fs::remove_file(&f.app).unwrap();
        }

        #[cfg(unix)]
        #[test]
        fn new_unix_store_and_files_are_private() {
            use std::os::unix::fs::PermissionsExt;
            let f = Fixture::new();
            f.edit(toggle("row", DecisionField::Pinned));
            for path in [&f.app, &f.app.join("harvest")] {
                assert_eq!(
                    fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o700
                );
            }
            assert_eq!(
                fs::metadata(f.path("agent")).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    #[ignore = "opt-in local archive census; aggregate output only"]
    fn live_census() {
        assert_eq!(
            std::env::var("DARBOT_LIVE_HISTORY_CENSUS").as_deref(),
            Ok("1")
        );
        let result = live_scan();
        println!(
            "scanned={} observed={} unknown={} dayour-swe={}",
            result.scanned_at,
            result.sessions.len(),
            result
                .sessions
                .iter()
                .filter(|s| s.agent_ids.is_empty())
                .count(),
            result
                .sessions
                .iter()
                .filter(|s| s.agent_ids.contains("dayour-swe"))
                .count()
        );
        for s in result.sources {
            println!(
                "{} indexed={} duplicates={} unreadable={} unattributed={} complete={} {}",
                s.id, s.indexed, s.duplicates, s.unreadable, s.unattributed, s.complete, s.status
            );
        }
    }

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = crate::test_support::temp_root("history-index");
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn put(&self, name: &str, data: impl AsRef<[u8]>) {
            let path = self.0.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, data).unwrap();
        }
        fn scan(&self) -> HistoryIndex {
            scan(Some(&self.0), Some(&self.0), Some(&self.0))
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn uncapped_and_provider_qualified_with_proven_app_mirrors() {
        let f = Fixture::new();
        let id = "550e8400-e29b-41d4-a716-446655440000";
        for i in 0..805 {
            f.put(
                &format!(".copilot/session-state/{i}/workspace.yaml"),
                format!("id: session-{i}\n"),
            );
        }
        let log = [
                    json!({"type":"session.start","data":{"sessionId":id}}),
                    json!({"type":"subagent.selected","data":{"agentName":"agency-rendered-0123456789abcdef0123456789abcdef:dayour-swe"}}),
                    json!({"type":"subagent.started","data":{"agentName":"reviewer"}}),
                    json!({"type":"subagent.selected","data":{"agentName":"dayour-swe"}}),
                    json!({"type":"user.message","data":{"agentName":"invented","content":"SENSITIVE-CONTENT"}}),
                ].map(|v|v.to_string()).join("\n");
        f.put(".copilot/session-state/local/events.jsonl", &log);
        f.put(".agentstudio/copilot/session-state/copy/events.jsonl", log);
        f.put(".copilot/clippyclaw-sessions/sessions-index.json",json!({"sessions":[
                    {"id":"local","agentId":"archivist"},{"id":"unrelated","backendOrigin":"gateway","backendId":id}
                ]}).to_string());
        f.put(
            ".copilot/m-sessions/sessions-index.json",
            json!({"sessions":[{"id":"local","backendOrigin":"copilot","backendId":id}]})
                .to_string(),
        );
        f.put(
            ".codex/archived_sessions/rollout-demo.jsonl",
            json!({"type":"session_meta","payload":{"id":id}}).to_string(),
        );
        f.put("graph3d-out/hp11d/copilot-history/session-inventory.csv",
                    format!("session_id,name,indexed,app_session\n{id},\"some\ntext\",True,False\nnot-a-session,other,True,False\n"));
        let result = f.scan();
        assert_eq!(result.sessions.len(), 808);
        let row = result
            .sessions
            .iter()
            .find(|s| s.id == format!("{CLI}:{id}"))
            .unwrap();
        assert_eq!(
            row.agent_ids.iter().map(String::as_str).collect::<Vec<_>>(),
            ["archivist", "dayour-swe", "reviewer"]
        );
        assert_eq!(row.source_ids.len(), 5);
        assert!(result
            .sessions
            .iter()
            .any(|s| s.id == format!("codex:{id}") && s.agent_ids.is_empty()));
        assert_eq!(result.sources[0].indexed, 806);
        assert_eq!(result.sources[0].unattributed, 805);
        assert_eq!(result.sources.last().unwrap().indexed, 1);
        let v = serde_json::to_value(result).unwrap();
        assert_eq!(v.as_object().unwrap().len(), 4);
        assert_eq!(v["schemaVersion"], 1);
        assert!(OffsetDateTime::parse(v["scannedAt"].as_str().unwrap(), &Rfc3339).is_ok());
        assert_eq!(v["sessions"][0].as_object().unwrap().len(), 6);
        assert_eq!(v["sources"][0].as_object().unwrap().len(), 8);
        for private in [
            "SENSITIVE-CONTENT",
            "invented",
            "resumable",
            "workspace.yaml",
            "not-a-session",
        ] {
            assert!(!v.to_string().contains(private));
        }
    }

    #[test]
    fn partial_records_preserve_next_line_not_guessed_identity() {
        let f = Fixture::new();
        let mut bytes = br#"{"type":"session.start","data":{"sessionId":"confirmed"}}"#.to_vec();
        bytes.extend_from_slice(b"\n{broken\n");
        bytes.resize(bytes.len() + RECORD_BYTES + 500, b'x');
        bytes.extend_from_slice(
            b"\n{\"type\":\"subagent.selected\",\"data\":{\"agentName\":\"late\"}}\n{\"type\":",
        );
        f.put(".copilot/session-state/a/events.jsonl", bytes);
        f.put(
            ".copilot/session-state/b/workspace.yaml",
            "id: do-not-join\n",
        );
        f.put(
            ".copilot/session-state/b/events.jsonl",
            br#"{"type":"session.start","data":{"sessionId":"wrong"}}"#,
        );
        f.put(
            ".copilot/session-state/c/events.jsonl",
            br#"{"type":"subagent.selected","data":{"agentName":"ghost"}}"#,
        );
        f.put(
            ".copilot/clippyclaw-sessions/sessions-index.json",
            "ENCRYPTED",
        );
        let r = f.scan();
        assert_eq!(r.sessions.len(), 1);
        assert_eq!(r.sessions[0].id, "copilot-cli:confirmed");
        assert!(r.sessions[0].agent_ids.contains("late"));
        assert!(!r.sources[0].complete && r.sources[0].unreadable > 0);
        assert!(r.sources[0].status.contains("oversize"));
        assert!(r.sources[1].status.contains("encrypted"));
        assert!(r
            .sources
            .iter()
            .find(|s| s.id == CLAUDE)
            .unwrap()
            .status
            .starts_with("unavailable"));
        assert!(!local(Path::new(r"\\offline-server\share")));
    }

    #[test]
    fn host_schemas_scope_authoritative_ids() {
        let f = Fixture::new();
        let uuid = "00000000-0000-0000-0000-000000000001";
        f.put(&format!(".claude/projects/project/{uuid}.jsonl"),json!({"type":"assistant","sessionId":uuid,"agentId":"child-1",
                    "message":{"content":[{"type":"tool_use","name":"Agent","input":{"subagent_type":"specialist","agentId":"injected"}}]}}).to_string());
        f.put(".codex/sessions/2026/01/01/rollout-test.jsonl",json!({"type":"session_meta","payload":{"id":uuid,
                    "source":{"subagent":{"thread_spawn":{"agent_role":"worker","agent_nickname":"not-authoritative"}}}}}).to_string());
        f.put(&format!("Code/User/workspaceStorage/space/chatSessions/{uuid}.jsonl"),[
                    json!({"kind":0,"v":{"sessionId":uuid,"requests":[]}}),
                    json!({"kind":2,"k":["requests"],"v":[{"agent":{"id":"ext.chat","name":"not-authoritative"}}]}),
                    json!({"kind":1,"k":["requests",0,"response",0,"agent"],"v":{"id":"injected"}})
                ].map(|v|v.to_string()).join("\n"));
        let r = f.scan();
        assert_eq!(r.sessions.len(), 3);
        let agents = |provider| {
            &r.sessions
                .iter()
                .find(|s| s.provider == provider)
                .unwrap()
                .agent_ids
        };
        assert_eq!(agents(VSC), &BTreeSet::from(["ext.chat".into()]));
        assert_eq!(
            agents(CLAUDE),
            &BTreeSet::from(["child-1".into(), "specialist".into()])
        );
        assert_eq!(agents(CODEX), &BTreeSet::from(["worker".into()]));
    }

    #[test]
    fn conflicting_aliases_and_unbound_agents_do_not_join_sessions() {
        let f = Fixture::new();
        f.put(".copilot/m-sessions/sessions-index.json",
                      br#"{"sessions":[{"id":"local","backendOrigin":"copilot","backendId":"one"},{"id":"local","sdkSessionId":"two"}]}"#);
        f.put(
            ".copilot/session-state/example/events.jsonl",
            concat!(
                "{\"type\":\"subagent.selected\",\"data\":{\"agentName\":\"wrong\"}}\n",
                "{\"type\":\"session.start\",\"data\":{\"sessionId\":\"one\"}}\n"
            ),
        );
        f.put(
            "graph3d-out/hp11d/copilot-history/session-inventory.csv",
            "session_id,name,indexed,app_session\none,folder-only,False,False\n",
        );
        let r = f.scan();
        assert_eq!(r.sessions.len(), 2);
        assert!(r
            .sessions
            .iter()
            .all(|s| s.source_ids.len() == 1 && s.agent_ids.is_empty()));
        assert!(r
            .sources
            .iter()
            .find(|s| s.id == CLIPPY)
            .unwrap()
            .status
            .contains("conflicting"));
        assert_eq!(r.sources.last().unwrap().indexed, 0);
    }

    #[test]
    fn linked_pool_is_not_traversed() {
        let f = Fixture::new();
        f.put("foreign/example/workspace.yaml", "id: foreign\n");
        fs::create_dir(f.0.join(".copilot")).unwrap();
        #[cfg(unix)]
        use std::os::unix::fs::symlink as link;
        #[cfg(windows)]
        use std::os::windows::fs::symlink_dir as link;
        let result = link(f.0.join("foreign"), f.0.join(".copilot/session-state"));
        #[cfg(windows)]
        if matches!(&result,Err(e) if e.raw_os_error()==Some(1314)) {
            return;
        }
        result.unwrap();
        let r = f.scan();
        assert!(r.sessions.is_empty());
        assert!(!r.sources[0].complete && r.sources[0].unreadable > 0);
    }

    #[test]
    fn one_refresh_for_overlapping_callers_then_reuse_completed_result() {
        use std::sync::mpsc;
        let cache = IndexCache::default();
        let (entered, started) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let mut first = cache.flight(true, move || {
            entered.send(()).unwrap();
            gate.recv().unwrap();
            finish(Vec::new())
        });
        started
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let second = cache.flight(true, || panic!("redundant refresh"));
        assert!(first.same_channel(&second));
        release.send(()).unwrap();
        tauri::async_runtime::block_on(first.changed()).unwrap();
        assert!(cache
            .flight(false, || panic!("unexpected cache miss"))
            .same_channel(&first));
        assert!(!cache
            .flight(true, || finish(Vec::new()))
            .same_channel(&first));
    }
}
