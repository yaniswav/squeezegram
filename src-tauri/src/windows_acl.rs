//! Shared descriptor lifetime for custom tools and temporary-folder checks.

use std::path::Path;

use windows::Win32::Foundation::{E_FAIL, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
use windows::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
};
use windows::core::{HSTRING, Result};

pub(crate) fn with_security<T>(
    path: &Path,
    check: impl FnOnce(PSID, *mut ACL) -> Result<T>,
) -> Result<T> {
    with_descriptor(path, |owner, dacl, _| {
        if dacl.is_null() {
            return Err(E_FAIL.into());
        }
        check(owner, dacl)
    })
}

fn with_descriptor<T>(
    path: &Path,
    check: impl FnOnce(PSID, *mut ACL, PSECURITY_DESCRIPTOR) -> Result<T>,
) -> Result<T> {
    struct Descriptor(PSECURITY_DESCRIPTOR);
    impl Drop for Descriptor {
        fn drop(&mut self) {
            if !self.0.0.is_null() {
                unsafe {
                    let _ = LocalFree(Some(HLOCAL(self.0.0)));
                }
            }
        }
    }
    let mut descriptor = Descriptor(PSECURITY_DESCRIPTOR::default());
    let mut dacl = std::ptr::null_mut();
    let mut owner = PSID::default();
    // The descriptor owns both pointers until the callback returns, including on errors.
    unsafe {
        GetNamedSecurityInfoW(
            &HSTRING::from(path.as_os_str()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            Some(&mut dacl),
            None,
            &mut descriptor.0,
        )
        .ok()?;
    }
    if owner.is_invalid() {
        return Err(E_FAIL.into());
    }
    check(owner, dacl, descriptor.0)
}

/// Resolve a custom choice to a private workspace. The writable parent is not trusted for
/// content isolation; path replacement during a job remains possible without handle binding.
pub(crate) fn private_temp_root(path: &Path) -> Result<std::path::PathBuf> {
    use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, HANDLE};
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows::Win32::Security::*;
    use windows::Win32::Storage::FileSystem::*;
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::core::{PCWSTR, PWSTR};

    fn directory(path: &Path) -> Result<()> {
        let meta =
            std::fs::symlink_metadata(path).map_err(|_| windows::core::Error::from(E_FAIL))?;
        if !meta.is_dir() || crate::queue::sweep::is_link(&meta) {
            return Err(E_FAIL.into());
        }
        Ok(())
    }

    directory(path)?;
    unsafe {
        let mut volume = [0u16; 32768];
        GetVolumePathNameW(&HSTRING::from(path.as_os_str()), &mut volume)?;
        let mut flags = 0;
        GetVolumeInformationW(
            PCWSTR(volume.as_ptr()),
            None,
            None,
            None,
            Some(&mut flags),
            None,
        )?;
        // Synthesized descriptors on filesystems without persistent ACLs prove nothing.
        const FILE_PERSISTENT_ACLS: u32 = 0x0000_0008;
        if flags & FILE_PERSISTENT_ACLS == 0 {
            return Err(E_FAIL.into());
        }
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
        let user = (|| -> Result<Vec<usize>> {
            let mut bytes = 0;
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut bytes);
            if bytes == 0 {
                return Err(E_FAIL.into());
            }
            let mut buffer = vec![0usize; (bytes as usize).div_ceil(std::mem::size_of::<usize>())];
            GetTokenInformation(
                token,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                bytes,
                &mut bytes,
            )?;
            Ok(buffer)
        })();
        let _ = CloseHandle(token);
        let user = user?;
        let current = (*(user.as_ptr().cast::<TOKEN_USER>())).User.Sid;
        let mut system = [0u32; SECURITY_MAX_SID_SIZE as usize / 4];
        let mut admins = system;
        for (kind, buffer) in [
            (WinLocalSystemSid, &mut system),
            (WinBuiltinAdministratorsSid, &mut admins),
        ] {
            let mut bytes = SECURITY_MAX_SID_SIZE;
            CreateWellKnownSid(
                kind,
                None,
                Some(PSID(buffer.as_mut_ptr().cast())),
                &mut bytes,
            )?;
        }
        let system = PSID(system.as_mut_ptr().cast());
        let admins = PSID(admins.as_mut_ptr().cast());
        with_descriptor(path, |owner, _, _| {
            if !IsValidSid(owner).as_bool()
                || ![current, system, admins]
                    .into_iter()
                    .any(|sid| EqualSid(owner, sid).is_ok())
            {
                return Err(E_FAIL.into());
            }
            Ok(())
        })?;

        let root = path.join("squeezegram-temp");
        let mut sid_text = PWSTR::null();
        ConvertSidToStringSidW(current, &mut sid_text)?;
        let sid = sid_text.to_string();
        let _ = LocalFree(Some(HLOCAL(sid_text.0.cast())));
        let sid = sid?;
        let sddl = HSTRING::from(format!("O:{sid}D:P(A;OICI;FA;;;{sid})(A;OICI;FA;;;SY)"));
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &sddl,
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        };
        // Security is applied by the creation itself, with no inherited/public window.
        let created = CreateDirectoryW(&HSTRING::from(root.as_os_str()), Some(&attributes));
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        if let Err(error) = created
            && error.code() != ERROR_ALREADY_EXISTS.to_hresult()
        {
            return Err(error);
        }
        // Also verifies a concurrent creator's result; never repairs or enters a rejected root.
        directory(&root)?;
        with_descriptor(&root, |owner, dacl, descriptor| {
            let mut control = 0u16;
            let mut revision = 0;
            GetSecurityDescriptorControl(descriptor, &mut control, &mut revision)?;
            if !IsValidSid(owner).as_bool()
                || EqualSid(owner, current).is_err()
                || control & SE_DACL_PROTECTED.0 == 0
                || dacl.is_null()
                || !IsValidAcl(dacl).as_bool()
            {
                return Err(E_FAIL.into());
            }
            for index in 0..u32::from((*dacl).AceCount) {
                let mut ace = std::ptr::null_mut();
                GetAce(dacl, index, &mut ace)?;
                let sid = ace_sid(ace)?;
                if ![current, system]
                    .into_iter()
                    .any(|known| EqualSid(sid, known).is_ok())
                {
                    return Err(E_FAIL.into());
                }
            }
            Ok(())
        })?;
        Ok(root)
    }
}

/// Trustee of an allow or deny ACE. Our private root only ever holds these two; any other
/// layout (object, callback, truncated) fails closed rather than being parsed.
unsafe fn ace_sid(ace: *mut std::ffi::c_void) -> Result<PSID> {
    use windows::Win32::Security::{ACCESS_ALLOWED_ACE, ACE_HEADER, IsValidSid};
    unsafe {
        let header = &*ace.cast::<ACE_HEADER>();
        // Allow and deny ACEs share the layout: header, mask, then the SID.
        if header.AceType > 1
            || usize::from(header.AceSize) < std::mem::size_of::<ACCESS_ALLOWED_ACE>()
        {
            return Err(E_FAIL.into());
        }
        let sid = PSID(
            std::ptr::addr_of!((*ace.cast::<ACCESS_ALLOWED_ACE>()).SidStart)
                .cast_mut()
                .cast(),
        );
        if !IsValidSid(sid).as_bool() {
            return Err(E_FAIL.into());
        }
        Ok(sid)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::queue::test_util::TestDir;
    use std::os::windows::process::CommandExt;
    use windows::Win32::Security::*;

    pub(crate) fn grant(path: &Path, ace: &str) {
        icacls(path, "/grant", ace);
    }

    fn icacls(path: &Path, action: &str, ace: &str) {
        let output = std::process::Command::new("icacls.exe")
            .arg(path)
            .args([action, ace])
            .creation_flags(0x0800_0000)
            .output()
            .unwrap();
        assert!(output.status.success(), "icacls failed: {:?}", output);
    }

    fn dacl_bytes(path: &Path) -> Vec<u8> {
        with_security(path, |_, dacl| unsafe {
            Ok(
                std::slice::from_raw_parts(dacl.cast::<u8>(), usize::from((*dacl).AclSize))
                    .to_vec(),
            )
        })
        .unwrap()
    }

    fn assert_private(root: &Path) {
        unsafe {
            with_descriptor(root, |owner, dacl, descriptor| {
                let mut control = 0u16;
                let mut revision = 0;
                GetSecurityDescriptorControl(descriptor, &mut control, &mut revision)?;
                assert_ne!(control & SE_DACL_PROTECTED.0, 0);
                assert_eq!((*dacl).AceCount, 2);
                for path in [root.to_path_buf(), root.join("file.bin"), root.join("job")] {
                    with_security(&path, |_, dacl| {
                        assert_eq!((*dacl).AceCount, 2);
                        for index in 0..u32::from((*dacl).AceCount) {
                            let mut ace = std::ptr::null_mut();
                            GetAce(dacl, index, &mut ace)?;
                            let sid = ace_sid(ace)?;
                            assert!(
                                EqualSid(sid, owner).is_ok()
                                    || IsWellKnownSid(sid, WinLocalSystemSid).as_bool()
                            );
                            if path != root {
                                assert_ne!(
                                    (*ace.cast::<ACE_HEADER>()).AceFlags & INHERITED_ACE.0 as u8,
                                    0
                                );
                            }
                        }
                        Ok(())
                    })?;
                }
                Ok(())
            })
            .unwrap();
        }
    }

    #[test]
    fn only_allow_and_deny_ace_trustees_are_read() {
        unsafe {
            for kind in 0u8..=21 {
                let mut buffer = [0u32; 32];
                let ace = buffer.as_mut_ptr().cast::<std::ffi::c_void>();
                let header = &mut *ace.cast::<ACE_HEADER>();
                header.AceType = kind;
                let expected = PSID(ace.cast::<u8>().add(8).cast());
                let mut bytes = SECURITY_MAX_SID_SIZE;
                CreateWellKnownSid(WinBuiltinUsersSid, None, Some(expected), &mut bytes).unwrap();
                header.AceSize = (8 + bytes) as u16;
                if kind <= 1 {
                    assert!(EqualSid(ace_sid(ace).unwrap(), expected).is_ok());
                } else {
                    assert!(ace_sid(ace).is_err(), "type {kind}");
                }
            }
        }
    }

    #[test]
    fn authenticated_users_modify_is_accepted_but_children_are_private() {
        // Do not depend on TEMP's inherited ACL: explicitly add the broad grant to generated dirs.
        for inherited in [false, true] {
            let dir = TestDir::new("temp-acl-broad-parent");
            let chosen = dir.path().join("chosen");
            if inherited {
                grant(dir.path(), "*S-1-5-11:(OI)(CI)M");
            }
            std::fs::create_dir(&chosen).unwrap();
            if !inherited {
                grant(&chosen, "*S-1-5-11:(OI)(CI)M");
            }
            // The old immediate-parent delete-child rule is intentionally gone as well.
            grant(dir.path(), "*S-1-1-0:(DC)");
            let root = crate::settings::resolve_custom_temp_dir(&chosen).unwrap();
            assert_eq!(root, chosen.join("squeezegram-temp"));
            std::fs::write(root.join("file.bin"), b"generated").unwrap();
            std::fs::create_dir(root.join("job")).unwrap();
            assert_private(&root);
            assert_eq!(
                std::fs::read_dir(&chosen).unwrap().count(),
                1,
                "probe stays private"
            );
        }
    }

    #[test]
    fn second_resolution_reuses_private_root() {
        let dir = TestDir::new("temp-acl-reuse");
        let root = crate::settings::resolve_custom_temp_dir(dir.path()).unwrap();
        std::fs::write(root.join("keep.bin"), b"same workspace").unwrap();
        assert_eq!(
            crate::settings::resolve_custom_temp_dir(dir.path()).unwrap(),
            root
        );
        assert_eq!(
            std::fs::read(root.join("keep.bin")).unwrap(),
            b"same workspace"
        );
        assert_eq!(std::fs::read_dir(root).unwrap().count(), 1);
    }

    #[test]
    fn inherited_existing_root_is_refused_without_touching_contents() {
        let dir = TestDir::new("temp-acl-existing");
        let root = dir.path().join("squeezegram-temp");
        std::fs::create_dir(&root).unwrap();
        let sentinel = root.join("keep.bin");
        let bytes = b"generated\0sentinel\xff";
        std::fs::write(&sentinel, bytes).unwrap();
        let acl = dacl_bytes(&root);
        assert!(private_temp_root(dir.path()).is_err());
        assert_eq!(dacl_bytes(&root), acl, "refusal must not repair the ACL");
        assert_eq!(std::fs::read(sentinel).unwrap(), bytes);
        assert_eq!(std::fs::read_dir(root).unwrap().count(), 1);
    }

    #[test]
    fn extra_read_allow_or_deny_ace_is_refused() {
        for action in ["/grant", "/deny"] {
            let dir = TestDir::new("temp-acl-extra");
            let root = private_temp_root(dir.path()).unwrap();
            icacls(&root, action, "*S-1-5-32-545:R");
            let acl = dacl_bytes(&root);
            assert!(private_temp_root(dir.path()).is_err(), "{action}");
            assert_eq!(dacl_bytes(&root), acl, "refusal must not repair the ACL");
            // Remove the generated deny to let TestDir clean up on standard user tokens.
            if action == "/deny" {
                icacls(&root, "/remove:d", "*S-1-5-32-545");
            }
        }
    }

    #[test]
    fn junctions_for_choice_or_private_root_are_refused() {
        let dir = TestDir::new("temp-acl-junction");
        let safe = dir.path().join("safe");
        std::fs::create_dir(&safe).unwrap();
        private_temp_root(&safe).unwrap();
        for link in [dir.path().join("link"), dir.path().join("squeezegram-temp")] {
            let output = std::process::Command::new("cmd.exe")
                .args(["/d", "/c", "mklink", "/J"])
                .arg(&link)
                .arg(&safe)
                .creation_flags(0x0800_0000)
                .output()
                .unwrap();
            assert!(output.status.success(), "junction creation failed");
            assert_eq!(
                crate::settings::resolve_custom_temp_dir(if link.file_name().unwrap() == "link" {
                    &link
                } else {
                    dir.path()
                })
                .unwrap_err(),
                crate::settings::TempDirProblem::Unsafe
            );
            std::fs::remove_dir(link).unwrap();
        }
        assert_eq!(std::fs::read_dir(&safe).unwrap().count(), 1);
    }

    #[test]
    fn null_private_dacl_is_refused() {
        use windows::Win32::Security::Authorization::SetNamedSecurityInfoW;
        let dir = TestDir::new("temp-acl-null");
        let root = private_temp_root(dir.path()).unwrap();
        unsafe {
            SetNamedSecurityInfoW(
                &HSTRING::from(root.as_os_str()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                None,
                None,
                None,
                None,
            )
            .ok()
            .unwrap();
        }
        assert!(private_temp_root(dir.path()).is_err());
    }

    #[test]
    fn missing_choice_and_file_workspace_are_refused() {
        let dir = TestDir::new("temp-acl-missing");
        assert!(private_temp_root(&dir.path().join("absent")).is_err());
        std::fs::write(dir.path().join("squeezegram-temp"), b"unchanged").unwrap();
        assert!(private_temp_root(dir.path()).is_err());
        assert_eq!(
            std::fs::read(dir.path().join("squeezegram-temp")).unwrap(),
            b"unchanged"
        );
    }
}
