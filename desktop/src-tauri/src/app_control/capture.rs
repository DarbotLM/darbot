use super::{error, Result};
use std::{io::Write, path::Path};

fn not_redirected(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| error("unavailable", "Private directory unavailable"))?;
    let redirected = metadata.file_type().is_symlink();
    #[cfg(windows)]
    let redirected = {
        use std::os::windows::fs::MetadataExt;
        redirected || metadata.file_attributes() & 0x400 != 0
    };
    if redirected || !metadata.is_dir() {
        return Err(error(
            "forbidden",
            "App-control directory must not be a link or junction",
        ));
    }
    Ok(())
}

pub(super) fn private_directory(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .map_err(|_| error("unavailable", "Cannot create private app-control directory"))?;
    not_redirected(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| error("unavailable", "Cannot restrict app-control directory"))?;
    }
    #[cfg(windows)]
    unsafe {
        use std::os::windows::ffi::OsStrExt;
        use windows::{
            core::PCWSTR,
            Win32::{
                Foundation::{LocalFree, HLOCAL},
                Security::{
                    Authorization::{
                        ConvertStringSecurityDescriptorToSecurityDescriptorW,
                        SetNamedSecurityInfoW, SE_FILE_OBJECT,
                    },
                    GetSecurityDescriptorDacl, ACL, DACL_SECURITY_INFORMATION,
                    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
                },
            },
        };
        // OWNER RIGHTS is the object's owner, not the Users/Administrators groups. Protect the
        // DACL before creating any credential bytes; children inherit only this owner ACE.
        let sddl: Vec<u16> = "D:P(A;OICI;FA;;;OW)\0".encode_utf16().collect();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            1,
            &mut descriptor,
            None,
        )
        .map_err(|_| error("unavailable", "Cannot create owner-only ACL"))?;
        let result = (|| {
            let mut present = windows::core::BOOL(0);
            let mut defaulted = windows::core::BOOL(0);
            let mut acl: *mut ACL = std::ptr::null_mut();
            GetSecurityDescriptorDacl(descriptor, &mut present, &mut acl, &mut defaulted)
                .map_err(|_| error("unavailable", "Cannot read owner-only ACL"))?;
            let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            SetNamedSecurityInfoW(
                PCWSTR(path.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(acl),
                None,
            )
            .ok()
            .map_err(|_| error("unavailable", "Cannot restrict app-control directory"))
        })();
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        result?;
    }
    #[cfg(not(any(windows, unix)))]
    return Err(error(
        "unavailable",
        "Owner-only credential files are unsupported on this platform",
    ));
    Ok(())
}

pub(super) fn write_discovery(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| error("unavailable", "Missing private directory"))?;
    not_redirected(parent)?;
    let staging = parent.join(format!("{}.json", super::random_id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> std::io::Result<()> {
        let mut file = options.open(&staging)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        std::fs::rename(&staging, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staging);
    }
    result.map_err(|_| error("unavailable", "Cannot publish owner-only connection file"))
}

#[cfg(not(windows))]
pub(super) fn app_window(
    _worker: &crate::native_capture::OwnedCapture,
    _window: &tauri::WebviewWindow,
) -> Result<serde_json::Value> {
    Err(error(
        "unavailable",
        "App-only native capture is currently supported on Windows",
    ))
}

#[cfg(windows)]
pub(super) fn app_window(
    worker: &crate::native_capture::OwnedCapture,
    window: &tauri::WebviewWindow,
) -> Result<serde_json::Value> {
    let handle = window
        .hwnd()
        .map_err(|_| error("unavailable", "App window handle unavailable"))?;
    let identity = crate::native_capture::WindowIdentity::bind(
        handle.0 as isize,
        crate::native_capture::window_title(handle.0 as isize),
        crate::native_capture::CaptureKind::AppOnly,
    )
    .map_err(|_| {
        error(
            "unavailable",
            "Cannot bind Darbot's current window instance",
        )
    })?;
    app_target(worker, &identity.0)
}

#[cfg(windows)]
fn app_target(
    worker: &crate::native_capture::OwnedCapture,
    target: &crate::native_capture::Target,
) -> Result<serde_json::Value> {
    use base64::Engine;
    if target.kind != crate::native_capture::CaptureKind::AppOnly
        || target.process != std::process::id()
    {
        return Err(error(
            "forbidden",
            "Capture requires Darbot's app-only identity",
        ));
    }
    let encoded =
        crate::native_capture::capture(worker, target, crate::native_capture::CAPTURE_TIMEOUT)
            .map_err(|problem| error("unavailable", &problem.said))?;
    let reader = png::Decoder::new(std::io::Cursor::new(&encoded))
        .read_info()
        .map_err(|_| error("unavailable", "Capture worker returned an invalid PNG"))?;
    let (width, height) = (reader.info().width, reader.info().height);
    if width == 0 || height == 0 || width > 2048 || height > 2048 {
        return Err(error("unavailable", "App capture exceeds its pixel limit"));
    }
    Ok(
        serde_json::json!({"mimeType":"image/png","width":width,"height":height,
        "capturedAt":super::now_ms(),"data":base64::engine::general_purpose::STANDARD.encode(encoded)}),
    )
}

#[cfg(test)]
#[path = "capture_tests.rs"]
mod tests;
