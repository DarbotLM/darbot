//! One-shot, native-owned consent. Renderer decisions never carry executable operations.

use std::collections::HashMap;
use std::sync::mpsc::{self, Sender};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Runtime};

use crate::copilot::{CopilotPermissionOption, CopilotPermissionOrigin, CopilotPermissionRequest};
use crate::problem::Problem;

const CONSENT_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_PENDING: usize = 32;
const MAX_DETAILS: usize = 32 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsentDecision {
    Allow,
    Reject,
    Cancel,
}

struct PendingConsent {
    owner: String,
    session_id: String,
    sender: Sender<ConsentDecision>,
}

#[derive(Default)]
pub struct ClientConsent {
    pending: Mutex<HashMap<String, PendingConsent>>,
}

impl ClientConsent {
    pub fn ask<R: Runtime>(
        &self,
        app: &AppHandle<R>,
        owner: &str,
        session_id: &str,
        origin: CopilotPermissionOrigin,
        details: Value,
    ) -> Result<ConsentDecision, Problem> {
        self.ask_if(app, owner, session_id, origin, details, || true)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn ask_if<R: Runtime>(
        &self,
        app: &AppHandle<R>,
        owner: &str,
        session_id: &str,
        origin: CopilotPermissionOrigin,
        details: Value,
        current: impl FnOnce() -> bool,
    ) -> Result<ConsentDecision, Problem> {
        self.ask_with(
            owner,
            session_id,
            origin,
            details,
            CONSENT_TIMEOUT,
            |request| {
                if !current() {
                    return Err(Problem::plain(
                        "The operation was cancelled before approval.",
                    ));
                }
                app.emit("copilot:permission-request", request)
                    .map_err(|error| {
                        Problem::with(
                            "Darbot could not show the requested permission.",
                            error.to_string(),
                        )
                    })
            },
            |request_id| {
                if let Err(error) = app.emit(
                    "copilot:permission-closed",
                    json!({"requestId": request_id, "sessionId": session_id}),
                ) {
                    eprintln!("Darbot could not close a permission notification: {error}");
                }
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn ask_with(
        &self,
        owner: &str,
        session_id: &str,
        origin: CopilotPermissionOrigin,
        details: Value,
        timeout: Duration,
        publish: impl FnOnce(&CopilotPermissionRequest) -> Result<(), Problem>,
        close: impl FnOnce(&str),
    ) -> Result<ConsentDecision, Problem> {
        if owner.is_empty() || session_id.is_empty() || details.to_string().len() > MAX_DETAILS {
            return Err(Problem::plain(
                "The native permission request has invalid ownership or oversized details.",
            ));
        }
        let request_id = format!("client:{:032x}", rand::random::<u128>());
        let (sender, receiver) = mpsc::channel();
        {
            let mut pending = self.pending.lock().unwrap();
            if pending.len() >= MAX_PENDING {
                return Err(Problem::plain(
                    "Too many permission requests are pending. Answer or cancel them first.",
                ));
            }
            pending.insert(
                request_id.clone(),
                PendingConsent {
                    owner: owner.into(),
                    session_id: session_id.into(),
                    sender,
                },
            );
        }
        let request = CopilotPermissionRequest {
            request_id: request_id.clone(),
            session_id: session_id.into(),
            origin,
            tool_call: details,
            options: vec![
                CopilotPermissionOption {
                    option_id: "allow-once".into(),
                    name: "Allow once".into(),
                    kind: "allow_once".into(),
                },
                CopilotPermissionOption {
                    option_id: "reject-once".into(),
                    name: "Reject".into(),
                    kind: "reject_once".into(),
                },
            ],
        };
        let result = publish(&request).and_then(|()| {
            receiver.recv_timeout(timeout).map_err(|error| {
                Problem::with(
                    "The permission request expired or its owner disconnected. No action was approved.",
                    error.to_string(),
                )
            })
        });
        self.pending.lock().unwrap().remove(&request_id);
        close(&request_id);
        result
    }

    pub fn respond(&self, request_id: &str, option_id: Option<&str>) -> Result<(), Problem> {
        let decision = match option_id {
            Some("allow-once") => ConsentDecision::Allow,
            Some("reject-once") => ConsentDecision::Reject,
            None => ConsentDecision::Cancel,
            _ => {
                return Err(Problem::plain(
                    "That decision was not offered by the native permission request.",
                ));
            }
        };
        let pending = self
            .pending
            .lock()
            .unwrap()
            .remove(request_id)
            .ok_or_else(|| {
                Problem::plain("That native permission request is no longer pending.")
            })?;
        pending.sender.send(decision).map_err(|_| {
            Problem::plain("The operation ended before its permission decision arrived.")
        })
    }

    pub fn cancel(&self, owner: &str, session_id: Option<&str>) {
        let mut pending = self.pending.lock().unwrap();
        let ids: Vec<_> = pending
            .iter()
            .filter(|(_, request)| {
                request.owner == owner
                    && session_id.is_none_or(|session| request.session_id == session)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Some(request) = pending.remove(&id) {
                // An expired receiver already means the operation cannot obtain approval.
                let _ = request.sender.send(ConsentDecision::Cancel);
            }
        }
    }
}

/// ACP terminals are pipes, not interactive shells. Executable/argv separation is preserved.
pub(crate) mod terminals {
    use std::collections::{HashMap, HashSet};
    use std::io::{self, Read};
    use std::path::{Path, PathBuf};
    use std::process::{Child, ExitStatus, Stdio};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    use serde::{Deserialize, Serialize};
    use serde_json::{json, Value};

    use crate::copilot_files;
    use crate::problem::Problem;

    const MAX_TERMINALS: usize = 16;
    const MAX_REQUEST_BYTES: usize = 24 * 1024;
    const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
    const DEFAULT_OUTPUT_BYTES: usize = 64 * 1024;
    #[cfg(unix)]
    const PIPE_CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    pub(crate) struct Create {
        pub session_id: String,
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Vec<Environment>,
        cwd: Option<String>,
        output_byte_limit: Option<usize>,
        #[serde(rename = "_meta")]
        _meta: Option<Value>,
    }

    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct Environment {
        name: String,
        value: String,
        #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
        _meta: Option<Value>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    pub(crate) struct Handle {
        pub session_id: String,
        pub terminal_id: String,
        #[serde(rename = "_meta")]
        _meta: Option<Value>,
    }

    impl Handle {
        pub(crate) fn validate(&self) -> Result<(), Problem> {
            if self.session_id.is_empty()
                || self.terminal_id.is_empty()
                || self.session_id.len() > 4096
                || self.terminal_id.len() > 4096
                || self._meta.as_ref().is_some_and(|meta| !meta.is_object())
            {
                return Err(problem("Invalid ACP terminal handle."));
            }
            Ok(())
        }
    }

    #[derive(Clone, Debug, Serialize)]
    #[serde(rename_all = "camelCase")]
    pub(crate) struct Status {
        exit_code: Option<i64>,
        signal: Option<String>,
    }

    #[derive(Clone, Serialize)]
    #[serde(rename_all = "camelCase")]
    pub(crate) struct Snapshot {
        output: String,
        truncated: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_status: Option<Status>,
    }

    pub(crate) type Publish =
        Arc<dyn Fn(&str, &str, &Snapshot) -> Result<(), String> + Send + Sync>;

    #[derive(Default)]
    struct Registry {
        stopped: bool,
        sessions: HashMap<String, u128>,
        reservations: usize,
        spawning: usize,
        terminals: HashMap<String, Arc<Terminal>>,
        disposing: HashMap<String, Arc<Terminal>>,
    }

    #[derive(Default)]
    pub(crate) struct Manager {
        state: Mutex<Registry>,
        quiesced: Condvar,
    }

    pub(crate) struct Reservation {
        manager: Arc<Manager>,
        session_id: String,
        generation: u128,
    }

    impl Drop for Reservation {
        fn drop(&mut self) {
            self.manager.state.lock().unwrap().reservations -= 1;
        }
    }

    impl Reservation {
        pub(crate) fn is_current(&self) -> bool {
            self.manager.current(self)
        }
    }

    struct Spawning<'a>(&'a Manager);

    impl Drop for Spawning<'_> {
        fn drop(&mut self) {
            self.0.state.lock().unwrap().spawning -= 1;
            self.0.quiesced.notify_all();
        }
    }

    struct Output {
        snapshot: Snapshot,
        error: Option<String>,
        finished: bool,
        released: bool,
        revision: u64,
    }

    struct Terminal {
        session_id: String,
        output: Mutex<Output>,
        completed: Condvar,
        tree: ProcessTree,
        limit: usize,
        #[cfg(unix)]
        drain_deadline: Mutex<Option<std::time::Instant>>,
    }

    fn problem(detail: impl Into<String>) -> Problem {
        Problem::with("The ACP terminal operation failed.", detail.into())
    }

    impl Create {
        pub(crate) fn validate(&self) -> Result<(), Problem> {
            let mut names = HashSet::new();
            if self.command.is_empty()
                || self.command.contains('\0')
                || self.command.len() > 4096
                || self.session_id.is_empty()
                || self.session_id.len() > 4096
                || self.args.len() > 256
                || self.env.len() > 128
                || self.args.iter().any(|arg| arg.contains('\0'))
                || self.env.iter().any(|var| {
                    var.name.is_empty()
                        || var.name.contains(['=', '\0'])
                        || var.value.contains('\0')
                        || !names.insert(if cfg!(windows) {
                            var.name.to_uppercase()
                        } else {
                            var.name.clone()
                        })
                })
                || self.output_byte_limit.is_some_and(|n| n > MAX_OUTPUT_BYTES)
                || self.cwd.as_ref().is_some_and(|cwd| {
                    cwd.is_empty()
                        || cwd.len() > 4096
                        || cwd.contains('\0')
                        || !Path::new(cwd).is_absolute()
                })
                || self._meta.as_ref().is_some_and(|meta| !meta.is_object())
                || self
                    .env
                    .iter()
                    .any(|env| env._meta.as_ref().is_some_and(|meta| !meta.is_object()))
                || (cfg!(windows)
                    && Path::new(&self.command)
                        .extension()
                        .is_some_and(|extension| {
                            matches!(
                                extension.to_string_lossy().to_ascii_lowercase().as_str(),
                                "bat" | "cmd"
                            )
                        }))
            {
                return Err(problem(
                    "Invalid command, arguments, environment or output byte limit (maximum 1 MiB).",
                ));
            }
            // Bound what is sent to consent, including JSON escaping and metadata.
            if self.details(Path::new("")).to_string().len()
                + self.cwd.as_ref().map_or(0, String::len)
                + self._meta.as_ref().map_or(0, |v| v.to_string().len())
                > MAX_REQUEST_BYTES
            {
                return Err(problem("Terminal requests are limited to 24 KiB."));
            }
            Ok(())
        }

        pub(crate) fn directory(&self, root: &str, home: &Path) -> Result<PathBuf, Problem> {
            let requested = self.cwd.as_deref().unwrap_or(root);
            if requested.is_empty()
                || requested.len() > 4096
                || requested.contains('\0')
                || !Path::new(requested).is_absolute()
            {
                return Err(problem(
                    "Terminal cwd must be an absolute project directory.",
                ));
            }
            // Reuse the host broker's canonical root/home/system/private-path policy without
            // creating a file. Its write resolver canonicalizes and validates this parent.
            let probe = Path::new(requested).join(".darbot-terminal-directory-check");
            let resolved = copilot_files::resolve(
                root,
                probe
                    .to_str()
                    .ok_or_else(|| problem("Invalid cwd encoding."))?,
                true,
                home,
            )?;
            Ok(resolved.parent().unwrap().to_path_buf())
        }

        pub(crate) fn details(&self, cwd: &Path) -> Value {
            json!({
                "title": "Run a command on this computer",
                "command": self.command,
                "args": self.args,
                "env": self.env,
                "workingDirectory": cwd,
                "warning": "This executes a native command, not a filesystem sandbox. It can access files, credentials, the network and other resources available to your user account.",
            })
        }
    }

    impl Manager {
        pub(crate) fn generation(&self, session_id: &str) -> Option<u128> {
            let state = self.state.lock().unwrap();
            if state.stopped {
                None
            } else {
                state.sessions.get(session_id).copied()
            }
        }

        pub(crate) fn activate(&self, session_id: &str) -> Result<(), Problem> {
            let mut state = self.state.lock().unwrap();
            if state.stopped || state.sessions.contains_key(session_id) {
                return Err(problem("The terminal session is closed or already active."));
            }
            state.sessions.insert(session_id.into(), rand::random());
            Ok(())
        }

        pub(crate) fn reserve(self: &Arc<Self>, session_id: &str) -> Result<Reservation, Problem> {
            let mut state = self.state.lock().unwrap();
            let generation = *state
                .sessions
                .get(session_id)
                .filter(|_| !state.stopped)
                .ok_or_else(|| {
                    problem("Terminal access requires this runtime's active session.")
                })?;
            if state.reservations + state.terminals.len() + state.disposing.len() >= MAX_TERMINALS {
                return Err(problem(
                    "Release existing terminals before creating more (maximum 16).",
                ));
            }
            state.reservations += 1;
            Ok(Reservation {
                manager: Arc::clone(self),
                session_id: session_id.into(),
                generation,
            })
        }

        fn current(&self, reservation: &Reservation) -> bool {
            let state = self.state.lock().unwrap();
            !state.stopped
                && state.sessions.get(&reservation.session_id) == Some(&reservation.generation)
        }

        pub(crate) fn create(
            &self,
            reservation: Reservation,
            request: &Create,
            directory: &Path,
            publish: Publish,
        ) -> Result<String, Problem> {
            request.validate()?;
            let spawning = {
                let mut state = self.state.lock().unwrap();
                if request.session_id != reservation.session_id
                    || state.stopped
                    || state.sessions.get(&reservation.session_id) != Some(&reservation.generation)
                {
                    return Err(problem("The command was cancelled before it could start."));
                }
                state.spawning += 1;
                Spawning(self)
            };
            let (mut child, tree) = ProcessTree::spawn(request, directory).map_err(|error| {
                problem(format!("Could not start the approved executable: {error}"))
            })?;
            let terminal = Arc::new(Terminal {
                session_id: request.session_id.clone(),
                output: Mutex::new(Output {
                    snapshot: Snapshot {
                        output: String::new(),
                        truncated: false,
                        exit_status: None,
                    },
                    error: None,
                    finished: false,
                    released: false,
                    revision: 0,
                }),
                completed: Condvar::new(),
                tree,
                limit: request.output_byte_limit.unwrap_or(DEFAULT_OUTPUT_BYTES),
                #[cfg(unix)]
                drain_deadline: Mutex::new(None),
            });
            let id = format!("terminal-{:032x}", rand::random::<u128>());
            // Cancellation can occur during spawn; the unregistered child is still owned and
            // killed/reaped before this call returns. No session/global lock spans OS work.
            let inserted = {
                let mut state = self.state.lock().unwrap();
                if !state.stopped
                    && state.sessions.get(&reservation.session_id) == Some(&reservation.generation)
                {
                    state.terminals.insert(id.clone(), Arc::clone(&terminal));
                    true
                } else {
                    state.disposing.insert(id.clone(), Arc::clone(&terminal));
                    false
                }
            };
            let stdout = child.stdout.take().unwrap();
            let stderr = child.stderr.take().unwrap();
            let first = if inserted {
                terminal.publish(&id, &publish)
            } else {
                Ok(())
            };
            let out_terminal = Arc::clone(&terminal);
            let out = std::thread::spawn(move || out_terminal.drain(stdout));
            let err_terminal = Arc::clone(&terminal);
            let err = std::thread::spawn(move || err_terminal.drain(stderr));
            let supervised = Arc::clone(&terminal);
            let terminal_id = id.clone();
            std::thread::spawn(move || {
                let mut revision = 0;
                let mut ticks = 0;
                let status = loop {
                    match child.try_wait() {
                        Ok(Some(status)) => break Ok(status),
                        Err(error) => break Err(error),
                        Ok(None) => {}
                    }
                    ticks += 1;
                    if ticks % 5 == 0 {
                        let current = supervised.output.lock().unwrap().revision;
                        if current != revision {
                            revision = current;
                            if let Err(error) = supervised.publish(&terminal_id, &publish) {
                                supervised.record_error(error.detail.unwrap_or(error.said));
                                if let Err(error) = supervised.tree.kill() {
                                    supervised.record_error(error.to_string());
                                }
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(10));
                };
                // A child may leave descendants holding its pipes. Close the owned tree before
                // joining drains, including after normal leader exit.
                if let Err(error) = supervised.tree.kill() {
                    supervised.record_error(format!("Command tree cleanup failed: {error}"));
                }
                // setsid descendants can escape the group and keep a writer open. Allow
                // buffered trailing output to drain, but never wait indefinitely for EOF.
                #[cfg(unix)]
                {
                    *supervised.drain_deadline.lock().unwrap() =
                        Some(std::time::Instant::now() + PIPE_CLEANUP_TIMEOUT);
                }
                if status.is_err() {
                    if let Err(error) = child.kill() {
                        supervised.record_error(format!("Command cleanup failed: {error}"));
                    }
                    if let Err(error) = child.wait() {
                        supervised.record_error(format!("Command reap failed: {error}"));
                    }
                }
                let stdout_joined = out.join();
                let stderr_joined = err.join();
                if stdout_joined.is_err() || stderr_joined.is_err() {
                    supervised.record_error("An output reader stopped unexpectedly.".into());
                }
                {
                    let mut output = supervised.output.lock().unwrap();
                    match status {
                        Ok(status) => output.snapshot.exit_status = Some(exit_status(status)),
                        Err(error) => output.error = Some(format!("Command wait failed: {error}")),
                    }
                }
                if let Err(error) = supervised.publish(&terminal_id, &publish) {
                    supervised.record_error(error.detail.unwrap_or(error.said));
                }
                supervised.output.lock().unwrap().finished = true;
                supervised.completed.notify_all();
            });
            drop(spawning);
            if !inserted {
                let disposed = terminal.dispose();
                self.state.lock().unwrap().disposing.remove(&id);
                disposed?;
                return Err(problem("The command was cancelled while it was starting."));
            }
            if let Err(error) = first {
                self.release(&Handle {
                    session_id: request.session_id.clone(),
                    terminal_id: id,
                    _meta: None,
                })?;
                return Err(error);
            }
            if !self.current(&reservation) {
                terminal
                    .tree
                    .kill()
                    .map_err(|error| problem(error.to_string()))?;
                terminal.wait(true)?;
                let mut state = self.state.lock().unwrap();
                state.terminals.remove(&id);
                state.disposing.remove(&id);
                return Err(problem("The command was cancelled while it was starting."));
            }
            Ok(id)
        }

        fn get(&self, handle: &Handle) -> Result<Arc<Terminal>, Problem> {
            self.state
                .lock()
                .unwrap()
                .terminals
                .get(&handle.terminal_id)
                .filter(|terminal| terminal.session_id == handle.session_id)
                .cloned()
                .ok_or_else(|| problem("Unknown terminal ID for this runtime and session."))
        }

        pub(crate) fn output(&self, handle: &Handle) -> Result<Value, Problem> {
            let terminal = self.get(handle)?;
            let output = terminal.output.lock().unwrap();
            output.check()?;
            Ok(serde_json::to_value(&output.snapshot).unwrap())
        }

        pub(crate) fn wait(&self, handle: &Handle) -> Result<Value, Problem> {
            self.get(handle)?.wait(false).map(|status| json!(status))
        }

        pub(crate) fn kill(&self, handle: &Handle) -> Result<(), Problem> {
            let terminal = self.get(handle)?;
            terminal.output.lock().unwrap().check()?;
            terminal.tree.kill().map_err(|e| problem(e.to_string()))?;
            terminal.wait(false)?;
            Ok(())
        }

        pub(crate) fn release(&self, handle: &Handle) -> Result<(), Problem> {
            let terminal = {
                let mut state = self.state.lock().unwrap();
                let terminal = state
                    .terminals
                    .get(&handle.terminal_id)
                    .filter(|terminal| terminal.session_id == handle.session_id)
                    .ok_or_else(|| problem("Unknown terminal ID for this runtime and session."))?;
                terminal.output.lock().unwrap().released = true;
                let terminal = state.terminals.remove(&handle.terminal_id).unwrap();
                state
                    .disposing
                    .insert(handle.terminal_id.clone(), Arc::clone(&terminal));
                terminal
            };
            let result = terminal.dispose();
            self.state
                .lock()
                .unwrap()
                .disposing
                .remove(&handle.terminal_id);
            result
        }

        /// Cancel retains IDs and final output; close/stop invalidates them.
        pub(crate) fn cancel(&self, session: Option<&str>, close: bool) -> Result<(), Problem> {
            let terminals = {
                let mut state = self.state.lock().unwrap();
                if let Some(session) = session {
                    if close {
                        state.sessions.remove(session);
                    } else if let Some(generation) = state.sessions.get_mut(session) {
                        *generation = rand::random();
                    }
                } else {
                    state.stopped = true;
                    state.sessions.clear();
                }
                while state.spawning != 0 {
                    state = self.quiesced.wait(state).unwrap();
                }
                let ids: Vec<_> = state
                    .terminals
                    .iter()
                    .chain(state.disposing.iter())
                    .filter(|(_, t)| session.is_none_or(|s| t.session_id == s))
                    .map(|(id, t)| (id.clone(), Arc::clone(t)))
                    .collect();
                if close {
                    for (id, terminal) in &ids {
                        terminal.output.lock().unwrap().released = true;
                        if let Some(terminal) = state.terminals.remove(id) {
                            state.disposing.insert(id.clone(), terminal);
                        }
                    }
                }
                ids
            };
            let mut errors = Vec::new();
            // Signal every tree before waiting for any one of them.
            for (_, terminal) in &terminals {
                if let Err(error) = terminal.tree.kill() {
                    errors.push(error.to_string());
                }
            }
            for (_, terminal) in &terminals {
                if let Err(error) = terminal.wait(true) {
                    errors.push(error.detail.unwrap_or(error.said));
                }
            }
            if close {
                let mut state = self.state.lock().unwrap();
                for (id, _) in terminals {
                    state.disposing.remove(&id);
                }
            }
            if errors.is_empty() {
                Ok(())
            } else {
                Err(problem(errors.join("; ")))
            }
        }
    }

    impl Drop for Manager {
        fn drop(&mut self) {
            if let Err(error) = self.cancel(None, true) {
                // Drop cannot report through the wire; the ordinary stop/EOF path reports this.
                // Never include command, environment or captured output in diagnostics.
                eprintln!("{}", error.said);
            }
        }
    }

    impl Output {
        fn check(&self) -> Result<(), Problem> {
            if self.released {
                Err(problem("The terminal has been released."))
            } else if let Some(error) = &self.error {
                Err(problem(error.clone()))
            } else {
                Ok(())
            }
        }
    }

    impl Terminal {
        fn record_error(&self, error: String) {
            let mut output = self.output.lock().unwrap();
            if output.error.is_none() {
                output.error = Some(error);
            }
        }

        fn publish(&self, id: &str, publish: &Publish) -> Result<(), Problem> {
            let snapshot = self.output.lock().unwrap().snapshot.clone();
            publish(&self.session_id, id, &snapshot).map_err(problem)
        }

        fn append(&self, text: &str) {
            if text.is_empty() {
                return;
            }
            let mut output = self.output.lock().unwrap();
            output.snapshot.output.push_str(text);
            if output.snapshot.output.len() > self.limit {
                let mut start = output.snapshot.output.len() - self.limit;
                while !output.snapshot.output.is_char_boundary(start) {
                    start += 1;
                }
                output.snapshot.output.drain(..start);
                output.snapshot.truncated = true;
            }
            output.revision = output.revision.wrapping_add(1);
        }

        #[cfg(not(unix))]
        fn drain(&self, pipe: impl Read) {
            self.drain_reader(pipe);
        }

        #[cfg(unix)]
        fn drain(&self, pipe: impl Read + std::os::fd::AsRawFd) {
            let fd = pipe.as_raw_fd();
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                self.output_read_failed(io::Error::last_os_error());
                return;
            }
            self.drain_reader(UnixPipe {
                pipe,
                terminal: self,
            });
        }

        fn output_read_failed(&self, error: io::Error) {
            self.record_error(format!("Command output read failed: {error}"));
            if let Err(error) = self.tree.kill() {
                self.record_error(error.to_string());
            }
        }

        fn drain_reader(&self, mut pipe: impl Read) {
            let mut decoder = Utf8::default();
            let mut bytes = [0; 4096];
            loop {
                match pipe.read(&mut bytes) {
                    Ok(0) => {
                        self.append(&decoder.decode(&[], true));
                        break;
                    }
                    Ok(count) => self.append(&decoder.decode(&bytes[..count], false)),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        #[cfg(unix)]
                        self.append(&decoder.decode(&[], true));
                        self.output_read_failed(error);
                        break;
                    }
                }
            }
        }

        fn wait(&self, released_ok: bool) -> Result<Status, Problem> {
            let mut output = self.output.lock().unwrap();
            while !output.finished {
                output = self.completed.wait(output).unwrap();
            }
            if let Some(error) = &output.error {
                return Err(problem(error.clone()));
            }
            if !released_ok {
                output.check()?;
            }
            output
                .snapshot
                .exit_status
                .clone()
                .ok_or_else(|| problem("Missing command exit status."))
        }

        fn dispose(&self) -> Result<(), Problem> {
            let killed = self.tree.kill().map_err(|e| problem(e.to_string()));
            let waited = self.wait(true);
            killed?;
            waited?;
            Ok(())
        }
    }

    #[cfg(unix)]
    struct UnixPipe<'a, T> {
        pipe: T,
        terminal: &'a Terminal,
    }

    #[cfg(unix)]
    impl<T: Read + std::os::fd::AsRawFd> Read for UnixPipe<'_, T> {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            loop {
                // Check even while data is available: an escaped writer can flood forever.
                if self
                    .terminal
                    .drain_deadline
                    .lock()
                    .unwrap()
                    .is_some_and(|deadline| std::time::Instant::now() >= deadline)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "Command tree cleanup failed: output pipe did not close; an escaped \
                         process may still be running and process ownership cannot be guaranteed.",
                    ));
                }
                match self.pipe.read(bytes) {
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    result => return result,
                }
                let mut descriptor = libc::pollfd {
                    fd: self.pipe.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // POLLHUP still requires a read: bytes queued before EOF must be preserved.
                if unsafe { libc::poll(&mut descriptor, 1, 20) } < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
        }
    }

    #[derive(Default)]
    struct Utf8 {
        pending: Vec<u8>,
    }

    impl Utf8 {
        fn decode(&mut self, bytes: &[u8], end: bool) -> String {
            self.pending.extend_from_slice(bytes);
            let mut text = String::new();
            let mut consumed = 0;
            while consumed < self.pending.len() {
                match std::str::from_utf8(&self.pending[consumed..]) {
                    Ok(valid) => {
                        text.push_str(valid);
                        consumed = self.pending.len();
                    }
                    Err(error) => {
                        let next = consumed + error.valid_up_to();
                        // valid_up_to is guaranteed to end on a character boundary.
                        text.push_str(std::str::from_utf8(&self.pending[consumed..next]).unwrap());
                        consumed = next;
                        if let Some(length) = error.error_len() {
                            text.push('\u{fffd}');
                            consumed += length;
                        } else if end {
                            text.push('\u{fffd}');
                            consumed = self.pending.len();
                        } else {
                            break;
                        }
                    }
                }
            }
            self.pending.drain(..consumed);
            text
        }
    }

    fn exit_status(status: ExitStatus) -> Status {
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt;
            status.signal().map(|signal| {
                if signal == libc::SIGKILL {
                    "SIGKILL".into()
                } else {
                    signal.to_string()
                }
            })
        };
        #[cfg(not(unix))]
        let signal = None;
        Status {
            exit_code: status.code().map(i64::from),
            signal,
        }
    }

    #[cfg(unix)]
    struct ProcessTree {
        group: i32,
        killed: Mutex<bool>,
    }

    #[cfg(unix)]
    impl ProcessTree {
        fn spawn(request: &Create, directory: &Path) -> io::Result<(Child, Self)> {
            use std::os::unix::process::CommandExt;
            let mut command = command(request, directory);
            command.process_group(0);
            let child = command.spawn()?;
            let group = i32::try_from(child.id()).map_err(io::Error::other)?;
            Ok((
                child,
                Self {
                    group,
                    killed: Mutex::new(false),
                },
            ))
        }

        fn kill(&self) -> io::Result<()> {
            let mut killed = self.killed.lock().unwrap();
            if *killed {
                return Ok(());
            }
            // This process group was created for this command alone.
            if unsafe { libc::kill(-self.group, libc::SIGKILL) } != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error);
                }
            }
            *killed = true;
            Ok(())
        }
    }

    #[cfg(unix)]
    impl Drop for ProcessTree {
        fn drop(&mut self) {
            if self.kill().is_err() {
                eprintln!("Darbot could not terminate an owned terminal process group.");
            }
        }
    }

    fn command(request: &Create, directory: &Path) -> std::process::Command {
        let mut command = crate::quiet::command(&request.command);
        command.args(&request.args).current_dir(directory);
        for variable in &request.env {
            command.env(&variable.name, &variable.value);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    #[cfg(windows)]
    struct ProcessTree {
        // Store an owned kernel handle as an integer so no raw pointer crosses threads.
        job: usize,
        killed: Mutex<bool>,
    }

    #[cfg(windows)]
    mod windows {
        use std::ffi::c_void;
        pub(super) type Handle = *mut c_void;

        #[repr(C)]
        #[derive(Default)]
        pub(super) struct BasicLimits {
            process_time: i64,
            job_time: i64,
            pub flags: u32,
            minimum_working_set: usize,
            maximum_working_set: usize,
            active_processes: u32,
            affinity: usize,
            priority: u32,
            scheduling: u32,
        }

        #[repr(C)]
        #[derive(Default)]
        pub(super) struct ExtendedLimits {
            pub basic: BasicLimits,
            io: [u64; 6],
            process_memory: usize,
            job_memory: usize,
            peak_process_memory: usize,
            peak_job_memory: usize,
        }

        #[link(name = "kernel32")]
        unsafe extern "system" {
            pub fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> Handle;
            pub fn SetInformationJobObject(
                job: Handle,
                class: i32,
                info: *const c_void,
                size: u32,
            ) -> i32;
            pub fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
            pub fn TerminateJobObject(job: Handle, code: u32) -> i32;
            pub fn CloseHandle(handle: Handle) -> i32;
        }

        #[link(name = "ntdll")]
        unsafe extern "system" {
            pub fn NtResumeProcess(process: Handle) -> i32;
        }
    }

    #[cfg(windows)]
    impl ProcessTree {
        fn spawn(request: &Create, directory: &Path) -> io::Result<(Child, Self)> {
            use std::os::windows::{io::AsRawHandle, process::CommandExt};
            use windows::*;
            let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if job.is_null() {
                return Err(io::Error::last_os_error());
            }
            let tree = Self {
                job: job as usize,
                killed: Mutex::new(false),
            };
            let mut limits = ExtendedLimits::default();
            limits.basic.flags = 0x2000; // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            if unsafe {
                SetInformationJobObject(
                    job,
                    9, // JobObjectExtendedLimitInformation
                    (&limits as *const ExtendedLimits).cast(),
                    std::mem::size_of::<ExtendedLimits>() as u32,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            let mut command = command(request, directory);
            // Suspend before any user code runs: descendants cannot escape job assignment.
            command.creation_flags(0x0800_0004); // CREATE_NO_WINDOW | CREATE_SUSPENDED
            let mut child = command.spawn()?;
            let assigned = unsafe { AssignProcessToJobObject(job, child.as_raw_handle()) };
            let failure = if assigned == 0 {
                Some(io::Error::last_os_error())
            } else {
                let status = unsafe { NtResumeProcess(child.as_raw_handle()) };
                (status < 0)
                    .then(|| io::Error::other(format!("Resume failed (NTSTATUS {status:#x}).")))
            };
            if let Some(error) = failure {
                let killed = child.kill();
                let reaped = child.wait();
                killed?;
                reaped?;
                return Err(error);
            }
            Ok((child, tree))
        }

        fn kill(&self) -> io::Result<()> {
            let mut killed = self.killed.lock().unwrap();
            if *killed {
                return Ok(());
            }
            if unsafe { windows::TerminateJobObject(self.job as windows::Handle, 1) } == 0 {
                return Err(io::Error::last_os_error());
            }
            *killed = true;
            Ok(())
        }
    }

    #[cfg(windows)]
    impl Drop for ProcessTree {
        fn drop(&mut self) {
            // Last-resort tree termination also covers unwinding; this job is never shared.
            unsafe { windows::CloseHandle(self.job as windows::Handle) };
        }
    }

    #[cfg(test)]
    pub(crate) mod test_fixture {
        use super::*;

        pub(crate) struct OwnedChild(Option<Child>);

        impl OwnedChild {
            pub(crate) fn new(child: Child) -> Self {
                Self(Some(child))
            }
            pub(crate) fn into_inner(mut self) -> Child {
                self.0.take().unwrap()
            }
        }

        impl std::ops::Deref for OwnedChild {
            type Target = Child;
            fn deref(&self) -> &Child {
                self.0.as_ref().unwrap()
            }
        }

        impl std::ops::DerefMut for OwnedChild {
            fn deref_mut(&mut self) -> &mut Child {
                self.0.as_mut().unwrap()
            }
        }

        impl Drop for OwnedChild {
            fn drop(&mut self) {
                if let Some(child) = &mut self.0 {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }

        pub(crate) struct Fixture {
            pub root: PathBuf,
            pub executable: PathBuf,
        }

        impl Fixture {
            pub(crate) fn new() -> Self {
                let root = Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join(format!(".terminal-test-{:032x}", rand::random::<u128>()));
                std::fs::create_dir(&root).unwrap();
                let source = root.join("fixture.rs");
                let executable = root.join(format!("fixture{}", std::env::consts::EXE_SUFFIX));
                std::fs::write(&source, r#"
use std::io::{BufRead, Write};
use std::time::Duration;
fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("wire") => for line in std::io::stdin().lock().lines() {
            let line = line.unwrap();
            if line == "\"not-json\"" { println!("not-json"); } else { println!("{line}"); }
            std::io::stdout().flush().unwrap();
        },
        Some("probe") => {
            let mut lines = std::io::stdin().lock().lines();
            lines.next().unwrap().unwrap();
            println!("{{\"jsonrpc\":\"2.0\",\"id\":\"callback\",\"method\":\"session/request_permission\",\"params\":{{}}}}");
            std::io::stdout().flush().unwrap();
            let reply = lines.next().unwrap().unwrap();
            assert!(reply.contains("-32601") && reply.contains("callback"));
            println!("{{\"jsonrpc\":\"2.0\",\"id\":42,\"result\":{{\"answered\":true}}}}");
            std::io::stdout().flush().unwrap();
        },
        Some("output") => {
            let mut out = std::io::stdout();
            out.write_all(&[0x41, 0xf0, 0x9f]).unwrap(); out.flush().unwrap();
            std::thread::sleep(Duration::from_millis(40));
            out.write_all(&[0x98, 0x80, 0x5a]).unwrap(); out.flush().unwrap();
            eprint!("stderr\n");
            std::thread::sleep(Duration::from_millis(150));
            std::process::exit(7);
        },
        Some("inspect") => println!("{:?}|{}|{}", &args[1..],
            std::env::var("DARBOT_TERMINAL_FIXTURE").unwrap(),
            std::env::current_dir().unwrap().display()),
        Some("flood") => {
            for _ in 0..64 {
                std::io::stdout().write_all(&[b'o';8192]).unwrap();
                std::io::stderr().write_all(&[b'e';8192]).unwrap();
            }
        },
        Some("tail") => {
            std::io::stdout().write_all(&vec![b'o'; 128 * 1024]).unwrap();
            std::io::stdout().write_all(b"stdout-tail\xf0\x9f\x98\x80").unwrap();
            std::io::stderr().write_all(&vec![b'e'; 128 * 1024]).unwrap();
            std::io::stderr().write_all(b"stderr-tail\xf0\x9f\x98\x80").unwrap();
        },
        #[cfg(unix)]
        Some("escape" | "escape-exit") => {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command.args(["escaped-writer", &args[1], &args[2]])
                .stdin(std::process::Stdio::null());
            if args[0] == "escape-exit" { command.stdout(std::process::Stdio::piped()); }
            let mut child = command.spawn().unwrap();
            if args[0] == "escape-exit" {
                let mut ready = String::new();
                std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut ready).unwrap();
                assert_eq!(ready, "ready\n");
                // Exit normally only after setsid; the descendant retains our stderr.
            } else {
                child.wait().unwrap();
            }
        },
        #[cfg(unix)]
        Some("escaped-writer") => {
            use std::io::Read;
            unsafe extern "C" { fn setsid() -> i32; }
            assert!(unsafe { setsid() } >= 0);
            println!("ready"); std::io::stdout().flush().unwrap();
            let mut control = std::net::TcpStream::connect(&args[1]).unwrap();
            control.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
            control.write_all(&std::process::id().to_be_bytes()).unwrap();
            if args[2] == "flood" {
                std::thread::spawn(|| loop {
                    if std::io::stdout().write_all(&[b'o';4096]).is_err() { break; }
                });
            }
            // Only the owning test has this connection. EOF also cleans up on assertion failure.
            let _ = control.read(&mut [0]);
            std::process::exit(0);
        },
        Some("tree") => {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("sleep").stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
            println!("{}", child.id()); std::io::stdout().flush().unwrap();
            child.wait().unwrap();
        },
        Some("sleep") => { println!("ready"); std::thread::sleep(Duration::from_secs(60)); },
        _ => std::process::exit(91),
    }
}
"#).unwrap();
                crate::test_support::compile_fixture(&source, &executable);
                Self { root, executable }
            }

            pub(crate) fn request(&self, mode: &str, limit: usize) -> Create {
                serde_json::from_value(json!({
                    "sessionId": "one", "command": self.executable.to_str().unwrap(),
                    "args": [mode], "outputByteLimit": limit,
                }))
                .unwrap()
            }
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                if let Err(error) = std::fs::remove_dir_all(&self.root) {
                    if !std::thread::panicking() {
                        panic!("remove owned native fixture: {error}");
                    }
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use test_fixture::Fixture;

        fn start(manager: &Arc<Manager>, fixture: &Fixture, request: &Create) -> Handle {
            let reservation = manager.reserve(&request.session_id).unwrap();
            let id = manager
                .create(
                    reservation,
                    request,
                    &fixture.root,
                    Arc::new(|_, _, _| Ok(())),
                )
                .unwrap();
            Handle {
                session_id: request.session_id.clone(),
                terminal_id: id,
                _meta: None,
            }
        }

        #[test]
        fn terminal_real_create_output_wait_and_release_preserve_split_unicode_and_status() {
            let fixture = Fixture::new();
            let manager = Arc::new(Manager::default());
            manager.activate("one").unwrap();
            let request = fixture.request("output", 1024);
            let began = std::time::Instant::now();
            let handle = start(&manager, &fixture, &request);
            assert!(began.elapsed() < Duration::from_secs(1));
            assert!(manager.output(&handle).unwrap().get("exitStatus").is_none());
            assert_eq!(
                manager.wait(&handle).unwrap(),
                json!({"exitCode": 7, "signal": null})
            );
            let output = manager.output(&handle).unwrap();
            for character in ['A', '😀', 'Z'] {
                assert!(output["output"].as_str().unwrap().contains(character));
            }
            assert!(output["output"].as_str().unwrap().contains("stderr\n"));
            assert_eq!(output["truncated"], false);
            manager.release(&handle).unwrap();
            assert!(manager.output(&handle).is_err());
            assert!(manager.wait(&handle).is_err());
            assert!(manager.kill(&handle).is_err());
            assert!(manager.release(&handle).is_err());
        }

        #[test]
        fn terminal_drains_both_pipes_and_limits_zero_and_unicode_at_character_boundaries() {
            let fixture = Fixture::new();
            let manager = Arc::new(Manager::default());
            manager.activate("one").unwrap();
            for (mode, limit) in [("output", 0), ("output", 5), ("flood", 37)] {
                let handle = start(&manager, &fixture, &fixture.request(mode, limit));
                manager.wait(&handle).unwrap();
                let output = manager.output(&handle).unwrap();
                assert!(output["output"].as_str().unwrap().len() <= limit);
                assert!(!output["output"].as_str().unwrap().contains('\u{fffd}'));
                assert_eq!(output["truncated"], true);
                manager.release(&handle).unwrap();
            }
            let mut decoder = Utf8::default();
            assert_eq!(decoder.decode(&[0xf0, 0x9f], false), "");
            assert_eq!(decoder.decode(&[0x98, 0x80], false), "😀");
            assert_eq!(decoder.decode(&[0xff, b'x', 0xe2], false), "\u{fffd}x");
            assert_eq!(decoder.decode(&[], true), "\u{fffd}");
        }

        #[cfg(unix)]
        #[test]
        fn terminal_cleanup_drains_all_normal_trailing_output_before_success() {
            let fixture = Fixture::new();
            let manager = Arc::new(Manager::default());
            manager.activate("one").unwrap();
            let handle = start(
                &manager,
                &fixture,
                &fixture.request("tail", MAX_OUTPUT_BYTES),
            );
            manager.wait(&handle).unwrap();
            let snapshot = manager.output(&handle).unwrap();
            let output = snapshot["output"].as_str().unwrap();
            assert_eq!(output.len(), 256 * 1024 + 30);
            assert!(output.contains("stdout-tail😀"));
            assert!(output.contains("stderr-tail😀"));
            assert_eq!(snapshot["truncated"], false);
            manager.release(&handle).unwrap();
        }

        #[cfg(unix)]
        #[test]
        fn terminal_cleanup_is_bounded_and_fails_for_setsid_inherited_pipe_writers() {
            use std::net::{Shutdown, TcpListener, TcpStream};
            use std::sync::mpsc;

            struct EscapedWriter(TcpStream);
            impl Drop for EscapedWriter {
                fn drop(&mut self) {
                    // Closing this test-owned control channel terminates only our escaped child.
                    // Never signal a bare PID, which could have been reused after a failure.
                    let _ = self.0.shutdown(Shutdown::Write);
                }
            }

            let fixture = Fixture::new();
            for mode in ["hold", "flood"] {
                for operation in ["wait", "kill", "release", "cancel", "close", "stop"] {
                    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
                    listener.set_nonblocking(true).unwrap();
                    let manager = Arc::new(Manager::default());
                    manager.activate("one").unwrap();
                    let mut request = fixture.request(
                        if operation == "wait" {
                            "escape-exit"
                        } else {
                            "escape"
                        },
                        1024,
                    );
                    request
                        .args
                        .extend([listener.local_addr().unwrap().to_string(), mode.into()]);
                    let handle = start(&manager, &fixture, &request);
                    let terminal = manager.get(&handle).unwrap();
                    let deadline = std::time::Instant::now() + Duration::from_secs(5);
                    let stream = loop {
                        match listener.accept() {
                            Ok((stream, _)) => break stream,
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                                assert!(std::time::Instant::now() < deadline);
                                std::thread::sleep(Duration::from_millis(10));
                            }
                            Err(error) => panic!("accept escaped fixture: {error}"),
                        }
                    };
                    let mut escaped = EscapedWriter(stream);
                    escaped
                        .0
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut pid = [0; 4];
                    escaped.0.read_exact(&mut pid).unwrap();
                    let pid = u32::from_be_bytes(pid);
                    assert!(alive(pid));
                    let worker_manager = Arc::clone(&manager);
                    let (send, receive) = mpsc::channel();
                    let worker = std::thread::spawn(move || {
                        let result = match operation {
                            "wait" => worker_manager.wait(&handle).map(|_| ()),
                            "kill" => worker_manager.kill(&handle),
                            "release" => worker_manager.release(&handle),
                            "cancel" => worker_manager.cancel(Some("one"), false),
                            "close" => worker_manager.cancel(Some("one"), true),
                            "stop" => worker_manager.cancel(None, true),
                            _ => unreachable!(),
                        };
                        let _ = send.send(result);
                    });
                    let result = receive.recv_timeout(Duration::from_secs(5));
                    // Clean up our escaped child before asserting, even on a timeout regression.
                    escaped.0.shutdown(Shutdown::Write).unwrap();
                    let mut byte = [0];
                    assert_eq!(escaped.0.read(&mut byte).unwrap(), 0);
                    let error = result.expect("terminal cleanup hung").unwrap_err();
                    assert!(
                        error
                            .detail
                            .unwrap()
                            .contains("process ownership cannot be guaranteed"),
                        "{operation}/{mode}"
                    );
                    worker.join().unwrap();
                    assert!(terminal.output.lock().unwrap().finished);
                }
            }
        }

        #[test]
        fn terminal_executes_exact_argv_environment_and_directory_without_shell_interpolation() {
            let fixture = Fixture::new();
            let manager = Arc::new(Manager::default());
            manager.activate("one").unwrap();
            let mut request = fixture.request("inspect", 4096);
            request.args.extend([
                "a b".into(),
                "\"quoted\"".into(),
                "$(literal); & |".into(),
                "".into(),
            ]);
            request.env.push(Environment {
                name: "DARBOT_TERMINAL_FIXTURE".into(),
                value: "nonsecret-value".into(),
                _meta: None,
            });
            let handle = start(&manager, &fixture, &request);
            assert_eq!(manager.wait(&handle).unwrap()["exitCode"], 0);
            let output = manager.output(&handle).unwrap();
            let output = output["output"].as_str().unwrap();
            assert!(output.contains(r#"["a b", "\"quoted\"", "$(literal); & |", ""]"#));
            assert!(output.contains("nonsecret-value"));
            assert!(output.contains(fixture.root.file_name().unwrap().to_str().unwrap()));
            manager.release(&handle).unwrap();
        }

        #[test]
        fn terminal_session_runtime_isolation_cancel_generations_and_limits_are_enforced() {
            let fixture = Fixture::new();
            let manager = Arc::new(Manager::default());
            manager.activate("one").unwrap();
            manager.activate("two").unwrap();
            let handle = start(&manager, &fixture, &fixture.request("sleep", 1024));
            let wrong_session = Handle {
                session_id: "two".into(),
                terminal_id: handle.terminal_id.clone(),
                _meta: None,
            };
            assert!(manager.output(&wrong_session).is_err());
            assert!(manager.wait(&wrong_session).is_err());
            assert!(manager.kill(&wrong_session).is_err());
            assert!(manager.release(&wrong_session).is_err());
            let other = Manager::default();
            assert!(other.output(&handle).is_err());
            let reservation = manager.reserve("one").unwrap();
            manager.cancel(Some("one"), false).unwrap();
            assert!(!reservation.is_current());
            assert!(manager
                .create(
                    reservation,
                    &fixture.request("sleep", 1024),
                    &fixture.root,
                    Arc::new(|_, _, _| Ok(()))
                )
                .is_err());
            let status = manager.wait(&handle).unwrap();
            #[cfg(windows)]
            assert_eq!(status["exitCode"], 1);
            #[cfg(unix)]
            assert_eq!(status["signal"], "SIGKILL");
            assert!(manager.output(&handle).unwrap().get("exitStatus").is_some());
            manager.release(&handle).unwrap();
            let reservations: Vec<_> = (0..MAX_TERMINALS)
                .map(|_| manager.reserve("one").unwrap())
                .collect();
            assert!(manager.reserve("one").is_err());
            drop(reservations);
            let reservation = manager.reserve("one").unwrap();
            manager.cancel(Some("one"), true).unwrap();
            manager.activate("one").unwrap();
            assert!(!reservation.is_current());
            drop(reservation);
            manager.cancel(None, true).unwrap();
            assert!(manager.reserve("one").is_err());
        }

        #[test]
        fn cancellation_during_spawn_registration_waits_for_owned_child_cleanup() {
            use std::sync::atomic::{AtomicBool, Ordering};
            use std::sync::mpsc;
            let fixture = Fixture::new();
            let manager = Arc::new(Manager::default());
            manager.activate("one").unwrap();
            let generation = manager.generation("one").unwrap();
            let reservation = manager.reserve("one").unwrap();
            let request = fixture.request("sleep", 1024);
            let directory = fixture.root.clone();
            let (entered, started) = mpsc::channel();
            let (resume, resumed) = mpsc::channel();
            let resumed = Mutex::new(resumed);
            let first = AtomicBool::new(true);
            let publishing: Publish = Arc::new(move |_, _, _| {
                if first.swap(false, Ordering::SeqCst) {
                    entered.send(()).unwrap();
                    resumed
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap();
                }
                Ok(())
            });
            let creating = Arc::clone(&manager);
            let creator = std::thread::spawn(move || {
                creating.create(reservation, &request, &directory, publishing)
            });
            started.recv_timeout(Duration::from_secs(10)).unwrap();
            let cancelling = Arc::clone(&manager);
            let canceller = std::thread::spawn(move || cancelling.cancel(Some("one"), false));
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while manager.generation("one") == Some(generation) {
                assert!(std::time::Instant::now() < deadline);
                std::thread::yield_now();
            }
            resume.send(()).unwrap();
            assert!(creator.join().unwrap().is_err());
            canceller.join().unwrap().unwrap();
            let state = manager.state.lock().unwrap();
            assert!(state.terminals.is_empty());
            assert!(state.disposing.is_empty());
            assert_eq!(state.spawning, 0);
        }

        #[test]
        fn output_delivery_failure_reaps_command_and_invalidates_handle() {
            let fixture = Fixture::new();
            let manager = Arc::new(Manager::default());
            manager.activate("one").unwrap();
            let result = manager.create(
                manager.reserve("one").unwrap(),
                &fixture.request("sleep", 1024),
                &fixture.root,
                Arc::new(|_, _, _| Err("fixture output listener unavailable".into())),
            );
            assert!(result.is_err());
            assert!(manager.state.lock().unwrap().terminals.is_empty());
            assert!(manager.state.lock().unwrap().disposing.is_empty());
        }

        #[test]
        fn terminal_rejects_bad_requests_and_cwd_without_granting_home_or_symlink_escapes() {
            let fixture = Fixture::new();
            let mut request = fixture.request("sleep", MAX_OUTPUT_BYTES + 1);
            assert!(request.validate().is_err());
            request.output_byte_limit = Some(0);
            assert!(request.validate().is_ok());
            request.env.push(Environment {
                name: "INVALID=NAME".into(),
                value: String::new(),
                _meta: None,
            });
            assert!(request.validate().is_err());
            request.env.clear();
            let private = Fixture::new();
            let home = private.root.clone();
            let root = fixture.root.to_str().unwrap();
            assert!(request.directory(root, &home).is_ok());
            for cwd in [
                "relative",
                fixture.root.parent().unwrap().to_str().unwrap(),
                home.to_str().unwrap(),
            ] {
                request.cwd = Some(cwd.into());
                assert!(request.directory(root, &home).is_err());
            }
            request.cwd = Some(fixture.root.join(".ssh").to_string_lossy().into());
            std::fs::create_dir(fixture.root.join(".ssh")).unwrap();
            assert!(request.directory(root, &home).is_err());
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(
                    fixture.root.parent().unwrap(),
                    fixture.root.join("escape"),
                )
                .unwrap();
                request.cwd = Some(fixture.root.join("escape").to_string_lossy().into());
                assert!(request.directory(root, &home).is_err());
            }
            for value in [
                json!({"sessionId":"one","command":"x","extra":true}),
                json!({"sessionId":"one","command":"x","outputByteLimit":-1}),
                json!({"sessionId":"one","command":"x","outputByteLimit":1.5}),
                json!({"sessionId":"one","command":"x","env":[{"name":"x","value":"y","extra":true}]}),
            ] {
                assert!(serde_json::from_value::<Create>(value).is_err());
            }
        }

        #[cfg(windows)]
        fn alive(pid: u32) -> bool {
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn OpenProcess(access: u32, inherit: i32, pid: u32) -> windows::Handle;
                fn WaitForSingleObject(handle: windows::Handle, milliseconds: u32) -> u32;
            }
            let handle = unsafe { OpenProcess(0x0010_0000, 0, pid) };
            if handle.is_null() {
                return false;
            }
            let pending = unsafe { WaitForSingleObject(handle, 0) } == 258;
            unsafe { windows::CloseHandle(handle) };
            pending
        }

        #[cfg(unix)]
        fn alive(pid: u32) -> bool {
            unsafe { libc::kill(pid as i32, 0) == 0 }
        }

        #[test]
        fn terminal_kill_release_close_and_runtime_stop_terminate_owned_process_trees() {
            let fixture = Fixture::new();
            for operation in ["kill", "release", "cancel", "close", "stop", "drop"] {
                let manager = Arc::new(Manager::default());
                manager.activate("one").unwrap();
                let handle = start(&manager, &fixture, &fixture.request("tree", 1024));
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                let pid: u32 = loop {
                    if let Ok(pid) = manager.output(&handle).unwrap()["output"]
                        .as_str()
                        .unwrap()
                        .trim()
                        .parse()
                    {
                        break pid;
                    }
                    assert!(std::time::Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                };
                assert!(alive(pid));
                match operation {
                    "kill" => {
                        manager.kill(&handle).unwrap();
                        manager.release(&handle).unwrap();
                    }
                    "release" => manager.release(&handle).unwrap(),
                    "cancel" => manager.cancel(Some("one"), false).unwrap(),
                    "close" => manager.cancel(Some("one"), true).unwrap(),
                    "stop" => manager.cancel(None, true).unwrap(),
                    "drop" => {}
                    _ => unreachable!(),
                }
                drop(manager);
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while alive(pid) {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "owned child survived {operation}"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn consent_validates_options_and_consumes_only_the_answered_request() {
        let consent = Arc::new(ClientConsent::default());
        let (opened, requests) = mpsc::channel();
        let mut workers = Vec::new();
        for session in ["one", "two"] {
            let consent = Arc::clone(&consent);
            let opened = opened.clone();
            workers.push(std::thread::spawn(move || {
                consent.ask_with(
                    "runtime",
                    session,
                    CopilotPermissionOrigin::Conversation,
                    json!({"title": "Read the selected file"}),
                    Duration::from_secs(5),
                    |request| {
                        opened.send(request.clone()).unwrap();
                        Ok(())
                    },
                    |_| {},
                )
            }));
        }
        let first = requests.recv_timeout(Duration::from_secs(5)).unwrap();
        let second = requests.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(consent
            .respond(&first.request_id, Some("allow-always"))
            .is_err());
        assert_eq!(consent.pending.lock().unwrap().len(), 2);
        consent
            .respond(&first.request_id, Some("allow-once"))
            .unwrap();
        assert_eq!(consent.pending.lock().unwrap().len(), 1);
        assert!(consent
            .respond(&first.request_id, Some("allow-once"))
            .is_err());
        consent
            .respond(&second.request_id, Some("reject-once"))
            .unwrap();
        let decisions: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap().unwrap())
            .collect();
        assert!(decisions.contains(&ConsentDecision::Allow));
        assert!(decisions.contains(&ConsentDecision::Reject));
    }

    #[test]
    fn interrupted_requests_are_cancelled_without_affecting_another_owner() {
        let consent = Arc::new(ClientConsent::default());
        let (opened, requests) = mpsc::channel();
        let owned = Arc::clone(&consent);
        let worker = std::thread::spawn(move || {
            owned.ask_with(
                "runtime-one",
                "session",
                CopilotPermissionOrigin::Conversation,
                json!({"title": "Write the selected file"}),
                Duration::from_secs(5),
                |request| {
                    opened.send(request.clone()).unwrap();
                    Ok(())
                },
                |_| {},
            )
        });
        let request = requests.recv_timeout(Duration::from_secs(5)).unwrap();
        consent.cancel("runtime-two", None);
        assert_eq!(consent.pending.lock().unwrap().len(), 1);
        consent.cancel("runtime-one", Some("session"));
        assert_eq!(worker.join().unwrap().unwrap(), ConsentDecision::Cancel);
        assert!(consent
            .respond(&request.request_id, Some("allow-once"))
            .is_err());
    }

    #[test]
    fn expiry_and_display_failure_remove_pending_authority() {
        let consent = ClientConsent::default();
        assert!(consent
            .ask_with(
                "runtime",
                "session",
                CopilotPermissionOrigin::Conversation,
                json!({"title": "Read"}),
                Duration::ZERO,
                |_| Ok(()),
                |_| {},
            )
            .is_err());
        assert!(consent
            .ask_with(
                "runtime",
                "session",
                CopilotPermissionOrigin::Conversation,
                json!({"title": "Read"}),
                Duration::from_secs(5),
                |_| Err(Problem::plain("Window is unavailable.")),
                |_| {},
            )
            .is_err());
        assert!(consent.pending.lock().unwrap().is_empty());
    }
}
