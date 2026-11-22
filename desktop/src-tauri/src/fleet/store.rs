use super::*;
use std::io::Write;

const MAX_STORE: u64 = 16 * 1024 * 1024;

pub(super) struct Store {
    path: PathBuf,
    _lock: std::fs::File,
}

impl Store {
    pub fn open(root: &Path) -> Result<(Self, Record)> {
        std::fs::create_dir_all(root).map_err(io)?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("fleet.lock"))
            .map_err(io)?;
        lock.try_lock()
            .map_err(|e| fail("store_busy", format!("Fleet store already in use: {e}")))?;
        let store = Self {
            path: root.join("fleet-v1.dpapi"),
            _lock: lock,
        };
        let record = match std::fs::metadata(&store.path) {
            Ok(meta) => {
                if meta.len() > MAX_STORE {
                    return Err(fail("corrupt_store", "Fleet store exceeds its limit"));
                }
                let bytes = std::fs::read(&store.path).map_err(io)?;
                let decoded = unprotect(&bytes)?;
                let mut record: Record = serde_json::from_slice(&decoded)
                    .map_err(|e| fail("corrupt_store", e.to_string()))?;
                record.validate()?;
                // A persisted running lease is not evidence that its side effects did not happen.
                let mut recovered = vec![];
                for task in record.tasks.values_mut() {
                    if task.executor == record.node_id
                        && (task.status == Status::Running
                            || (task.status == Status::InputRequired && task.session_id.is_some()))
                    {
                        task.status = Status::ReconciliationRequired;
                        task.detail =
                            "Executor restarted during execution; do not automatically retry."
                                .into();
                        task.revision += 1;
                        recovered.push(task.clone());
                    }
                }
                for task in recovered {
                    record.publish(task)?;
                }
                record
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Record::new()?,
            Err(e) => return Err(io(e)),
        };
        store.save(&record)?;
        Ok((store, record))
    }

    pub fn save(&self, record: &Record) -> Result<()> {
        let plain = serde_json::to_vec(record).map_err(|e| fail("storage", e.to_string()))?;
        if plain.len() as u64 > MAX_STORE / 2 {
            return Err(fail(
                "storage_full",
                "Fleet history/outbox is full. No acknowledgement was sent.",
            ));
        }
        let sealed = protect(&plain)?;
        let pending = self.path.with_extension("pending");
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&pending).map_err(io)?;
        file.write_all(&sealed).map_err(io)?;
        file.sync_all().map_err(io)?;
        drop(file);
        Self::replace(&pending, &self.path)?;
        #[cfg(unix)]
        std::fs::File::open(self.path.parent().unwrap())
            .map_err(io)?
            .sync_all()
            .map_err(io)?;
        Ok(())
    }

    #[cfg(windows)]
    fn replace(source: &Path, target: &Path) -> Result<()> {
        use std::os::windows::ffi::OsStrExt;
        use windows::{
            core::PCWSTR,
            Win32::Storage::FileSystem::{
                MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
            },
        };
        let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            MoveFileExW(
                PCWSTR(source.as_ptr()),
                PCWSTR(target.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(|e| fail("storage", e.to_string()))
    }
    #[cfg(not(windows))]
    fn replace(source: &Path, target: &Path) -> Result<()> {
        std::fs::rename(source, target).map_err(io)
    }
}

#[cfg(windows)]
fn crypt(bytes: &[u8], seal: bool) -> Result<Vec<u8>> {
    use windows::Win32::{
        Foundation::{LocalFree, HLOCAL},
        Security::Cryptography::{
            CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
        },
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // DPAPI CurrentUser binds the node key and all task data to this OS account and machine.
    unsafe {
        let result = if seal {
            CryptProtectData(
                &input,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        result.map_err(|e| fail("protected_storage", e.to_string()))?;
        let value = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        LocalFree(Some(HLOCAL(output.pbData.cast())));
        Ok(value)
    }
}
#[cfg(windows)]
fn protect(bytes: &[u8]) -> Result<Vec<u8>> {
    crypt(bytes, true)
}
#[cfg(windows)]
fn unprotect(bytes: &[u8]) -> Result<Vec<u8>> {
    crypt(bytes, false)
}
#[cfg(all(not(windows), test))]
fn protect(bytes: &[u8]) -> Result<Vec<u8>> {
    Ok(bytes.to_vec())
}
#[cfg(all(not(windows), test))]
fn unprotect(bytes: &[u8]) -> Result<Vec<u8>> {
    Ok(bytes.to_vec())
}
#[cfg(all(not(windows), not(test)))]
fn protect(_: &[u8]) -> Result<Vec<u8>> {
    Err(fail(
        "protected_storage",
        "Fleet credentials currently require Windows DPAPI",
    ))
}
#[cfg(all(not(windows), not(test)))]
fn unprotect(bytes: &[u8]) -> Result<Vec<u8>> {
    protect(bytes)
}
