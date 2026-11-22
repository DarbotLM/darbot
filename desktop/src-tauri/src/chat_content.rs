//! Human-selected, memory-only prompt content. No filesystem or capture grants enter ACP/MCP.
use crate::problem::Problem;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};

const FILE_LIMIT: usize = 2 * 1024 * 1024;
const STORE_LIMIT: usize = 16 * 1024 * 1024;
const TURN_LIMIT: usize = 3 * 1024 * 1024;
const ITEM_LIMIT: usize = 8;
const FRAME_INTERVAL: Duration = Duration::from_secs(3);
const LEASE_TIMEOUT: Duration = Duration::from_secs(15);

fn fail(message: &str) -> Problem {
    Problem::plain(message)
}
fn opaque_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 24];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContentOwner {
    pub conversation_id: String,
    pub agent_id: String,
}

pub fn owner_for_session(
    conversation_id: Option<String>,
    agent: Option<&str>,
) -> Result<Option<ContentOwner>, Problem> {
    conversation_id
        .map(|conversation_id| {
            let agent_id = agent.unwrap_or_default().to_owned();
            if conversation_id.is_empty() || conversation_id.len() > 1024 || agent_id.len() > 1024 {
                return Err(fail("Invalid attachment conversation identity."));
            }
            Ok(ContentOwner {
                conversation_id,
                agent_id,
            })
        })
        .transpose()
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    pub bytes: usize,
    pub provenance: String,
    pub preview: String,
}

#[derive(Clone)]
struct Entry {
    owner: ContentOwner,
    window: String,
    attachment: Attachment,
    content: Vec<u8>,
    screen: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureSource {
    pub id: String,
    pub name: String,
    #[cfg(windows)]
    #[serde(skip)]
    handle: isize,
    #[cfg(windows)]
    #[serde(skip)]
    process: u32,
    #[cfg(windows)]
    #[serde(skip)]
    identity: Option<Arc<crate::native_capture::WindowIdentity>>,
}

struct CaptureLease {
    id: String,
    source: CaptureSource,
    last_frame: Instant,
    attachment_id: String,
}

#[derive(Default)]
struct Store {
    active: Option<(String, ContentOwner)>,
    epoch: String,
    entries: HashMap<String, Entry>,
    sources: HashMap<String, CaptureSource>,
    lease: Option<CaptureLease>,
    pending_send: Option<(String, String)>,
    bound_session: Option<(String, ContentOwner, String)>,
    capture: Option<Arc<crate::native_capture::OwnedCapture>>,
}

#[derive(Default)]
pub struct ChatContent(Mutex<Store>);

struct CaptureOperation<'a> {
    state: &'a ChatContent,
    worker: crate::native_capture::CancelOnDrop,
    window: String,
    owner: ContentOwner,
    epoch: String,
}

impl<'a> CaptureOperation<'a> {
    fn begin(
        state: &'a ChatContent,
        window: &str,
        owner: &ContentOwner,
        epoch: &str,
    ) -> Result<Self, Problem> {
        let mut store = state.0.lock().unwrap();
        store.check(window, owner)?;
        if store.epoch != epoch {
            return Err(fail("Capture consent was revoked."));
        }
        if store.capture.is_some() {
            return Err(fail("A selected-window capture is already running."));
        }
        // This Arc identifies the entire operation, including approval and post-worker commit.
        let worker = Arc::new(crate::native_capture::OwnedCapture::default());
        store.capture = Some(Arc::clone(&worker));
        Ok(Self {
            state,
            worker: crate::native_capture::CancelOnDrop(worker),
            window: window.into(),
            owner: owner.clone(),
            epoch: epoch.into(),
        })
    }

    fn check(&self, store: &Store) -> Result<(), Problem> {
        store.check(&self.window, &self.owner)?;
        if store.epoch != self.epoch
            || store
                .capture
                .as_ref()
                .is_none_or(|active| !Arc::ptr_eq(active, &self.worker.0))
        {
            return Err(fail(
                "Capture access was revoked before the frame was ready.",
            ));
        }
        Ok(())
    }

    fn finish<T>(
        &self,
        commit: impl FnOnce(&mut Store) -> Result<T, Problem>,
    ) -> Result<T, Problem> {
        // Even discard must reap before making a new operation eligible.
        self.worker.0.cancel()?;
        let mut store = self.state.0.lock().unwrap();
        self.check(&store)?;
        let result = commit(&mut store);
        if store
            .capture
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, &self.worker.0))
        {
            store.capture = None;
        }
        result
    }
}

impl Drop for CaptureOperation<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.worker.0.cancel() {
            eprintln!("Darbot capture cleanup failed: {error:?}");
            return;
        }
        let mut store = self.state.0.lock().unwrap();
        if store
            .capture
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, &self.worker.0))
        {
            store.capture = None;
        }
    }
}

async fn capture_native(
    operation: &CaptureOperation<'_>,
    source: CaptureSource,
) -> Result<Vec<u8>, Problem> {
    operation.check(&operation.state.0.lock().unwrap())?;
    let worker = Arc::clone(&operation.worker.0);
    tauri::async_runtime::spawn_blocking(move || platform::capture(&worker, &source))
        .await
        .map_err(|_| fail("Windows capture did not finish."))?
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentSnapshot {
    pub attachments: Vec<Attachment>,
    pub lease_id: Option<String>,
}

impl Store {
    fn check_session(
        &self,
        window: &str,
        owner: &ContentOwner,
        session: &str,
    ) -> Result<(), Problem> {
        if self
            .bound_session
            .as_ref()
            .is_none_or(|(w, o, s)| w != window || o != owner || s != session)
        {
            return Err(fail(
                "The attachment conversation is not bound to this ACP session. Reopen it locally.",
            ));
        }
        Ok(())
    }

    fn check(&self, window: &str, owner: &ContentOwner) -> Result<(), Problem> {
        if self.active.as_ref() != Some(&(window.to_owned(), owner.clone())) {
            return Err(fail(
                "Attachment ownership changed. Reopen this conversation locally.",
            ));
        }
        Ok(())
    }

    fn stop_screen(&mut self) {
        self.stop_capture();
        self.epoch = opaque_id();
        self.lease = None;
        self.sources.clear();
        self.entries.retain(|_, entry| !entry.screen);
    }

    fn stop_capture(&mut self) {
        if let Some(worker) = &self.capture {
            if let Err(error) = worker.cancel() {
                eprintln!("Darbot capture revocation failed: {error:?}");
            } else {
                self.capture = None;
            }
        }
    }

    fn expire(&mut self) {
        if self
            .lease
            .as_ref()
            .is_some_and(|lease| lease.last_frame.elapsed() > LEASE_TIMEOUT)
        {
            self.stop_screen();
        }
    }

    fn snapshot(&mut self, window: &str, owner: &ContentOwner) -> Result<ContentSnapshot, Problem> {
        self.check(window, owner)?;
        self.expire();
        Ok(ContentSnapshot {
            attachments: self
                .entries
                .values()
                .filter(|entry| entry.window == window && &entry.owner == owner)
                .map(|entry| entry.attachment.clone())
                .collect(),
            lease_id: self.lease.as_ref().map(|lease| lease.id.clone()),
        })
    }

    fn insert(&mut self, window: &str, owner: &ContentOwner, entry: Entry) -> Result<(), Problem> {
        self.check(window, owner)?;
        let count = self
            .entries
            .values()
            .filter(|e| e.window == window && &e.owner == owner)
            .count();
        let bytes: usize = self.entries.values().map(|e| e.content.len()).sum();
        if count >= ITEM_LIMIT || bytes + entry.content.len() > STORE_LIMIT {
            return Err(fail("Attachment memory is full. Remove attachments before adding more (8 per conversation, 16 MiB total)."));
        }
        self.entries.insert(entry.attachment.id.clone(), entry);
        Ok(())
    }

    fn selected(
        &mut self,
        window: &str,
        owner: &ContentOwner,
        ids: &[String],
    ) -> Result<Vec<Entry>, Problem> {
        self.check(window, owner)?;
        self.expire();
        if ids.is_empty()
            || ids.len() > ITEM_LIMIT
            || ids.iter().collect::<HashSet<_>>().len() != ids.len()
        {
            return Err(fail("Choose between 1 and 8 distinct attachments."));
        }
        ids.iter()
            .map(|id| {
                self.entries
                    .get(id)
                    .filter(|entry| entry.window == window && &entry.owner == owner)
                    .cloned()
                    .ok_or_else(|| {
                        fail("An attachment was removed or revoked. Review the draft again.")
                    })
            })
            .collect()
    }
}

impl ChatContent {
    pub fn bind_session(&self, window: &str, owner: ContentOwner, session: &str) {
        let mut store = self.0.lock().unwrap();
        let binding = (window.to_owned(), owner, session.to_owned());
        if store
            .bound_session
            .as_ref()
            .is_some_and(|old| old != &binding)
        {
            store.stop_screen();
        }
        store.bound_session = Some(binding);
    }

    pub fn retire_session(&self, session: &str) {
        let mut store = self.0.lock().unwrap();
        if store
            .bound_session
            .as_ref()
            .is_some_and(|(_, _, id)| id == session)
        {
            if store
                .bound_session
                .as_ref()
                .is_some_and(|(window, owner, _)| {
                    store.active.as_ref() == Some(&(window.clone(), owner.clone()))
                })
            {
                store.stop_screen();
            }
            store.bound_session = None;
        }
    }

    pub fn cancel_session(&self, session_id: &str) -> bool {
        let mut store = self.0.lock().unwrap();
        let pending = store
            .pending_send
            .as_ref()
            .is_some_and(|(session, _)| session == session_id);
        let capturing = store
            .bound_session
            .as_ref()
            .is_some_and(|(window, owner, session)| {
                session == session_id
                    && store.active.as_ref() == Some(&(window.clone(), owner.clone()))
                    && (store.capture.is_some() || store.lease.is_some())
            });
        if pending || capturing {
            store.stop_screen();
            store.pending_send = None;
            true
        } else {
            false
        }
    }

    pub fn revoke_window_content(&self, window: &str) {
        let mut store = self.0.lock().unwrap();
        if store
            .active
            .as_ref()
            .is_some_and(|(active, _)| active == window)
        {
            store.stop_screen();
            store.active = None;
            store.pending_send = None;
            store.epoch = opaque_id();
        }
        store.entries.retain(|_, entry| entry.window != window);
        // Hiding to tray revokes content, not the surviving ACP session's ownership.
        // Actual session closure retires the binding through retire_session.
    }

    pub fn remote_send_allowed(&self) -> Result<(), Problem> {
        let mut store = self.0.lock().unwrap();
        store.expire();
        if store.lease.is_some()
            || !store.sources.is_empty()
            || store
                .entries
                .values()
                .any(|e| store.active.as_ref() == Some(&(e.window.clone(), e.owner.clone())))
        {
            return Err(fail("Send attachments locally after reviewing them. App-control approval covers text only."));
        }
        Ok(())
    }
}

fn validate_png(bytes: &[u8]) -> Result<(), Problem> {
    if bytes.len() > FILE_LIMIT
        || !bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || !bytes.ends_with(b"\x00\x00\x00\x00IEND\xae\x42\x60\x82")
    {
        return Err(fail("Choose a valid PNG image no larger than 2 MiB."));
    }
    let mut offset = 8usize;
    loop {
        let header = bytes
            .get(offset..offset + 8)
            .ok_or_else(|| fail("Invalid PNG chunk."))?;
        let length = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        let end = offset
            .checked_add(12)
            .and_then(|start| start.checked_add(length))
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| fail("Invalid PNG chunk length."))?;
        if &header[4..] == b"IEND" {
            if length != 0 || end != bytes.len() {
                return Err(fail(
                    "PNG attachments cannot contain trailing non-image data.",
                ));
            }
            break;
        }
        offset = end;
    }
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_limits(png::Limits {
        bytes: 32 * 1024 * 1024,
    });
    let mut reader = decoder
        .read_info()
        .map_err(|_| fail("The PNG image is invalid."))?;
    let info = reader.info();
    if info.width == 0
        || info.height == 0
        || info.width > 4096
        || info.height > 4096
        || u64::from(info.width) * u64::from(info.height) > 4_194_304
        || info.animation_control.is_some()
    {
        return Err(fail(
            "Choose a static PNG image up to 4096 pixels per side and 4 megapixels.",
        ));
    }
    let mut decoded = vec![0; reader.output_buffer_size()];
    reader
        .next_frame(&mut decoded)
        .map_err(|_| fail("The PNG image is incomplete or invalid."))?;
    reader
        .finish()
        .map_err(|_| fail("The PNG image is incomplete or invalid."))?;
    Ok(())
}

fn entry(
    window: &str,
    owner: &ContentOwner,
    name: String,
    mime: &str,
    bytes: Vec<u8>,
    provenance: &str,
    screen: bool,
) -> Result<Entry, Problem> {
    if bytes.is_empty() || bytes.len() > FILE_LIMIT || name.len() > 512 {
        return Err(fail(
            "Attachments must be nonempty and no larger than 2 MiB.",
        ));
    }
    let preview = match mime {
        "image/png" => {
            validate_png(&bytes)?;
            format!(
                "data:image/png;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(&bytes)
            )
        }
        "text/plain" | "text/markdown" | "application/json" | "text/csv" => {
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| fail("Text attachments must contain valid UTF-8."))?;
            if text
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
            {
                return Err(fail("This file contains binary data, not supported text."));
            }
            if mime == "application/json" {
                serde_json::from_str::<Value>(text)
                    .map_err(|_| fail("This JSON file is not valid JSON."))?;
            }
            text.chars().take(400).collect()
        }
        _ => return Err(fail("Unsupported attachment MIME type.")),
    };
    Ok(Entry {
        window: window.to_owned(),
        owner: owner.clone(),
        screen,
        attachment: Attachment {
            id: opaque_id(),
            name,
            mime_type: mime.into(),
            bytes: bytes.len(),
            provenance: provenance.into(),
            preview,
        },
        content: bytes,
    })
}

fn prompt_blocks(
    prompt: &str,
    entries: &[Entry],
    image: bool,
    embedded: bool,
) -> Result<Vec<Value>, Problem> {
    let mut blocks = Vec::new();
    if prompt.len() > 128_000 {
        return Err(fail("The prompt exceeds the content limit."));
    }
    if !prompt.trim().is_empty() {
        blocks.push(json!({"type":"text","text":prompt}));
    }
    for entry in entries {
        if entry.attachment.mime_type == "image/png" {
            if !image {
                return Err(fail(
                    "This connected agent did not negotiate image prompts. The draft was kept.",
                ));
            }
            blocks.push(json!({"type":"image","mimeType":"image/png",
                "data":base64::engine::general_purpose::STANDARD.encode(&entry.content)}));
        } else {
            if !embedded {
                return Err(fail(
                    "This connected agent did not negotiate embedded context. The draft was kept.",
                ));
            }
            let text = std::str::from_utf8(&entry.content)
                .map_err(|_| fail("Invalid text attachment."))?;
            blocks.push(json!({"type":"resource","resource":{
                "uri":format!("darbot-attachment:///{}",entry.attachment.id),
                "mimeType":entry.attachment.mime_type,"text":text}}));
        }
    }
    if blocks.is_empty()
        || serde_json::to_vec(&blocks)
            .map_err(|_| fail("Cannot encode prompt content."))?
            .len()
            > TURN_LIMIT
    {
        return Err(fail("The complete prompt must be nonempty and fit the 3 MiB encoded turn limit. Remove attachments and retry."));
    }
    Ok(blocks)
}

#[tauri::command]
pub fn chat_content_activate(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, ChatContent>,
    owner: Option<ContentOwner>,
) -> Result<Option<ContentSnapshot>, Problem> {
    let mut store = state.0.lock().unwrap();
    if let Some(owner) = owner {
        if owner.conversation_id.is_empty()
            || owner.conversation_id.len() > 1024
            || owner.agent_id.len() > 1024
        {
            return Err(fail("Invalid attachment owner."));
        }
        let next = (window.label().to_owned(), owner.clone());
        if store.active.as_ref() != Some(&next) {
            store.stop_screen();
            store.epoch = opaque_id();
            store.active = Some(next);
        }
        store.snapshot(window.label(), &owner).map(Some)
    } else {
        store.stop_screen();
        store.active = None;
        store.epoch = opaque_id();
        Ok(None)
    }
}

#[tauri::command]
pub async fn chat_content_pick(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, ChatContent>,
    owner: ContentOwner,
) -> Result<ContentSnapshot, Problem> {
    let epoch = {
        let store = state.0.lock().unwrap();
        store.check(window.label(), &owner)?;
        store.epoch.clone()
    };
    let picker_window = window.clone();
    let paths = tauri::async_runtime::spawn_blocking(move || {
        picker_window
            .dialog()
            .file()
            .set_parent(&picker_window)
            .set_title("Darbot — attach only these selected files")
            .add_filter(
                "PNG or UTF-8 text",
                &[
                    "png", "txt", "md", "json", "csv", "rs", "ts", "tsx", "js", "css", "html",
                    "log", "yaml", "yml",
                ],
            )
            .blocking_pick_files()
    })
    .await
    .map_err(|_| fail("File selection did not finish."))?;
    let mut pending = Vec::new();
    if let Some(paths) = paths {
        if paths.len() > ITEM_LIMIT {
            return Err(fail("Select at most 8 files."));
        }
        for path in paths {
            let path = path.into_path().map_err(|_| fail("Choose a local file."))?;
            let extension = path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let mime = match extension.as_str() {
                "png" => "image/png",
                "md" => "text/markdown",
                "json" => "application/json",
                "csv" => "text/csv",
                "txt" | "rs" | "ts" | "tsx" | "js" | "css" | "html" | "log" | "yaml" | "yml" => {
                    "text/plain"
                }
                _ => {
                    return Err(fail(
                        "Unsupported file type. Choose PNG or supported UTF-8 text.",
                    ))
                }
            };
            let file = std::fs::File::open(&path)
                .map_err(|_| fail("The selected file could not be opened."))?;
            let metadata = file
                .metadata()
                .map_err(|_| fail("The selected file could not be inspected."))?;
            if !metadata.is_file() || metadata.len() > FILE_LIMIT as u64 {
                return Err(fail("Choose regular files no larger than 2 MiB."));
            }
            let mut bytes = Vec::new();
            file.take(FILE_LIMIT as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| fail("The selected file could not be read."))?;
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| fail("Unsupported filename."))?
                .to_owned();
            pending.push(entry(
                window.label(),
                &owner,
                name,
                mime,
                bytes,
                "Selected file",
                false,
            )?);
        }
    }
    let mut store = state.0.lock().unwrap();
    store.check(window.label(), &owner)?;
    if epoch != store.epoch {
        return Err(fail("File selection was cancelled by navigation."));
    }
    let count = store
        .entries
        .values()
        .filter(|e| e.window == window.label() && e.owner == owner)
        .count();
    let size: usize = store
        .entries
        .values()
        .chain(pending.iter())
        .map(|e| e.content.len())
        .sum();
    if count + pending.len() > ITEM_LIMIT || size > STORE_LIMIT {
        return Err(fail(
            "Attachment memory is full. No selected files were added.",
        ));
    }
    for entry in pending {
        store.insert(window.label(), &owner, entry)?;
    }
    store.snapshot(window.label(), &owner)
}

#[tauri::command]
pub fn chat_content_text(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, ChatContent>,
    owner: ContentOwner,
    text: String,
) -> Result<ContentSnapshot, Problem> {
    let entry = entry(
        window.label(),
        &owner,
        "Selected text".into(),
        "text/plain",
        text.into_bytes(),
        "Text explicitly selected by you",
        false,
    )?;
    let mut store = state.0.lock().unwrap();
    store.insert(window.label(), &owner, entry)?;
    store.snapshot(window.label(), &owner)
}

#[tauri::command]
pub fn chat_content_remove(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, ChatContent>,
    owner: ContentOwner,
    id: String,
) -> Result<ContentSnapshot, Problem> {
    let mut store = state.0.lock().unwrap();
    store.selected(window.label(), &owner, std::slice::from_ref(&id))?;
    store.entries.remove(&id);
    if store
        .lease
        .as_ref()
        .is_some_and(|lease| lease.attachment_id == id)
    {
        store.stop_capture();
        store.lease = None;
    }
    store.snapshot(window.label(), &owner)
}

#[tauri::command]
pub fn chat_capture_sources(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, ChatContent>,
    owner: ContentOwner,
) -> Result<Vec<CaptureSource>, Problem> {
    let mut store = state.0.lock().unwrap();
    store.check(window.label(), &owner)?;
    let sources = platform::sources()?;
    store.sources = sources
        .iter()
        .map(|source| (source.id.clone(), source.clone()))
        .collect();
    Ok(sources)
}

#[tauri::command]
pub async fn chat_capture_start(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, ChatContent>,
    owner: ContentOwner,
    source_id: String,
    share: bool,
) -> Result<ContentSnapshot, Problem> {
    let (source, epoch) = {
        let store = state.0.lock().unwrap();
        store.check(window.label(), &owner)?;
        (
            store
                .sources
                .get(&source_id)
                .cloned()
                .ok_or_else(|| fail("Refresh and choose a current capture source."))?,
            store.epoch.clone(),
        )
    };
    let operation = CaptureOperation::begin(&state, window.label(), &owner, &epoch)?;
    let confirm_window = window.clone();
    let question = format!("{} \"{}\" for agent \"{}\", conversation \"{}\"?\n\nOnly this window will be captured. Review the preview before sending. {} Nothing is sent automatically.",
        if share { "Share snapshots of" } else { "Capture" }, source.name,
        if owner.agent_id.is_empty() { "Copilot CLI" } else { &owner.agent_id }, owner.conversation_id,
        if share { "A local preview refreshes every 3 seconds until Stop, navigation or inactivity." } else { "One image is added to your draft." });
    let approved = tauri::async_runtime::spawn_blocking(move || {
        confirm_window
            .dialog()
            .message(question)
            .parent(&confirm_window)
            .title("Darbot — approve screen access")
            .buttons(MessageDialogButtons::OkCancelCustom(
                "Capture selected window".into(),
                "Cancel".into(),
            ))
            .blocking_show()
    })
    .await
    .map_err(|_| fail("Capture approval did not finish."))?;
    {
        let store = state.0.lock().unwrap();
        operation.check(&store)?;
    }
    if !approved {
        return operation.finish(|store| {
            store.stop_screen();
            store.snapshot(window.label(), &owner)
        });
    }
    let captured = capture_native(&operation, source.clone()).await;
    operation.finish(|store| commit_capture(store, window.label(), &owner, source, share, captured))
}

fn commit_capture(
    store: &mut Store,
    window: &str,
    owner: &ContentOwner,
    source: CaptureSource,
    share: bool,
    captured: Result<Vec<u8>, Problem>,
) -> Result<ContentSnapshot, Problem> {
    let bytes = match captured {
        Ok(bytes) => bytes,
        Err(error) => {
            store.stop_screen();
            return Err(error);
        }
    };
    let image = entry(
        window,
        owner,
        source.name.clone(),
        "image/png",
        bytes,
        if share {
            "Shared window snapshot"
        } else {
            "Captured window"
        },
        true,
    )?;
    let attachment_id = image.attachment.id.clone();
    store.stop_screen();
    store.insert(window, owner, image)?;
    if share {
        store.lease = Some(CaptureLease {
            id: opaque_id(),
            source,
            last_frame: Instant::now(),
            attachment_id,
        });
    }
    store.snapshot(window, owner)
}

#[tauri::command]
pub async fn chat_capture_frame(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, ChatContent>,
    owner: ContentOwner,
    lease_id: String,
) -> Result<ContentSnapshot, Problem> {
    let (id, source, epoch) = {
        let mut store = state.0.lock().unwrap();
        store.check(window.label(), &owner)?;
        store.expire();
        let lease = store
            .lease
            .as_ref()
            .filter(|lease| lease.id == lease_id)
            .ok_or_else(|| fail("Screen sharing expired or stopped. Select a source again."))?;
        if lease.last_frame.elapsed() < FRAME_INTERVAL {
            return store.snapshot(window.label(), &owner);
        }
        (
            lease.attachment_id.clone(),
            lease.source.clone(),
            store.epoch.clone(),
        )
    };
    let operation = CaptureOperation::begin(&state, window.label(), &owner, &epoch)?;
    let captured = capture_native(&operation, source.clone()).await;
    operation.finish(|store| {
        if store
            .lease
            .as_ref()
            .is_none_or(|lease| lease.id != lease_id)
        {
            return Err(fail("Screen sharing stopped before the frame was ready."));
        }
        let result = (|| {
            let bytes = captured?;
            if store
                .entries
                .get(&id)
                .is_some_and(|entry| entry.content == bytes)
            {
                return Ok(());
            }
            let image = entry(
                window.label(),
                &owner,
                source.name,
                "image/png",
                bytes,
                "Shared window snapshot",
                true,
            )?;
            store.entries.remove(&id);
            let new_id = image.attachment.id.clone();
            store.insert(window.label(), &owner, image)?;
            store.lease.as_mut().unwrap().attachment_id = new_id;
            Ok::<_, Problem>(())
        })();
        if let Err(error) = result {
            store.stop_screen();
            return Err(error);
        }
        store.lease.as_mut().unwrap().last_frame = Instant::now();
        store.snapshot(window.label(), &owner)
    })
}

#[tauri::command]
pub fn chat_capture_stop(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, ChatContent>,
    owner: ContentOwner,
    keep_snapshot: bool,
    expected_attachment_id: Option<String>,
) -> Result<ContentSnapshot, Problem> {
    let mut store = state.0.lock().unwrap();
    store.check(window.label(), &owner)?;
    store.expire();
    if keep_snapshot {
        if store
            .lease
            .as_ref()
            .is_some_and(|lease| Some(&lease.attachment_id) != expected_attachment_id.as_ref())
        {
            return Err(fail(
                "The shared preview changed. Review the latest frame and press Send again.",
            ));
        }
        // Explicit send freezes the reviewed frame. Stop without send revokes all screen grants.
        store.stop_capture();
        store.lease = None;
        store.sources.clear();
        store.epoch = opaque_id();
    } else {
        store.stop_screen();
    }
    store.snapshot(window.label(), &owner)
}

#[tauri::command]
pub async fn chat_content_send(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, ChatContent>,
    runtime: tauri::State<'_, std::sync::Arc<crate::copilot::CopilotRuntimeState>>,
    owner: ContentOwner,
    session_id: String,
    prompt: String,
    attachment_ids: Vec<String>,
) -> Result<crate::copilot::CopilotPromptResult, Problem> {
    let (entries, epoch) = {
        let mut store = state.0.lock().unwrap();
        store.check_session(window.label(), &owner, &session_id)?;
        let entries = store.selected(window.label(), &owner, &attachment_ids)?;
        if store.lease.is_some() {
            return Err(fail("Freeze the shared preview before sending."));
        }
        (entries, store.epoch.clone())
    };
    let capabilities = runtime.content_capabilities()?;
    let blocks = prompt_blocks(&prompt, &entries, capabilities.0, capabilities.1)?;
    let send_token = opaque_id();
    {
        let mut store = state.0.lock().unwrap();
        store.check(window.label(), &owner)?;
        if epoch != store.epoch || store.pending_send.is_some() {
            return Err(fail(
                "Another turn is pending or attachment access changed. Review the draft again.",
            ));
        }
        store.pending_send = Some((session_id.clone(), send_token.clone()));
    }
    struct Sending<'a>(&'a ChatContent, String);
    impl Drop for Sending<'_> {
        fn drop(&mut self) {
            let mut store = self.0 .0.lock().unwrap();
            if store
                .pending_send
                .as_ref()
                .is_some_and(|(_, token)| token == &self.1)
            {
                store.pending_send = None;
            }
        }
    }
    let _sending = Sending(&state, send_token);
    let confirm_window = window.clone();
    let question = format!("Send this reviewed turn to agent \"{}\", conversation \"{}\" (ACP session \"{}\")?\n\n{}\n\nAttachments:\n{}\n\nFiles and images leave this device through the connected agent. Cancel keeps your draft.",
        if owner.agent_id.is_empty() { "Copilot CLI" } else { &owner.agent_id }, owner.conversation_id,
        session_id,
        prompt.chars().take(1200).collect::<String>(),
        entries.iter().map(|entry| format!("{} — {} ({} bytes)", entry.attachment.name,
            entry.attachment.provenance, entry.content.len())).collect::<Vec<_>>().join("\n"));
    let approved = tauri::async_runtime::spawn_blocking(move || {
        confirm_window
            .dialog()
            .message(question)
            .parent(&confirm_window)
            .title("Darbot — send reviewed attachments")
            .buttons(MessageDialogButtons::OkCancelCustom(
                "Send this turn".into(),
                "Keep draft".into(),
            ))
            .blocking_show()
    })
    .await
    .map_err(|_| fail("Send approval did not finish."))?;
    if !approved {
        return Err(fail(
            "Send cancelled. Your draft and attachments were kept.",
        ));
    }
    {
        let mut store = state.0.lock().unwrap();
        store.selected(window.label(), &owner, &attachment_ids)?;
        store.check_session(window.label(), &owner, &session_id)?;
        if epoch != store.epoch {
            return Err(fail("The recipient changed. Your draft was kept."));
        }
    }
    let runtime = std::sync::Arc::clone(&runtime);
    let agent = owner.agent_id.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        runtime.prompt_content(agent, session_id, blocks)
    })
    .await
    .map_err(|_| {
        fail("The attachment turn did not finish. Inspect the conversation before retrying.")
    })??;
    let mut store = state.0.lock().unwrap();
    for id in attachment_ids {
        store.entries.remove(&id);
    }
    drop(store);
    Ok(result)
}

#[cfg(not(windows))]
mod platform {
    use super::*;
    pub fn sources() -> Result<Vec<CaptureSource>, Problem> {
        Err(fail(
            "Native window capture is available only on Windows. Upload a PNG instead.",
        ))
    }
    pub fn capture(
        _worker: &crate::native_capture::OwnedCapture,
        _source: &CaptureSource,
    ) -> Result<Vec<u8>, Problem> {
        Err(fail(
            "Native window capture is unavailable on this platform.",
        ))
    }
}

#[cfg(windows)]
pub(crate) fn render_worker(target: &crate::native_capture::Target) -> Result<Vec<u8>, Problem> {
    platform::render(target)
}

#[cfg(windows)]
mod platform {
    use super::*;
    use windows::Win32::{
        Foundation::{HWND, LPARAM, RECT},
        Graphics::Gdi::*,
        UI::WindowsAndMessaging::*,
    };

    unsafe extern "system" fn enumerate(hwnd: HWND, data: LPARAM) -> windows::core::BOOL {
        let sources = &mut *(data.0 as *mut Vec<CaptureSource>);
        if sources.len() >= 128 || !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return windows::core::BOOL(1);
        }
        let name = crate::native_capture::window_title(hwnd.0 as isize);
        if !name.is_empty() {
            let mut process = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut process));
            // A destroyed HWND loses this property, even when its numeric value is reused.
            match crate::native_capture::WindowIdentity::bind(
                hwnd.0 as isize,
                name.clone(),
                crate::native_capture::CaptureKind::SelectedWindow,
            ) {
                Ok(identity) => sources.push(CaptureSource {
                    id: opaque_id(),
                    name,
                    handle: hwnd.0 as isize,
                    process,
                    identity: Some(identity),
                }),
                Err(error) => eprintln!("Darbot capture source unavailable: {error:?}"),
            }
        }
        windows::core::BOOL(1)
    }

    pub fn sources() -> Result<Vec<CaptureSource>, Problem> {
        let mut sources = Vec::new();
        unsafe {
            EnumWindows(
                Some(enumerate),
                LPARAM(&mut sources as *mut Vec<CaptureSource> as isize),
            )
            .map_err(|_| fail("Windows could not list capture sources."))?;
        }
        Ok(sources)
    }

    pub fn capture(
        worker: &crate::native_capture::OwnedCapture,
        source: &CaptureSource,
    ) -> Result<Vec<u8>, Problem> {
        let identity = source
            .identity
            .as_ref()
            .ok_or_else(|| fail("Refresh and choose a bound capture source."))?;
        if identity.0.handle != source.handle || identity.0.process != source.process {
            return Err(fail("Capture source identity changed."));
        }
        let bytes = crate::native_capture::capture(
            worker,
            &identity.0,
            crate::native_capture::CAPTURE_TIMEOUT,
        )?;
        validate_png(&bytes)?;
        Ok(bytes)
    }

    pub fn render(target: &crate::native_capture::Target) -> Result<Vec<u8>, Problem> {
        let hwnd = target.check()?;
        unsafe {
            let mut rect = RECT::default();
            GetWindowRect(hwnd, &mut rect).map_err(|_| fail("Capture bounds are unavailable."))?;
            let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
            let app_only = target.kind == crate::native_capture::CaptureKind::AppOnly;
            let dimension_limit = if app_only { 2048 } else { 4096 };
            if width <= 0
                || height <= 0
                || width > dimension_limit
                || height > dimension_limit
                || i64::from(width) * i64::from(height) > 4_194_304
            {
                return Err(fail(if app_only {
                    "App-only capture is limited to 2048 by 2048 pixels."
                } else {
                    "Resize the selected window to at most 4 megapixels before capturing."
                }));
            }
            let dc = GetDC(Some(hwnd));
            if dc.is_invalid() {
                return Err(fail("The selected window cannot be captured."));
            }
            let memory = CreateCompatibleDC(Some(dc));
            let bitmap = CreateCompatibleBitmap(dc, width, height);
            if memory.is_invalid() || bitmap.is_invalid() {
                if !bitmap.is_invalid() {
                    let _ = DeleteObject(bitmap.into());
                }
                if !memory.is_invalid() {
                    let _ = DeleteDC(memory);
                }
                ReleaseDC(Some(hwnd), dc);
                return Err(fail("Windows could not allocate capture resources."));
            }
            let previous = SelectObject(memory, bitmap.into());
            let cleared = PatBlt(memory, 0, 0, width, height, BLACKNESS).as_bool();
            // Win32 does not make this check atomic with PrintWindow; reject changed-instance
            // pixels again before encoding or returning any image from the disposable worker.
            let still_selected = target.check().is_ok();
            let rendered = cleared
                && still_selected
                && windows::Win32::Storage::Xps::PrintWindow(
                    hwnd,
                    memory,
                    windows::Win32::Storage::Xps::PRINT_WINDOW_FLAGS(if app_only { 2 } else { 0 }),
                )
                .as_bool();
            SelectObject(memory, previous);
            let mut info = BITMAPINFO::default();
            info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = width;
            info.bmiHeader.biHeight = -height;
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            info.bmiHeader.biCompression = BI_RGB.0;
            let mut pixels = vec![0u8; width as usize * height as usize * 4];
            let rows = if rendered {
                GetDIBits(
                    dc,
                    bitmap,
                    0,
                    height as u32,
                    Some(pixels.as_mut_ptr().cast()),
                    &mut info,
                    DIB_RGB_COLORS,
                )
            } else {
                0
            };
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(memory);
            ReleaseDC(Some(hwnd), dc);
            target.check()?;
            let blank = if app_only {
                pixels.chunks_exact(4).all(|p| p[..3] == pixels[..3])
            } else {
                pixels.chunks_exact(4).all(|p| p[..3] == [0, 0, 0])
            };
            if rows != height || blank {
                return Err(fail("This window does not support rendered capture. Choose another window; no desktop fallback was used."));
            }
            for pixel in pixels.chunks_exact_mut(4) {
                pixel.swap(0, 2);
                pixel[3] = 255;
            }
            let mut bytes = Vec::new();
            {
                let mut encoder = png::Encoder::new(&mut bytes, width as u32, height as u32);
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                let mut writer = encoder
                    .write_header()
                    .map_err(|_| fail("Capture PNG encoding failed."))?;
                writer
                    .write_image_data(&pixels)
                    .map_err(|_| fail("Capture PNG encoding failed."))?;
            }
            validate_png(&bytes)?;
            Ok(bytes)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) fn revoke_owned_capture_fixture(worker: Arc<crate::native_capture::OwnedCapture>) {
        let state = ChatContent::default();
        {
            let mut store = state.0.lock().unwrap();
            store.active = Some(("main".into(), owner("fixture")));
            store.capture = Some(worker);
        }
        state.revoke_window_content("main");
        assert!(state.0.lock().unwrap().capture.is_none());
    }

    fn owner(id: &str) -> ContentOwner {
        ContentOwner {
            conversation_id: id.into(),
            agent_id: "agent".into(),
        }
    }
    fn png_fixture() -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 2, 1);
            encoder.set_color(png::ColorType::Rgb);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[255, 0, 0, 0, 255, 0]).unwrap();
        }
        bytes
    }
    pub(crate) fn fixture_prompt() -> Vec<Value> {
        let owner = owner("one");
        let image = entry(
            "main",
            &owner,
            "fixture.png".into(),
            "image/png",
            png_fixture(),
            "test",
            true,
        )
        .unwrap();
        let text = entry(
            "main",
            &owner,
            "fixture.txt".into(),
            "text/plain",
            b"owned fixture context".to_vec(),
            "test",
            false,
        )
        .unwrap();
        prompt_blocks("Review owned fixture", &[image, text], true, true).unwrap()
    }
    #[test]
    fn validates_codec_mime_and_bounded_text_without_spilling_content() {
        let owner = owner("one");
        assert!(entry(
            "main",
            &owner,
            "fixture.png".into(),
            "image/png",
            png_fixture(),
            "test",
            false
        )
        .is_ok());
        for (mime, bytes) in [
            ("image/png", b"private content".to_vec()),
            ("text/plain", vec![0, 255]),
            ("application/json", b"{bad private content}".to_vec()),
            ("image/svg+xml", b"<svg/>".to_vec()),
            ("text/plain", vec![b'a'; FILE_LIMIT + 1]),
        ] {
            let error = entry("main", &owner, "fixture".into(), mime, bytes, "test", false)
                .err()
                .unwrap();
            assert!(!serde_json::to_string(&error)
                .unwrap()
                .contains("private content"));
        }
        let mut corrupt = png_fixture();
        corrupt.extend_from_slice(b"not image content");
        assert!(validate_png(&corrupt).is_err());
        corrupt.extend_from_slice(b"\x00\x00\x00\x00IEND\xae\x42\x60\x82");
        assert!(validate_png(&corrupt).is_err());
        let mut corrupt = png_fixture();
        corrupt.truncate(corrupt.len() - 8);
        assert!(validate_png(&corrupt).is_err());
    }
    #[test]
    fn opaque_ownership_removal_and_remote_guard() {
        let state = ChatContent::default();
        let owner = owner("one");
        let mut store = state.0.lock().unwrap();
        store.active = Some(("main".into(), owner.clone()));
        let image = entry(
            "main",
            &owner,
            "fixture".into(),
            "image/png",
            png_fixture(),
            "test",
            true,
        )
        .unwrap();
        let id = image.attachment.id.clone();
        assert_eq!(id.len(), 32);
        store.insert("main", &owner, image).unwrap();
        assert!(store
            .selected("other", &owner, std::slice::from_ref(&id))
            .is_err());
        assert!(store
            .selected(
                "main",
                &super::tests::owner("two"),
                std::slice::from_ref(&id)
            )
            .is_err());
        assert!(store
            .selected("main", &owner, &[id.clone(), id.clone()])
            .is_err());
        drop(store);
        assert!(state.remote_send_allowed().is_err());
        state.revoke_window_content("main");
        assert!(state.remote_send_allowed().is_ok());
        assert!(state.0.lock().unwrap().entries.is_empty());
    }
    #[test]
    fn tray_hide_revokes_content_but_preserves_live_session_binding() {
        let state = ChatContent::default();
        let owner = owner("one");
        state.bind_session("main", owner.clone(), "session-one");
        let old_id = {
            let mut store = state.0.lock().unwrap();
            store.active = Some(("main".into(), owner.clone()));
            let image = entry(
                "main",
                &owner,
                "old.png".into(),
                "image/png",
                png_fixture(),
                "test",
                true,
            )
            .unwrap();
            let id = image.attachment.id.clone();
            store.insert("main", &owner, image).unwrap();
            id
        };
        state.revoke_window_content("main");
        {
            let mut store = state.0.lock().unwrap();
            assert!(store.active.is_none());
            assert!(store.entries.is_empty());
            store.active = Some(("main".into(), owner.clone()));
            assert!(store.selected("main", &owner, &[old_id]).is_err());
            let image = entry(
                "main",
                &owner,
                "new.png".into(),
                "image/png",
                png_fixture(),
                "test",
                true,
            )
            .unwrap();
            let id = image.attachment.id.clone();
            store.insert("main", &owner, image).unwrap();
            store.check_session("main", &owner, "session-one").unwrap();
            assert!(store.check_session("main", &owner, "session-two").is_err());
            assert!(store
                .check_session("main", &super::tests::owner("two"), "session-one")
                .is_err());
            let selected = store.selected("main", &owner, &[id]).unwrap();
            assert_eq!(
                prompt_blocks("Review", &selected, true, true).unwrap()[1]["type"],
                "image"
            );
        }
        state.retire_session("session-one");
        assert!(state
            .0
            .lock()
            .unwrap()
            .check_session("main", &owner, "session-one")
            .is_err());
    }
    #[test]
    fn acp_wire_blocks_and_capabilities_are_not_text_substitutions() {
        let image = entry(
            "main",
            &owner("one"),
            "fixture".into(),
            "image/png",
            png_fixture(),
            "test",
            true,
        )
        .unwrap();
        assert!(prompt_blocks("", std::slice::from_ref(&image), false, true).is_err());
        let blocks = prompt_blocks("", &[image], true, false).unwrap();
        assert_eq!(blocks[0]["type"], "image");
        assert_eq!(blocks[0]["mimeType"], "image/png");
        let context = entry(
            "main",
            &owner("one"),
            "fixture.txt".into(),
            "text/plain",
            b"owned fixture".to_vec(),
            "test",
            false,
        )
        .unwrap();
        assert!(prompt_blocks("", std::slice::from_ref(&context), true, false).is_err());
        let blocks = prompt_blocks("question", &[context], false, true).unwrap();
        assert_eq!(blocks[1]["type"], "resource");
        assert_eq!(blocks[1]["resource"]["text"], "owned fixture");
        assert!(blocks[1]["resource"]["uri"]
            .as_str()
            .unwrap()
            .starts_with("darbot-attachment:///"));
        assert!(prompt_blocks("", &[], true, true).is_err());
    }
    #[test]
    fn navigation_revokes_screen_but_keeps_uploaded_drafts() {
        let mut store = Store {
            active: Some(("main".into(), owner("one"))),
            ..Store::default()
        };
        for screen in [true, false] {
            store
                .insert(
                    "main",
                    &owner("one"),
                    entry(
                        "main",
                        &owner("one"),
                        "fixture".into(),
                        "text/plain",
                        b"owned".to_vec(),
                        "test",
                        screen,
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        store.stop_screen();
        assert_eq!(store.entries.len(), 1);
        store.active = Some(("main".into(), owner("two")));
        assert!(store
            .snapshot("main", &owner("two"))
            .unwrap()
            .attachments
            .is_empty());
        store.active = Some(("main".into(), owner("one")));
        assert_eq!(
            store
                .snapshot("main", &owner("one"))
                .unwrap()
                .attachments
                .len(),
            1
        );
    }

    #[test]
    fn encoded_turn_budget_includes_json_escaping_and_drafts_remain_available() {
        let owner = owner("one");
        let context = entry(
            "main",
            &owner,
            "fixture.txt".into(),
            "text/plain",
            vec![b'\t'; FILE_LIMIT],
            "test",
            false,
        )
        .unwrap();
        assert!(prompt_blocks("question", std::slice::from_ref(&context), true, true).is_err());
        assert_eq!(context.content.len(), FILE_LIMIT);
    }

    #[test]
    fn expired_capture_lease_revokes_its_frame() {
        let owner = owner("one");
        let mut store = Store {
            active: Some(("main".into(), owner.clone())),
            ..Store::default()
        };
        let image = entry(
            "main",
            &owner,
            "fixture".into(),
            "image/png",
            png_fixture(),
            "test",
            true,
        )
        .unwrap();
        let id = image.attachment.id.clone();
        store.insert("main", &owner, image).unwrap();
        store.lease = Some(CaptureLease {
            id: opaque_id(),
            source: CaptureSource {
                id: opaque_id(),
                name: "fixture".into(),
                #[cfg(windows)]
                handle: 0,
                #[cfg(windows)]
                process: 0,
                #[cfg(windows)]
                identity: None,
            },
            last_frame: Instant::now() - LEASE_TIMEOUT - Duration::from_secs(1),
            attachment_id: id.clone(),
        });
        assert!(store.selected("main", &owner, &[id]).is_err());
        assert!(store.lease.is_none());
        assert!(store.entries.is_empty());
    }

    #[test]
    fn cancelling_exact_pending_turn_revokes_capture_but_keeps_file_drafts() {
        let state = ChatContent::default();
        let owner = owner("one");
        let epoch = {
            let mut store = state.0.lock().unwrap();
            store.active = Some(("main".into(), owner.clone()));
            store.pending_send = Some(("session-one".into(), opaque_id()));
            for screen in [true, false] {
                store
                    .insert(
                        "main",
                        &owner,
                        entry(
                            "main",
                            &owner,
                            "fixture".into(),
                            "text/plain",
                            b"owned fixture".to_vec(),
                            "test",
                            screen,
                        )
                        .unwrap(),
                    )
                    .unwrap();
            }
            store.epoch.clone()
        };
        assert!(!state.cancel_session("other-session"));
        assert!(state.cancel_session("session-one"));
        let store = state.0.lock().unwrap();
        assert_ne!(store.epoch, epoch);
        assert!(store.pending_send.is_none());
        assert_eq!(store.entries.len(), 1);
        assert!(store.entries.values().all(|entry| !entry.screen));
    }

    #[test]
    fn cancelling_a_bound_capture_without_a_pending_send_is_session_specific() {
        let state = ChatContent::default();
        let owner = owner("capture");
        state.bind_session("main", owner.clone(), "session-capture");
        {
            let mut store = state.0.lock().unwrap();
            store.active = Some(("main".into(), owner));
            store.capture = Some(Arc::new(crate::native_capture::OwnedCapture::default()));
        }
        assert!(!state.cancel_session("another-session"));
        assert!(state.0.lock().unwrap().capture.is_some());
        assert!(state.cancel_session("session-capture"));
        assert!(state.0.lock().unwrap().capture.is_none());
    }

    #[test]
    fn capture_requires_the_approved_conversation_and_epoch_before_registration() {
        let state = ChatContent::default();
        let approved = owner("approved");
        {
            let mut store = state.0.lock().unwrap();
            store.active = Some(("main".into(), approved.clone()));
            store.epoch = "approved-epoch".into();
        }
        assert!(
            CaptureOperation::begin(&state, "main", &owner("other"), "approved-epoch").is_err()
        );
        assert!(
            CaptureOperation::begin(&state, "another-window", &approved, "approved-epoch").is_err()
        );
        assert!(CaptureOperation::begin(&state, "main", &approved, "revoked-epoch").is_err());
        assert!(state.0.lock().unwrap().capture.is_none());
    }

    fn capture_source_fixture() -> CaptureSource {
        CaptureSource {
            id: opaque_id(),
            name: "not a window".into(),
            #[cfg(windows)]
            handle: 0,
            #[cfg(windows)]
            process: 0,
            #[cfg(windows)]
            identity: None,
        }
    }

    fn capture_state_fixture() -> (ChatContent, ContentOwner) {
        let state = ChatContent::default();
        let owner = owner("capture");
        state.bind_session("main", owner.clone(), "session-capture");
        {
            let mut store = state.0.lock().unwrap();
            store.active = Some(("main".into(), owner.clone()));
            store.epoch = opaque_id();
        }
        (state, owner)
    }

    #[test]
    fn session_cancellation_invalidates_approval_running_and_completed_capture_operations() {
        for boundary in ["pending-approval", "worker-running", "before-commit"] {
            for share in [false, true] {
                let (state, owner) = capture_state_fixture();
                let epoch = state.0.lock().unwrap().epoch.clone();
                let operation = CaptureOperation::begin(&state, "main", &owner, &epoch).unwrap();
                let worker = operation.worker.0.clone();
                let running = if boundary == "worker-running" {
                    let running = worker.clone();
                    let (tx, rx) = std::sync::mpsc::sync_channel(1);
                    let thread = std::thread::spawn(move || {
                        crate::native_capture::tests::run_fixture(&running, "hang", || {
                            let _ = tx.try_send(());
                        })
                    });
                    rx.recv_timeout(crate::native_capture::CAPTURE_TIMEOUT)
                        .unwrap();
                    Some(thread)
                } else {
                    if boundary == "before-commit" {
                        crate::native_capture::tests::run_fixture(&worker, "finish", || {})
                            .unwrap();
                        assert!(crate::native_capture::tests::is_reaped(&worker));
                    }
                    None
                };
                assert!(!state.cancel_session("another-session"));
                operation.check(&state.0.lock().unwrap()).unwrap();
                assert!(state.cancel_session("session-capture"), "{boundary}");
                if let Some(thread) = running {
                    assert!(thread.join().unwrap().is_err());
                }
                assert!(crate::native_capture::tests::is_reaped(&worker));
                assert!(
                    operation
                        .finish(|store| commit_capture(
                            store,
                            "main",
                            &owner,
                            capture_source_fixture(),
                            share,
                            Ok(png_fixture()),
                        ))
                        .is_err(),
                    "{boundary}"
                );
                let store = state.0.lock().unwrap();
                assert_ne!(store.epoch, epoch);
                assert!(store.capture.is_none());
                assert!(store.lease.is_none());
                assert!(store.entries.is_empty());
            }
        }
    }

    #[test]
    fn commit_unregisters_atomically_and_later_cancellation_revokes_only_shared_capture() {
        for share in [false, true] {
            let (state, owner) = capture_state_fixture();
            let epoch = state.0.lock().unwrap().epoch.clone();
            let operation = CaptureOperation::begin(&state, "main", &owner, &epoch).unwrap();
            let snapshot = operation
                .finish(|store| {
                    commit_capture(
                        store,
                        "main",
                        &owner,
                        capture_source_fixture(),
                        share,
                        Ok(png_fixture()),
                    )
                })
                .unwrap();
            assert_eq!(snapshot.attachments.len(), 1);
            assert_eq!(snapshot.lease_id.is_some(), share);
            assert!(state.0.lock().unwrap().capture.is_none());
            assert_eq!(state.cancel_session("session-capture"), share);
            let store = state.0.lock().unwrap();
            assert!(store.lease.is_none());
            assert_eq!(store.entries.is_empty(), share);
        }
    }

    #[test]
    fn old_operation_discard_and_drop_cannot_clear_new_capture_registration() {
        let (state, owner) = capture_state_fixture();
        let epoch = state.0.lock().unwrap().epoch.clone();
        let old = CaptureOperation::begin(&state, "main", &owner, &epoch).unwrap();
        assert!(state.cancel_session("session-capture"));
        let epoch = state.0.lock().unwrap().epoch.clone();
        let new = CaptureOperation::begin(&state, "main", &owner, &epoch).unwrap();
        assert!(old
            .finish::<()>(|_| panic!("Revoked operation must not commit"))
            .is_err());
        drop(old);
        new.check(&state.0.lock().unwrap()).unwrap();
        new.finish(|store| {
            commit_capture(
                store,
                "main",
                &owner,
                capture_source_fixture(),
                true,
                Ok(png_fixture()),
            )
        })
        .unwrap();
        assert!(state.0.lock().unwrap().capture.is_none());
    }

    #[tokio::test]
    async fn cancellation_during_approval_prevents_a_worker_launch_after_approval() {
        let (state, owner) = capture_state_fixture();
        let epoch = state.0.lock().unwrap().epoch.clone();
        let operation = CaptureOperation::begin(&state, "main", &owner, &epoch).unwrap();
        assert!(state.cancel_session("session-capture"));
        assert!(capture_native(&operation, capture_source_fixture())
            .await
            .is_err());
        assert!(crate::native_capture::tests::is_reaped(&operation.worker.0));
    }

    #[test]
    fn count_bounds_and_source_handles_are_not_client_authority() {
        let owner = owner("one");
        let mut store = Store {
            active: Some(("main".into(), owner.clone())),
            ..Store::default()
        };
        for _ in 0..ITEM_LIMIT {
            store
                .insert(
                    "main",
                    &owner,
                    entry(
                        "main",
                        &owner,
                        "fixture.txt".into(),
                        "text/plain",
                        b"fixture".to_vec(),
                        "test",
                        false,
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        assert!(store
            .insert(
                "main",
                &owner,
                entry(
                    "main",
                    &owner,
                    "extra.txt".into(),
                    "text/plain",
                    b"fixture".to_vec(),
                    "test",
                    false
                )
                .unwrap()
            )
            .is_err());
        assert_eq!(store.entries.len(), ITEM_LIMIT);
        let source = CaptureSource {
            id: opaque_id(),
            name: "fixture".into(),
            #[cfg(windows)]
            handle: 12345,
            #[cfg(windows)]
            process: 55555,
            #[cfg(windows)]
            identity: None,
        };
        let value = serde_json::to_value(source).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 2);
        assert!(value.get("handle").is_none());
        assert!(value.get("process").is_none());
    }

    #[test]
    fn native_session_binding_rejects_same_agent_cross_conversation_and_cross_window_delivery() {
        let state = ChatContent::default();
        state.bind_session("main", owner("one"), "session-one");
        {
            let store = state.0.lock().unwrap();
            assert!(store
                .check_session("main", &owner("one"), "session-one")
                .is_ok());
            assert!(store
                .check_session("main", &owner("two"), "session-one")
                .is_err());
            assert!(store
                .check_session("main", &owner("one"), "session-two")
                .is_err());
            assert!(store
                .check_session("other-window", &owner("one"), "session-one")
                .is_err());
        }
        state.retire_session("unrelated-session");
        assert!(state
            .0
            .lock()
            .unwrap()
            .check_session("main", &owner("one"), "session-one")
            .is_ok());
        state.retire_session("session-one");
        assert!(state
            .0
            .lock()
            .unwrap()
            .check_session("main", &owner("one"), "session-one")
            .is_err());
        assert!(owner_for_session(Some(String::new()), None).is_err());
        assert!(owner_for_session(None, None).unwrap().is_none());
    }

    #[cfg(windows)]
    #[test]
    pub(crate) fn native_capture_encodes_only_an_owned_fixture_window_and_rejects_closed_sources() {
        use windows::{
            core::w,
            Win32::{
                Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM},
                Graphics::Gdi::*,
                UI::WindowsAndMessaging::*,
            },
        };
        unsafe extern "system" fn paint(
            hwnd: HWND,
            message: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            if message == WM_PRINT || message == WM_PRINTCLIENT {
                if std::env::var("DARBOT_OWNED_CAPTURE_FIXTURE").as_deref() == Ok("hung-render") {
                    loop {
                        std::thread::sleep(Duration::from_secs(60));
                    }
                }
                let mut rect = RECT::default();
                GetClientRect(hwnd, &mut rect).unwrap();
                let brush = CreateSolidBrush(COLORREF(0x00ff00));
                FillRect(HDC(wparam.0 as *mut std::ffi::c_void), &rect, brush);
                let _ = DeleteObject(brush.into());
                return LRESULT(1);
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        // Only this test's WM_PRINT renderer is sampled, never the desktop or another process.
        let class = WNDCLASSW {
            lpfnWndProc: Some(paint),
            lpszClassName: w!("DarbotCaptureFixture"),
            hbrBackground: unsafe { HBRUSH(GetStockObject(WHITE_BRUSH).0) },
            ..Default::default()
        };
        assert_ne!(unsafe { RegisterClassW(&class) }, 0);
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW,
                w!("DarbotCaptureFixture"),
                w!("Darbot owned capture fixture"),
                WS_POPUP | WS_VISIBLE,
                0,
                0,
                160,
                96,
                None,
                None,
                None,
                None,
            )
            .unwrap()
        };
        struct Owned(HWND);
        impl Drop for Owned {
            fn drop(&mut self) {
                unsafe {
                    let _ = DestroyWindow(self.0);
                    let _ = UnregisterClassW(w!("DarbotCaptureFixture"), None);
                }
            }
        }
        let owned = Owned(hwnd);
        unsafe {
            let _ = UpdateWindow(hwnd);
        }
        let source = CaptureSource {
            id: opaque_id(),
            name: "Darbot owned capture fixture".into(),
            handle: hwnd.0 as isize,
            process: std::process::id(),
            identity: Some(
                crate::native_capture::WindowIdentity::bind(
                    hwnd.0 as isize,
                    "Darbot owned capture fixture".into(),
                    crate::native_capture::CaptureKind::SelectedWindow,
                )
                .unwrap(),
            ),
        };
        let target = &source.identity.as_ref().unwrap().0;
        if std::env::var("DARBOT_OWNED_CAPTURE_FIXTURE").as_deref() == Ok("hung-render") {
            // A compositor can satisfy PrintWindow without WM_PRINT. Drive the owned hung
            // renderer explicitly so worker-lifetime tests do not depend on that OS choice.
            unsafe {
                SendMessageW(hwnd, WM_PRINT, Some(WPARAM(0)), Some(LPARAM(0)));
            }
            panic!("The owned WM_PRINT renderer must not return");
        }
        let bytes = render_worker(target).unwrap();
        validate_png(&bytes).unwrap();
        let reader = png::Decoder::new(std::io::Cursor::new(bytes))
            .read_info()
            .unwrap();
        assert_eq!((reader.info().width, reader.info().height), (160, 96));
        let mut impostor = target.clone();
        impostor.process = 0;
        assert!(render_worker(&impostor).is_err());
        let mut reused = target.clone();
        reused.property = "DarbotCapture-different-instance".into();
        assert!(render_worker(&reused).is_err());
        let mut changed_process = target.clone();
        changed_process.created = changed_process.created.wrapping_add(1);
        assert!(render_worker(&changed_process).is_err());
        let property: Vec<u16> = target.property.encode_utf16().chain(Some(0)).collect();
        unsafe {
            let _ = RemovePropW(hwnd, windows::core::PCWSTR(property.as_ptr()));
        }
        // Same HWND, PID, thread and title are insufficient without the original instance token.
        let replacement = crate::native_capture::WindowIdentity::bind(
            hwnd.0 as isize,
            source.name.clone(),
            crate::native_capture::CaptureKind::SelectedWindow,
        )
        .unwrap();
        assert!(render_worker(target).is_err());
        assert!(render_worker(&replacement.0).is_ok());
        drop(owned);
        assert!(render_worker(target).is_err());
        assert!(render_worker(&replacement.0).is_err());
    }
}
