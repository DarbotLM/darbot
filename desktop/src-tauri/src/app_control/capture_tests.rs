use super::*;

#[test]
fn connection_file_is_written_only_inside_restricted_owned_directory() {
    struct OwnedFixture(std::path::PathBuf);
    impl Drop for OwnedFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let directory = OwnedFixture(std::env::current_dir().unwrap().join(format!(
        ".app-control-fixture-{}",
        crate::app_control::random_id()
    )));
    private_directory(&directory.0).unwrap();
    let file = directory.0.join("connection.json");
    write_discovery(&file, br#"{"fixture":true}"#).unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), br#"{"fixture":true}"#);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&directory.0)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    #[cfg(windows)]
    unsafe {
        use std::os::windows::ffi::OsStrExt;
        use windows::{
            core::{PCWSTR, PWSTR},
            Win32::{
                Foundation::{LocalFree, HLOCAL},
                Security::{
                    Authorization::{
                        ConvertSecurityDescriptorToStringSecurityDescriptorW,
                        GetNamedSecurityInfoW, SE_FILE_OBJECT,
                    },
                    DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
                },
            },
        };
        let path: Vec<u16> = directory
            .0
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        GetNamedSecurityInfoW(
            PCWSTR(path.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            None,
            None,
            &mut descriptor,
        )
        .ok()
        .unwrap();
        let mut text = PWSTR::null();
        let converted = ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            1,
            DACL_SECURITY_INFORMATION,
            &mut text,
            None,
        );
        let sddl = if converted.is_ok() {
            text.to_string().unwrap()
        } else {
            String::new()
        };
        if !text.is_null() {
            let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        }
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        assert!(
            sddl.starts_with("D:P"),
            "DACL must disable inherited access"
        );
        assert_eq!(
            sddl.matches('(').count(),
            1,
            "Only the owner may access connection files"
        );
        assert!(sddl.contains(";;;OW)") || sddl.contains(";;;S-1-3-4)"));
    }
}
