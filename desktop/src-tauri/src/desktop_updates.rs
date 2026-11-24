//! Opt-in release notices. GitHub authenticates build evidence; fleet messages are only hints.
//! This module never installs or executes downloaded content.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    io::{Cursor, Read},
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::{Emitter, Manager};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

pub const BUILD_REPOSITORY: &str = "DarbotLM/darbot";
pub const SOURCE_REPOSITORY: &str = "DarbotLM/darbot";
pub const WORKFLOW: &str = "desktop-build.yml";
const PERIOD: u64 = 15 * 60;
const JSON_LIMIT: usize = 1024 * 1024;
const ARCHIVE_LIMIT: usize = 256 * 1024 * 1024;
const MANIFEST_LIMIT: u64 = 16 * 1024;
const HISTORY_LIMIT: usize = 128;
type Result<T> = std::result::Result<T, UpdateError>;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateHint {
    pub version: String,
    pub build_repository: String,
    pub source_repository: String,
    pub workflow: String,
    pub run_id: u64,
    pub artifact_id: u64,
    pub build_sha: String,
    pub source_sha: String,
}
impl UpdateHint {
    pub fn validate(&self) -> Result<()> {
        if self.build_repository != BUILD_REPOSITORY
            || self.source_repository != SOURCE_REPOSITORY
            || self.workflow != WORKFLOW
            || self.run_id == 0
            || self.artifact_id == 0
            || !hex(&self.build_sha, 40)
            || !hex(&self.source_sha, 40)
            || version(&self.version).is_none()
        {
            return Err(error("invalid_notice", "Unrecognized release identity"));
        }
        Ok(())
    }
    fn key(&self) -> String {
        format!("{}:{}:{}", self.version, self.run_id, self.artifact_id)
    }
    fn url(&self) -> String {
        format!(
            "https://github.com/{BUILD_REPOSITORY}/actions/runs/{}",
            self.run_id
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UpdateError {
    pub code: String,
    pub message: String,
}
fn error(code: &str, message: &str) -> UpdateError {
    UpdateError {
        code: code.into(),
        message: message.into(),
    }
}
fn hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn version(value: &str) -> Option<[u64; 3]> {
    if value.len() > 48 {
        return None;
    }
    let parts: Vec<_> = value.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let mut result = [0; 3];
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty()
            || (part.len() > 1 && part.starts_with('0'))
            || !part.bytes().all(|c| c.is_ascii_digit())
        {
            return None;
        }
        result[index] = part.parse().ok()?;
    }
    Some(result)
}
fn newer(candidate: &str, current: &str) -> bool {
    match (version(candidate), version(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Preferences {
    enabled: bool,
    seen: Vec<String>,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            enabled: false,
            seen: vec![],
        }
    }
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    pub enabled: bool,
    pub current_version: String,
    pub build_repository: String,
    pub workflow: String,
    pub phase: String,
    pub available: Option<UpdateHint>,
    pub signed: Option<bool>,
    pub notify: bool,
    pub last_checked: Option<u64>,
    pub next_check: Option<u64>,
    pub error: Option<UpdateError>,
}
struct Inner {
    preferences: Preferences,
    path: Option<PathBuf>,
    status: UpdateStatus,
    hint: Option<UpdateHint>,
    failures: u32,
    generation: u64,
    check_cancel: CancellationToken,
}
pub struct UpdateState {
    inner: Mutex<Inner>,
    check: tokio::sync::Mutex<()>,
    stop: CancellationToken,
    wake: tokio::sync::Notify,
}
impl Default for UpdateState {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner {
                preferences: Preferences::default(),
                path: None,
                hint: None,
                failures: 0,
                generation: 0,
                check_cancel: CancellationToken::new(),
                status: UpdateStatus {
                    enabled: false,
                    current_version: env!("CARGO_PKG_VERSION").into(),
                    build_repository: BUILD_REPOSITORY.into(),
                    workflow: WORKFLOW.into(),
                    phase: "disabled".into(),
                    available: None,
                    signed: None,
                    notify: false,
                    last_checked: None,
                    next_check: None,
                    error: None,
                },
            }),
            check: tokio::sync::Mutex::new(()),
            stop: CancellationToken::new(),
            wake: tokio::sync::Notify::new(),
        }
    }
}
fn save(inner: &Inner) -> Result<()> {
    let path = inner
        .path
        .as_ref()
        .ok_or_else(|| error("storage", "Update preferences are unavailable"))?;
    let bytes = serde_json::to_vec(&inner.preferences)
        .map_err(|_| error("storage", "Cannot encode update preferences"))?;
    let staging = path.with_file_name(format!(
        "desktop-update-notices-{:032x}.tmp",
        rand::random::<u128>()
    ));
    use std::io::Write;
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&staging, path)
    })();
    if result.is_err() {
        if let Err(e) = std::fs::remove_file(&staging) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!("Darbot could not remove an incomplete update-preference write.");
            }
        }
    }
    result.map_err(|_| error("storage", "Cannot save update preferences"))
}
impl UpdateState {
    pub fn status(&self) -> UpdateStatus {
        self.inner.lock().unwrap().status.clone()
    }
    fn emit(&self, app: &tauri::AppHandle) {
        if app.emit("desktop:update-state", self.status()).is_err() {
            eprintln!("Darbot could not deliver an update-status notification.");
        }
    }
    pub fn shutdown(&self) {
        self.stop.cancel();
        self.inner.lock().unwrap().check_cancel.cancel();
    }
    pub fn initialize(self: &Arc<Self>, app: tauri::AppHandle) {
        {
            let mut inner = self.inner.lock().unwrap();
            let loaded = (|| -> Result<Preferences> {
                let root = app
                    .path()
                    .app_local_data_dir()
                    .map_err(|_| error("storage", "Update preferences directory unavailable"))?;
                std::fs::create_dir_all(&root)
                    .map_err(|_| error("storage", "Update preferences directory unavailable"))?;
                let path = root.join("desktop-update-notices.json");
                inner.path = Some(path.clone());
                if !path.exists() {
                    return Ok(Preferences::default());
                }
                if std::fs::metadata(&path)
                    .map_err(|_| error("storage", "Cannot read update preferences"))?
                    .len()
                    > 32 * 1024
                {
                    return Err(error("storage", "Update preferences exceed the size limit"));
                }
                let preferences: Preferences = serde_json::from_slice(
                    &std::fs::read(path)
                        .map_err(|_| error("storage", "Cannot read update preferences"))?,
                )
                .map_err(|_| {
                    error(
                        "storage",
                        "Update preferences are invalid; polling remains disabled",
                    )
                })?;
                if preferences.seen.len() > HISTORY_LIMIT
                    || preferences.seen.iter().any(|k| k.len() > 128)
                {
                    return Err(error("storage", "Update notification history is invalid"));
                }
                Ok(preferences)
            })();
            match loaded {
                Ok(preferences) => {
                    inner.status.enabled = preferences.enabled;
                    inner.status.phase = if preferences.enabled {
                        "idle"
                    } else {
                        "disabled"
                    }
                    .into();
                    inner.preferences = preferences;
                }
                Err(e) => {
                    inner.status.phase = "error".into();
                    inner.status.error = Some(e);
                }
            }
        }
        let state = self.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                if state.stop.is_cancelled() {
                    break;
                }
                let status = state.status();
                if status.enabled && status.next_check.is_none_or(|at| at <= now()) {
                    state.run_check(&app, false).await;
                }
                tokio::select! {
                    _ = state.stop.cancelled() => break,
                    _ = state.wake.notified() => {},
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {},
                }
            }
        });
    }
    fn set_enabled(&self, enabled: bool) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let old = inner.preferences.enabled;
        inner.preferences.enabled = enabled;
        if let Err(e) = save(&inner) {
            inner.preferences.enabled = old;
            return Err(e);
        }
        inner.check_cancel.cancel();
        inner.check_cancel = self.stop.child_token();
        inner.generation += 1;
        inner.status.enabled = enabled;
        inner.status.phase = if enabled { "idle" } else { "disabled" }.into();
        inner.status.available = None;
        inner.status.signed = None;
        inner.status.notify = false;
        inner.status.error = None;
        inner.status.next_check = None;
        self.wake.notify_one();
        Ok(())
    }
    fn dismiss(&self) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(hint) = inner.status.available.clone() {
            let old = inner.preferences.clone();
            let key = hint.key();
            if !inner.preferences.seen.contains(&key) {
                inner.preferences.seen.push(key);
            }
            if inner.preferences.seen.len() > HISTORY_LIMIT {
                inner.preferences.seen.remove(0);
            }
            if let Err(e) = save(&inner) {
                inner.preferences = old;
                return Err(e);
            }
        }
        inner.status.notify = false;
        Ok(())
    }
    /// Called only from the pinned fleet transport; no URLs, prompts or credentials are accepted.
    pub fn receive_hint(&self, hint: UpdateHint) {
        if hint.validate().is_err() || !newer(&hint.version, env!("CARGO_PKG_VERSION")) {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        if inner.status.available.as_ref() == Some(&hint) || inner.hint.as_ref() == Some(&hint) {
            return;
        }
        inner.hint = Some(hint);
        if inner.status.available.is_none() && inner.status.phase != "checking" {
            inner.status.phase = "verification_pending".into();
        }
        // Hints never enable polling or bypass the local schedule/backoff.
    }
    async fn run_check(&self, app: &tauri::AppHandle, explicit: bool) {
        let Ok(_guard) = self.check.try_lock() else {
            return;
        };
        let (cancel, generation, hint) = {
            let mut inner = self.inner.lock().unwrap();
            if self.stop.is_cancelled() || (!explicit && !inner.preferences.enabled) {
                return;
            }
            inner.status.phase = "checking".into();
            inner.status.error = None;
            inner.status.available = None;
            inner.status.signed = None;
            inner.status.notify = false;
            (
                inner.check_cancel.clone(),
                inner.generation,
                inner.hint.clone(),
            )
        };
        self.emit(app);
        let result = tokio::select! {
            _ = cancel.cancelled() => Err(error("cancelled", "Update check cancelled")),
            result = tokio::time::timeout(Duration::from_secs(240), discover(&Gh, &cancel, hint.as_ref())) =>
                result.unwrap_or_else(|_| Err(error("timeout", "Update verification timed out"))),
        };
        let advertised = {
            let mut inner = self.inner.lock().unwrap();
            if generation != inner.generation || self.stop.is_cancelled() {
                return;
            }
            inner.status.last_checked = Some(now());
            match result {
                Ok(found) => {
                    inner.failures = 0;
                    inner.status.phase = if found.is_some() {
                        "available"
                    } else {
                        "no_newer_build"
                    }
                    .into();
                    if let Some((notice, signed)) = found {
                        inner.status.notify =
                            explicit || !inner.preferences.seen.contains(&notice.key());
                        inner.status.available = Some(notice);
                        inner.status.signed = Some(signed);
                    }
                }
                Err(e) => {
                    inner.failures = inner.failures.saturating_add(1);
                    inner.status.phase = if inner.hint.is_some() {
                        "verification_pending"
                    } else {
                        "error"
                    }
                    .into();
                    inner.status.error = Some(e);
                }
            }
            let delay = PERIOD * (1u64 << inner.failures.min(4));
            inner.status.next_check = inner.preferences.enabled.then(|| now() + delay);
            inner.status.available.clone()
        };
        self.emit(app);
        if let Some(notice) = advertised {
            if let Some(fleet) = app.try_state::<crate::fleet::FleetState>() {
                fleet.advertise_update(notice);
            }
        }
    }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[async_trait::async_trait]
trait Api: Sync {
    async fn get(
        &self,
        endpoint: &str,
        limit: usize,
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>>;
}
struct Gh;
#[async_trait::async_trait]
impl Api for Gh {
    async fn get(
        &self,
        endpoint: &str,
        limit: usize,
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>> {
        let mut command = tokio::process::Command::new("gh");
        command
            .args([
                "api",
                "--hostname",
                "github.com",
                "--method",
                "GET",
                endpoint,
            ])
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_PAGER", "cat")
            .env_remove("GH_DEBUG")
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .env_remove("GH_HOST")
            .env_remove("GH_REPO")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        run_command(command, limit, cancel, Duration::from_secs(120)).await
    }
}
async fn run_command(
    mut command: tokio::process::Command,
    limit: usize,
    cancel: &CancellationToken,
    timeout: Duration,
) -> Result<Vec<u8>> {
    if cancel.is_cancelled() {
        return Err(error("cancelled", "Update check cancelled"));
    }
    let mut child = command.spawn().map_err(|_| error("gh_unavailable", "GitHub CLI unavailable. Sign in locally with gh auth login; credentials are never shared."))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| error("gh_failed", "Cannot read GitHub CLI response"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| error("gh_failed", "Cannot read GitHub CLI status"))?;
    let work = async {
        let read_out = async {
            let mut bytes = Vec::new();
            stdout
                .take(limit as u64 + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| error("gh_failed", "GitHub CLI response failed"))?;
            if bytes.len() > limit {
                return Err(error(
                    "response_limit",
                    "GitHub response exceeded the size limit",
                ));
            }
            Ok(bytes)
        };
        let read_err = async {
            let mut bytes = Vec::new();
            stderr
                .take(8193)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| error("gh_failed", "GitHub CLI status failed"))?;
            if bytes.len() > 8192 {
                return Err(error(
                    "response_limit",
                    "GitHub CLI diagnostic exceeded the size limit",
                ));
            }
            Ok(bytes)
        };
        let (out, err) = tokio::try_join!(read_out, read_err)?;
        let status = child
            .wait()
            .await
            .map_err(|_| error("gh_failed", "GitHub CLI did not exit cleanly"))?;
        if !status.success() {
            return Err(classify_failure(&err));
        }
        Ok(out)
    };
    let result = tokio::select! {
        _ = cancel.cancelled() => Err(error("cancelled", "Update check cancelled")),
        result = tokio::time::timeout(timeout, work) => result.unwrap_or_else(|_| Err(error("timeout", "GitHub CLI request timed out"))),
    };
    if result.is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    result
}
fn classify_failure(stderr: &[u8]) -> UpdateError {
    let text = String::from_utf8_lossy(stderr);
    if text.contains("401") || text.contains("gh auth login") || text.contains("not logged") {
        error(
            "authentication_required",
            "Sign in locally with gh auth login to verify private build artifacts",
        )
    } else if text.contains("403") {
        error(
            "access_denied",
            "GitHub denied access or rate-limited this check (403)",
        )
    } else if text.contains("404") {
        error(
            "not_found_or_private",
            "Build workflow/artifact not found or not accessible to your GitHub account (404)",
        )
    } else {
        error(
            "gh_failed",
            "GitHub CLI request failed. Check local authentication and network access.",
        )
    }
}
async fn json(api: &impl Api, endpoint: &str, cancel: &CancellationToken) -> Result<Value> {
    let bytes = api.get(endpoint, JSON_LIMIT, cancel).await?;
    serde_json::from_slice(&bytes)
        .map_err(|_| error("invalid_metadata", "GitHub returned invalid build metadata"))
}
fn invalid() -> UpdateError {
    error(
        "invalid_metadata",
        "Build evidence does not match the trusted workflow contract",
    )
}
fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key].as_str().ok_or_else(invalid)
}

async fn discover(
    api: &impl Api,
    cancel: &CancellationToken,
    _hint: Option<&UpdateHint>,
) -> Result<Option<(UpdateHint, bool)>> {
    let workflow = json(
        api,
        &format!("repos/{BUILD_REPOSITORY}/actions/workflows/{WORKFLOW}"),
        cancel,
    )
    .await?;
    if workflow["path"] != format!(".github/workflows/{WORKFLOW}") || workflow["state"] != "active"
    {
        return Err(invalid());
    }
    let workflow_id = workflow["id"]
        .as_u64()
        .filter(|id| *id > 0)
        .ok_or_else(invalid)?;
    let list = json(api, &format!("repos/{BUILD_REPOSITORY}/actions/workflows/{WORKFLOW}/runs?branch=main&event=workflow_dispatch&status=success&per_page=10"), cancel).await?;
    let runs = list["workflow_runs"]
        .as_array()
        .ok_or_else(invalid)?
        .clone();
    if runs.len() > 10 {
        return Err(invalid());
    }
    // A peer hint cannot constrain or fail independent discovery from the trusted workflow.
    let mut best: Option<(UpdateHint, bool)> = None;
    for run in runs {
        if cancel.is_cancelled() {
            return Err(error("cancelled", "Update check cancelled"));
        }
        let run_id = run["id"]
            .as_u64()
            .filter(|id| *id > 0)
            .ok_or_else(invalid)?;
        let build_sha = string(&run, "head_sha")?;
        if run["workflow_id"].as_u64() != Some(workflow_id)
            || run["head_branch"] != "main"
            || run["event"] != "workflow_dispatch"
            || run["status"] != "completed"
            || run["conclusion"] != "success"
            || run["repository"]["full_name"] != BUILD_REPOSITORY
            || run["head_repository"]["full_name"] != BUILD_REPOSITORY
            || !hex(build_sha, 40)
            || run["path"] != format!(".github/workflows/{WORKFLOW}")
        {
            return Err(invalid());
        }
        let artifacts = json(
            api,
            &format!("repos/{BUILD_REPOSITORY}/actions/runs/{run_id}/artifacts?per_page=20"),
            cancel,
        )
        .await?;
        let items = artifacts["artifacts"].as_array().ok_or_else(invalid)?;
        if artifacts["total_count"].as_u64().ok_or_else(invalid)? > 20 || items.len() > 20 {
            return Err(invalid());
        }
        for item in items {
            let name = string(item, "name")?;
            let Some(identity) = name.strip_prefix("darbot-desktop-windows-") else {
                continue;
            };
            let Some((candidate, source_sha)) = identity.rsplit_once('-') else {
                return Err(invalid());
            };
            if version(candidate).is_none() || !hex(source_sha, 40) {
                return Err(invalid());
            }
            if !newer(candidate, env!("CARGO_PKG_VERSION"))
                || best
                    .as_ref()
                    .is_some_and(|(b, _)| !newer(candidate, &b.version))
            {
                continue;
            }
            if item["expired"] != false {
                continue;
            }
            let artifact_id = item["id"]
                .as_u64()
                .filter(|id| *id > 0)
                .ok_or_else(invalid)?;
            if item["workflow_run"]["id"].as_u64() != Some(run_id)
                || item["workflow_run"]["head_sha"] != build_sha
                || item["workflow_run"]["head_branch"] != "main"
                || item["size_in_bytes"]
                    .as_u64()
                    .is_none_or(|size| size == 0 || size > ARCHIVE_LIMIT as u64)
            {
                return Err(invalid());
            }
            let notice = UpdateHint {
                version: candidate.into(),
                build_repository: BUILD_REPOSITORY.into(),
                source_repository: SOURCE_REPOSITORY.into(),
                workflow: WORKFLOW.into(),
                run_id,
                artifact_id,
                build_sha: build_sha.into(),
                source_sha: source_sha.into(),
            };
            // The workflow's source commit must still be reachable from public main.
            let compare = json(
                api,
                &format!("repos/{SOURCE_REPOSITORY}/compare/{source_sha}...main"),
                cancel,
            )
            .await?;
            if compare["base_commit"]["sha"] != source_sha
                || !matches!(compare["status"].as_str(), Some("ahead" | "identical"))
            {
                return Err(invalid());
            }
            let archive = api
                .get(
                    &format!("repos/{BUILD_REPOSITORY}/actions/artifacts/{artifact_id}/zip"),
                    ARCHIVE_LIMIT,
                    cancel,
                )
                .await?;
            let digest = string(item, "digest")?;
            if digest != format!("sha256:{:x}", Sha256::digest(&archive)) {
                return Err(error(
                    "digest_mismatch",
                    "GitHub artifact digest did not match",
                ));
            }
            let checked = notice.clone();
            let signed = tokio::task::spawn_blocking(move || verify_archive(&archive, &checked))
                .await
                .map_err(|_| invalid())??;
            best = Some((notice, signed));
        }
    }
    Ok(best)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Manifest {
    schema: String,
    schema_version: u32,
    version: String,
    source_repository: String,
    source_sha: String,
    build_repository: String,
    workflow: String,
    run_id: u64,
    installer_file: String,
    installer_sha256: String,
    installed_exe_sha256: String,
    signed: bool,
}
fn verify_archive(bytes: &[u8], notice: &UpdateHint) -> Result<bool> {
    if bytes.len() > ARCHIVE_LIMIT {
        return Err(invalid());
    }
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|_| invalid())?;
    if archive.len() != 2 {
        return Err(invalid());
    }
    let mut manifest_bytes = Vec::new();
    {
        let manifest = archive
            .by_name("release-manifest.json")
            .map_err(|_| invalid())?;
        if manifest.size() > MANIFEST_LIMIT {
            return Err(invalid());
        }
        manifest
            .take(MANIFEST_LIMIT + 1)
            .read_to_end(&mut manifest_bytes)
            .map_err(|_| invalid())?;
        if manifest_bytes.len() as u64 > MANIFEST_LIMIT {
            return Err(invalid());
        }
    }
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes).map_err(|_| invalid())?;
    if manifest.schema != "darbot.desktop.release"
        || manifest.schema_version != 1
        || manifest.version != notice.version
        || manifest.source_repository != SOURCE_REPOSITORY
        || manifest.source_sha != notice.source_sha
        || manifest.build_repository != BUILD_REPOSITORY
        || manifest.workflow != WORKFLOW
        || manifest.run_id != notice.run_id
        || !hex(&manifest.installer_sha256, 64)
        || !hex(&manifest.installed_exe_sha256, 64)
        || manifest.installer_file.len() > 180
        || !manifest.installer_file.ends_with(".exe")
        || !manifest
            .installer_file
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
        || manifest.installer_file.starts_with('.')
    {
        return Err(invalid());
    }
    let installer = archive
        .by_name(&manifest.installer_file)
        .map_err(|_| invalid())?;
    if installer.size() == 0 || installer.size() > ARCHIVE_LIMIT as u64 {
        return Err(invalid());
    }
    let mut reader = installer.take(ARCHIVE_LIMIT as u64 + 1);
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    let mut count = 0;
    loop {
        let length = reader.read(&mut buffer).map_err(|_| invalid())?;
        if length == 0 {
            break;
        }
        count += length;
        if count > ARCHIVE_LIMIT {
            return Err(invalid());
        }
        hash.update(&buffer[..length]);
    }
    if format!("{:x}", hash.finalize()) != manifest.installer_sha256 {
        return Err(error(
            "digest_mismatch",
            "Installer digest did not match its manifest",
        ));
    }
    Ok(manifest.signed)
}

#[tauri::command]
pub fn desktop_update_status(state: tauri::State<'_, Arc<UpdateState>>) -> UpdateStatus {
    state.status()
}
#[tauri::command]
pub async fn desktop_update_check(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, Arc<UpdateState>>,
) -> Result<UpdateStatus> {
    local(&window)?;
    state.run_check(&app, true).await;
    Ok(state.status())
}
fn local(window: &tauri::WebviewWindow) -> Result<()> {
    crate::app_control::main_window(window)
        .map_err(|_| error("forbidden", "Update controls require the local desktop"))
}
#[tauri::command]
pub async fn desktop_update_enable(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, Arc<UpdateState>>,
    enabled: bool,
) -> Result<UpdateStatus> {
    local(&window)?;
    if enabled {
        crate::app_control::approve_fleet_owner(&app,
            "Enable desktop update notices?\n\nCheck DarbotLM/darbot build artifacts at startup and every 15 minutes using your local GitHub CLI sign-in. Only verified update notices are shared with paired peers. Nothing is installed automatically.".into())
            .await.map_err(|e| error(e.code, &e.message))?;
    }
    state.set_enabled(enabled)?;
    state.emit(&app);
    Ok(state.status())
}
#[tauri::command]
pub fn desktop_update_dismiss(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, Arc<UpdateState>>,
) -> Result<UpdateStatus> {
    local(&window)?;
    state.dismiss()?;
    state.emit(&app);
    Ok(state.status())
}
#[tauri::command]
pub async fn desktop_update_view(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, Arc<UpdateState>>,
) -> Result<()> {
    local(&window)?;
    let expected = state
        .status()
        .available
        .ok_or_else(|| error("not_verified", "Check for a verified update first"))?;
    state.run_check(&app, true).await;
    let current = state
        .status()
        .available
        .ok_or_else(|| error("not_verified", "Update could not be verified again"))?;
    if current != expected {
        return Err(error(
            "changed",
            "The available build changed; review it before opening",
        ));
    }
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_url(current.url(), None::<&str>)
        .map_err(|_| {
            error(
                "open_failed",
                "Could not open the trusted GitHub build page",
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn notice() -> UpdateHint {
        UpdateHint {
            version: "0.0.30".into(),
            build_repository: BUILD_REPOSITORY.into(),
            source_repository: SOURCE_REPOSITORY.into(),
            workflow: WORKFLOW.into(),
            run_id: 7,
            artifact_id: 8,
            build_sha: "a".repeat(40),
            source_sha: "b".repeat(40),
        }
    }
    fn archive(mut edit: impl FnMut(&mut Value)) -> Vec<u8> {
        let hint = notice();
        let bytes = b"owned archive fixture; never executed";
        let mut manifest = json!({
            "schema":"darbot.desktop.release", "schemaVersion":1, "version":hint.version,
            "sourceRepository":SOURCE_REPOSITORY, "sourceSha":hint.source_sha,
            "buildRepository":BUILD_REPOSITORY, "workflow":WORKFLOW, "runId":hint.run_id,
            "installerFile":"darbot_0.0.30_x64-setup.exe",
            "installerSha256":format!("{:x}",Sha256::digest(bytes)),
            "installedExeSha256":"c".repeat(64), "signed":false
        });
        edit(&mut manifest);
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.start_file("release-manifest.json", options).unwrap();
        writer
            .write_all(&serde_json::to_vec(&manifest).unwrap())
            .unwrap();
        writer
            .start_file("darbot_0.0.30_x64-setup.exe", options)
            .unwrap();
        writer.write_all(bytes).unwrap();
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn notice_is_bound_to_repository_workflow_and_immutable_build() {
        let mut hint = notice();
        assert!(hint.validate().is_ok());
        hint.build_repository = "untrusted/repo".into();
        assert!(hint.validate().is_err());
        hint = notice();
        hint.source_sha = "../main".into();
        assert!(hint.validate().is_err());
        assert!(newer("0.0.30", "0.0.29"));
        assert!(!newer("0.0.9", "0.0.29"));
        assert!(!newer("0.0.29", "0.0.29"));
        assert!(version("0.00.30").is_none());
    }

    #[test]
    fn archive_checks_identity_and_actual_installer_digest() {
        assert!(!verify_archive(&archive(|_| {}), &notice()).unwrap());
        for key in [
            "sourceSha",
            "workflow",
            "sourceRepository",
            "installerSha256",
            "installerFile",
        ] {
            let data = archive(|manifest| {
                manifest[key] = json!("not-the-approved-value");
            });
            assert!(verify_archive(&data, &notice()).is_err(), "{key}");
        }
    }

    #[test]
    fn peer_hint_does_not_enable_checks_or_create_a_verified_notice() {
        let state = UpdateState::default();
        state.receive_hint(notice());
        let status = state.status();
        assert!(!status.enabled);
        assert!(!status.notify);
        assert!(status.available.is_none());
        assert_eq!(status.phase, "verification_pending");
    }

    #[test]
    fn authentication_errors_never_echo_cli_secrets() {
        let failure = classify_failure(b"401 Authorization: private-value");
        assert_eq!(failure.code, "authentication_required");
        assert!(!failure.message.contains("private-value"));
    }
    #[tokio::test]
    async fn an_unavailable_peer_hint_cannot_override_canonical_discovery() {
        struct Canonical;
        #[async_trait::async_trait]
        impl Api for Canonical {
            async fn get(
                &self,
                endpoint: &str,
                _limit: usize,
                _cancel: &CancellationToken,
            ) -> Result<Vec<u8>> {
                let value = if endpoint.ends_with(WORKFLOW) {
                    json!({"path":format!(".github/workflows/{WORKFLOW}"),"state":"active","id":1})
                } else if endpoint.contains("/runs?") {
                    json!({"workflow_runs":[]})
                } else {
                    return Err(error("not_found_or_private", "Hint run does not exist"));
                };
                Ok(serde_json::to_vec(&value).unwrap())
            }
        }
        assert!(
            discover(&Canonical, &CancellationToken::new(), Some(&notice()))
                .await
                .unwrap()
                .is_none()
        );
    }
}
