//! An owned, disposable renderer. Only the worker may enter blocking Windows rendering.
use crate::problem::Problem;
use std::{
    io::{Read, Write},
    process::{Child, Command, Stdio},
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant},
};

const WORKER: &str = "--darbot-native-capture";
const OUTPUT_LIMIT: usize = 2 * 1024 * 1024;
const REQUEST_LIMIT: usize = 8192;
pub(crate) const CAPTURE_TIMEOUT: Duration = Duration::from_secs(2);

fn fail(message: impl AsRef<str>) -> Problem {
    Problem::plain(message.as_ref())
}

#[derive(Default)]
struct Process {
    child: Option<Child>,
    cancelled: bool,
}

/// Share only this worker's process handle, never a PID-based termination capability.
#[derive(Default)]
pub(crate) struct OwnedCapture(Mutex<Process>);

impl OwnedCapture {
    pub(crate) fn cancel(&self) -> Result<(), Problem> {
        let mut process = self.0.lock().unwrap();
        process.cancelled = true;
        Self::reap(&mut process)
    }

    fn reap(process: &mut Process) -> Result<(), Problem> {
        if let Some(child) = &mut process.child {
            if child
                .try_wait()
                .map_err(|e| fail(format!("Cannot inspect Darbot capture worker: {e}")))?
                .is_none()
            {
                child
                    .kill()
                    .map_err(|e| fail(format!("Cannot stop Darbot capture worker: {e}")))?;
            }
            child
                .wait()
                .map_err(|e| fail(format!("Cannot reap Darbot capture worker: {e}")))?;
            process.child = None;
        }
        Ok(())
    }

    fn run(
        &self,
        mut command: Command,
        request: &[u8],
        budget: Duration,
        valid: impl Fn() -> Result<(), Problem>,
    ) -> Result<Vec<u8>, Problem> {
        let started = Instant::now();
        valid()?;
        let (input, output) = {
            let mut process = self.0.lock().unwrap();
            if process.cancelled || process.child.is_some() {
                return Err(fail("Capture was cancelled or is already running."));
            }
            let mut child = command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| fail(format!("Cannot start Darbot capture worker: {e}")))?;
            let pipes = (child.stdin.take(), child.stdout.take());
            process.child = Some(child);
            pipes
        };
        let result = (|| {
            let mut input = input.ok_or_else(|| fail("Capture worker input is unavailable."))?;
            let output = output.ok_or_else(|| fail("Capture worker output is unavailable."))?;
            // Drain concurrently: a valid PNG can exceed the anonymous pipe's capacity.
            let (tx, rx) = mpsc::sync_channel(1);
            let reader = std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let result = output
                    .take((OUTPUT_LIMIT + 1) as u64)
                    .read_to_end(&mut bytes)
                    .map(|_| bytes);
                let _ = tx.send(result);
            });
            let request = request.to_vec();
            let (written_tx, written_rx) = mpsc::sync_channel(1);
            let writer = std::thread::spawn(move || {
                let result = input.write_all(&request);
                drop(input);
                let _ = written_tx.send(result);
            });
            let outcome = (|| loop {
                valid()?;
                if let Ok(Err(error)) = written_rx.try_recv() {
                    return Err(fail(format!("Cannot request Darbot capture: {error}")));
                }
                if started.elapsed() >= budget {
                    return Err(fail("Darbot capture exceeded its two-second budget."));
                }
                let status = {
                    let mut process = self.0.lock().unwrap();
                    if process.cancelled {
                        return Err(fail("Darbot capture was cancelled."));
                    }
                    process
                        .child
                        .as_mut()
                        .ok_or_else(|| fail("Darbot capture was stopped."))?
                        .try_wait()
                        .map_err(|e| fail(format!("Cannot inspect Darbot capture: {e}")))?
                };
                if let Some(status) = status {
                    if !status.success() {
                        return Err(fail(
                                "The selected window could not be rendered; no desktop fallback was used.",
                            ));
                    }
                    let remaining = budget.saturating_sub(started.elapsed());
                    let bytes = rx
                        .recv_timeout(remaining)
                        .map_err(|_| fail("Capture worker output did not finish."))?
                        .map_err(|e| fail(format!("Cannot read capture worker output: {e}")))?;
                    if bytes.len() > OUTPUT_LIMIT {
                        return Err(fail("Capture PNG exceeds 2 MiB."));
                    }
                    valid()?;
                    return Ok(bytes);
                }
                std::thread::sleep(Duration::from_millis(5));
            })();
            let cleanup = {
                let mut process = self.0.lock().unwrap();
                Self::reap(&mut process)
            };
            // Killing the exact child closes its pipe and releases the reader even on a hang.
            cleanup?;
            reader
                .join()
                .map_err(|_| fail("Capture output reader failed."))?;
            writer
                .join()
                .map_err(|_| fail("Capture input writer failed."))?;
            outcome
        })();
        let cleanup = Self::reap(&mut self.0.lock().unwrap());
        cleanup?;
        result
    }
}

impl Drop for OwnedCapture {
    fn drop(&mut self) {
        if let Err(error) = self.cancel() {
            eprintln!("Darbot capture cleanup failed: {error:?}");
        }
    }
}

pub(crate) struct CancelOnDrop(pub Arc<OwnedCapture>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Err(error) = self.0.cancel() {
            eprintln!("Darbot capture cancellation failed: {error:?}");
        }
    }
}

#[cfg(windows)]
#[derive(Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(crate) enum CaptureKind {
    SelectedWindow,
    AppOnly,
}

#[cfg(windows)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Target {
    pub handle: isize,
    pub process: u32,
    pub thread: u32,
    pub created: u64,
    pub property: String,
    pub token: usize,
    pub title: String,
    pub kind: CaptureKind,
}

#[cfg(windows)]
impl Target {
    pub(crate) fn check(&self) -> Result<windows::Win32::Foundation::HWND, Problem> {
        use windows::{
            core::PCWSTR,
            Win32::{Foundation::HWND, UI::WindowsAndMessaging::*},
        };
        if self.token != 1
            || self.handle == 0
            || self.created == 0
            || !self
                .property
                .strip_prefix("DarbotCapture-")
                .is_some_and(|nonce| {
                    nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
        {
            return Err(fail("Invalid Darbot capture instance binding."));
        }
        let hwnd = HWND(self.handle as *mut std::ffi::c_void);
        let property: Vec<u16> = self.property.encode_utf16().chain(Some(0)).collect();
        unsafe {
            let mut process = 0;
            let thread = GetWindowThreadProcessId(hwnd, Some(&mut process));
            if !IsWindow(Some(hwnd)).as_bool()
                || !IsWindowVisible(hwnd).as_bool()
                || IsIconic(hwnd).as_bool()
                || process != self.process
                || thread != self.thread
                || process_created(process)? != self.created
                || GetPropW(hwnd, PCWSTR(property.as_ptr())).0 as usize != self.token
                || window_title(self.handle) != self.title
            {
                return Err(fail(
                    "The selected window instance closed or changed. Select it again.",
                ));
            }
        }
        Ok(hwnd)
    }
}

#[cfg(windows)]
pub(crate) fn window_title(handle: isize) -> String {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn InternalGetWindowText(
            window: *mut std::ffi::c_void,
            text: *mut u16,
            capacity: i32,
        ) -> i32;
    }
    // GetWindowTextW sends WM_GETTEXT for same-process windows and can itself hang.
    let mut title = [0u16; 512];
    let length =
        unsafe { InternalGetWindowText(handle as *mut _, title.as_mut_ptr(), title.len() as i32) };
    String::from_utf16_lossy(&title[..length.max(0) as usize])
}

#[cfg(windows)]
pub(crate) struct WindowIdentity(pub Target);

#[cfg(windows)]
impl WindowIdentity {
    pub(crate) fn bind(
        handle: isize,
        title: String,
        kind: CaptureKind,
    ) -> Result<Arc<Self>, Problem> {
        use windows::{
            core::PCWSTR,
            Win32::{
                Foundation::{HANDLE, HWND},
                UI::WindowsAndMessaging::*,
            },
        };
        let hwnd = HWND(handle as *mut std::ffi::c_void);
        let mut process = 0;
        let thread = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process)) };
        let created = process_created(process)?;
        if kind == CaptureKind::AppOnly && process != std::process::id() {
            return Err(fail("App-only capture requires Darbot's own window."));
        }
        let target = Target {
            handle,
            process,
            thread,
            created,
            property: format!("DarbotCapture-{:032x}", rand::random::<u128>()),
            token: 1,
            title,
            kind,
        };
        let property: Vec<u16> = target.property.encode_utf16().chain(Some(0)).collect();
        unsafe {
            SetPropW(
                hwnd,
                PCWSTR(property.as_ptr()),
                Some(HANDLE(std::ptr::without_provenance_mut(target.token))),
            )
            .map_err(|_| fail("Windows denied binding this capture source."))?;
        }
        let identity = Arc::new(Self(target));
        identity.0.check()?;
        Ok(identity)
    }
}

#[cfg(windows)]
impl Drop for WindowIdentity {
    fn drop(&mut self) {
        use windows::{
            core::PCWSTR,
            Win32::{Foundation::HWND, UI::WindowsAndMessaging::*},
        };
        let property: Vec<u16> = self.0.property.encode_utf16().chain(Some(0)).collect();
        unsafe {
            let hwnd = HWND(self.0.handle as *mut std::ffi::c_void);
            if GetPropW(hwnd, PCWSTR(property.as_ptr())).0 as usize == self.0.token {
                let _ = RemovePropW(hwnd, PCWSTR(property.as_ptr()));
            }
        }
    }
}

#[cfg(windows)]
fn process_created(process: u32) -> Result<u64, Problem> {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, process: u32) -> *mut c_void;
        fn GetProcessTimes(
            process: *mut c_void,
            creation: *mut u64,
            exit: *mut u64,
            kernel: *mut u64,
            user: *mut u64,
        ) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }
    unsafe {
        let handle = OpenProcess(0x1000, 0, process); // PROCESS_QUERY_LIMITED_INFORMATION
        if handle.is_null() {
            return Err(fail("Cannot verify the selected process instance."));
        }
        let (mut creation, mut exit, mut kernel, mut user) = (0, 0, 0, 0);
        let ok = GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user);
        CloseHandle(handle);
        if ok == 0 || exit != 0 {
            return Err(fail("The selected process closed or cannot be verified."));
        }
        Ok(creation)
    }
}

#[cfg(windows)]
pub(crate) fn capture(
    worker: &OwnedCapture,
    target: &Target,
    budget: Duration,
) -> Result<Vec<u8>, Problem> {
    let request = serde_json::to_vec(target)
        .map_err(|_| fail("Cannot encode the approved capture target."))?;
    if request.len() > REQUEST_LIMIT {
        return Err(fail("Capture target exceeds the worker request limit."));
    }
    let executable =
        std::env::current_exe().map_err(|_| fail("Cannot locate the Darbot capture worker."))?;
    let mut command = crate::quiet::command(executable);
    command.arg(WORKER);
    worker.run(command, &request, budget.min(CAPTURE_TIMEOUT), || {
        target.check().map(|_| ())
    })
}

/// Must run before Tauri/single-instance initialization. No renderer or ACP capture entry point.
pub fn run_worker() -> Option<i32> {
    if std::env::args_os().nth(1).as_deref() != Some(std::ffi::OsStr::new(WORKER)) {
        return None;
    }
    #[cfg(windows)]
    let result = (|| {
        let mut request = Vec::new();
        std::io::stdin()
            .take((REQUEST_LIMIT + 1) as u64)
            .read_to_end(&mut request)
            .map_err(|_| fail("Cannot read capture request."))?;
        if request.len() > REQUEST_LIMIT {
            return Err(fail("Capture request exceeds limit."));
        }
        let target: Target =
            serde_json::from_slice(&request).map_err(|_| fail("Invalid capture request."))?;
        let bytes = crate::chat_content::render_worker(&target)?;
        std::io::stdout()
            .write_all(&bytes)
            .map_err(|_| fail("Cannot return capture PNG."))
    })();
    #[cfg(not(windows))]
    let result: Result<(), Problem> = Err(fail("Native capture requires Windows."));
    if let Err(error) = &result {
        eprintln!("Darbot capture worker failed: {error:?}");
    }
    Some(if result.is_ok() { 0 } else { 1 })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn fixture(mode: &str) -> Command {
        let mut command = crate::quiet::command(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "native_capture::tests::owned_capture_fixture",
                "--nocapture",
            ])
            .env("DARBOT_OWNED_CAPTURE_FIXTURE", mode);
        command
    }

    // The test executable is also the worker fixture; no helper executable or screen sampling.
    #[test]
    fn owned_capture_fixture() {
        let Ok(mode) = std::env::var("DARBOT_OWNED_CAPTURE_FIXTURE") else {
            return;
        };
        if mode == "hung-render" {
            #[cfg(windows)]
            crate::chat_content::tests::native_capture_encodes_only_an_owned_fixture_window_and_rejects_closed_sources();
            #[cfg(not(windows))]
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
            panic!("The owned WM_PRINT fixture must hang");
        }
        if mode == "hang" {
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }
        let mut request = Vec::new();
        std::io::stdin().read_to_end(&mut request).unwrap();
        assert_eq!(request, b"owned request");
        println!("Darbot owned fixture finished");
    }

    fn assert_reaped(worker: &OwnedCapture) {
        assert!(worker.0.lock().unwrap().child.is_none());
    }

    pub(crate) fn run_fixture(
        worker: &OwnedCapture,
        mode: &str,
        started: impl Fn(),
    ) -> Result<Vec<u8>, Problem> {
        worker.run(fixture(mode), b"owned request", CAPTURE_TIMEOUT, || {
            if worker.0.lock().unwrap().child.is_some() {
                started();
            }
            Ok(())
        })
    }

    pub(crate) fn is_reaped(worker: &OwnedCapture) -> bool {
        worker.0.lock().unwrap().child.is_none()
    }

    #[test]
    fn hung_owned_render_is_reaped_and_repeated_capture_succeeds() {
        for _ in 0..2 {
            let worker = OwnedCapture::default();
            let started = Instant::now();
            let error = worker
                .run(
                    fixture("hung-render"),
                    b"owned request",
                    CAPTURE_TIMEOUT,
                    || Ok(()),
                )
                .unwrap_err();
            assert!(error.said.contains("budget"), "{error:?}");
            assert!(started.elapsed() < CAPTURE_TIMEOUT + Duration::from_secs(1));
            assert_reaped(&worker);
            let bytes = worker
                .run(fixture("finish"), b"owned request", CAPTURE_TIMEOUT, || {
                    Ok(())
                })
                .unwrap();
            assert!(String::from_utf8(bytes)
                .unwrap()
                .contains("Darbot owned fixture finished"));
            assert_reaped(&worker);
        }
    }

    #[test]
    fn cancellation_reaps_only_the_owned_worker() {
        let worker = Arc::new(OwnedCapture::default());
        let running = Arc::clone(&worker);
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || {
            running.run(fixture("hang"), b"owned request", CAPTURE_TIMEOUT, || {
                if running.0.lock().unwrap().child.is_some() {
                    let _ = started_tx.try_send(());
                }
                Ok(())
            })
        });
        started_rx.recv_timeout(CAPTURE_TIMEOUT).unwrap();
        worker.cancel().unwrap();
        assert!(thread.join().unwrap().is_err());
        assert_reaped(&worker);
    }

    #[test]
    fn identity_change_reaps_worker_and_prevents_another_launch() {
        let worker = OwnedCapture::default();
        let valid = AtomicBool::new(true);
        assert!(worker
            .run(fixture("hang"), b"owned request", CAPTURE_TIMEOUT, || {
                if worker.0.lock().unwrap().child.is_some() {
                    valid.store(false, Ordering::SeqCst);
                }
                if valid.load(Ordering::SeqCst) {
                    Ok(())
                } else {
                    Err(fail("Owned source identity changed"))
                }
            })
            .is_err());
        assert_reaped(&worker);
        assert!(worker
            .run(fixture("finish"), b"owned request", CAPTURE_TIMEOUT, || {
                Err(fail("Owned source is closed"))
            })
            .is_err());
        assert_reaped(&worker);
    }

    #[test]
    fn dropped_call_and_revoked_store_reap_the_worker() {
        for revoke_store in [false, true] {
            let worker = Arc::new(OwnedCapture::default());
            let running = Arc::clone(&worker);
            let (started_tx, started_rx) = mpsc::sync_channel(1);
            let thread = std::thread::spawn(move || {
                running.run(fixture("hang"), b"owned request", CAPTURE_TIMEOUT, || {
                    if running.0.lock().unwrap().child.is_some() {
                        let _ = started_tx.try_send(());
                    }
                    Ok(())
                })
            });
            started_rx.recv_timeout(CAPTURE_TIMEOUT).unwrap();
            if revoke_store {
                crate::chat_content::tests::revoke_owned_capture_fixture(Arc::clone(&worker));
            } else {
                drop(CancelOnDrop(Arc::clone(&worker)));
            }
            assert!(thread.join().unwrap().is_err());
            assert_reaped(&worker);
        }
    }
}
