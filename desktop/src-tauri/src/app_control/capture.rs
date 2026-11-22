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
pub(super) fn app_window(_window: &tauri::WebviewWindow) -> Result<serde_json::Value> {
    Err(error(
        "unavailable",
        "App-only native capture is currently supported on Windows",
    ))
}

#[cfg(windows)]
pub(super) fn app_window(window: &tauri::WebviewWindow) -> Result<serde_json::Value> {
    use base64::Engine;
    use windows::Win32::{
        Foundation::{HWND, RECT},
        Graphics::Gdi::*,
        UI::WindowsAndMessaging::{GetWindowRect, IsIconic, IsWindowVisible},
    };
    let handle = window
        .hwnd()
        .map_err(|_| error("unavailable", "App window handle unavailable"))?;
    let hwnd = HWND(handle.0);
    // PrintWindow renders this process's known HWND into an off-screen bitmap. It never samples
    // the desktop DC, other windows, or the user's monitor. No image is written to disk.
    unsafe {
        if IsIconic(hwnd).as_bool() || !IsWindowVisible(hwnd).as_bool() {
            return Err(error(
                "unavailable",
                "Capture requires a visible, non-minimized app window",
            ));
        }
        let mut rect = RECT::default();
        GetWindowRect(hwnd, &mut rect)
            .map_err(|_| error("unavailable", "App window bounds unavailable"))?;
        let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
        if width <= 0 || height <= 0 || width > 2048 || height > 2048 {
            return Err(error(
                "unavailable",
                "Capture is limited to 2048 × 2048 app pixels",
            ));
        }
        let dc = GetDC(Some(hwnd));
        if dc.is_invalid() {
            return Err(error("unavailable", "App render context unavailable"));
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
            return Err(error("unavailable", "App capture allocation failed"));
        }
        let previous = SelectObject(memory, bitmap.into());
        let rendered = windows::Win32::Storage::Xps::PrintWindow(
            hwnd,
            memory,
            windows::Win32::Storage::Xps::PRINT_WINDOW_FLAGS(2),
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
        if rows != height || pixels.chunks_exact(4).all(|p| p[0..3] == pixels[0..3]) {
            return Err(error(
                "unavailable",
                "App did not provide a current rendered image",
            ));
        }
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.swap(0, 2);
            pixel[3] = 255;
        }
        let mut encoded = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut encoded, width as u32, height as u32);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder
                .write_header()
                .map_err(|_| error("unavailable", "PNG encoder failed"))?;
            writer
                .write_image_data(&pixels)
                .map_err(|_| error("unavailable", "PNG encoding failed"))?;
        }
        if encoded.len() > 2 * 1024 * 1024 {
            return Err(error("unavailable", "App image exceeds 2 MiB limit"));
        }
        Ok(
            serde_json::json!({"mimeType":"image/png","width":width,"height":height,
            "capturedAt":super::now_ms(),"data":base64::engine::general_purpose::STANDARD.encode(encoded)}),
        )
    }
}

#[cfg(test)]
#[path = "capture_tests.rs"]
mod tests;
