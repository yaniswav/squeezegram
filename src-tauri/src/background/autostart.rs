//! Start with Windows: one value in `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
//!
//! A registry value rather than `tauri-plugin-autostart`: the plugin wraps exactly this value
//! (through `auto-launch`) and adds a crate, a page-facing permission set and a JavaScript
//! package for something the core does in a few calls with the `windows` crate it already
//! has. The page never reaches the registry: it asks `autostart_set`, which checks the
//! license.
//!
//! The entry is the only record of the choice (no setting to drift from it). Windows also
//! keeps an on/off flag per entry for the Startup page of Task Manager
//! (`...\Explorer\StartupApproved\Run`, a binary value whose first byte is 3 when the user
//! switched the entry off). The option reads as off while that flag says so, since Windows
//! then starts nothing; turning the option on clears it, an explicit choice of the user.

use std::io;
use std::path::Path;

use super::BACKGROUND_ARG;
use crate::entitlements::{Entitlements, Feature};

/// The persistence of the "start with Windows" choice; the registry in the app, a fake in tests.
pub trait Autostart: Send + Sync {
    /// What the Run entry runs; `None` when there is no entry.
    fn command(&self) -> Option<String>;
    /// False when the user switched the entry off on Task Manager's Startup page: Windows
    /// then skips it although it exists.
    fn is_approved(&self) -> bool;
    /// Whether Windows will start the app at login.
    fn is_enabled(&self) -> bool {
        self.command().is_some() && self.is_approved()
    }
    /// Creates or replaces the Run entry, which runs `command`.
    fn write(&self, command: &str) -> io::Result<()>;
    /// Clears the Task Manager flag: the entry starts again if the user switched it off there.
    fn approve(&self) -> io::Result<()>;
    /// Removes the entry; already gone is not an error.
    fn disable(&self) -> io::Result<()>;
}

/// Whether a `StartupApproved` value says the user switched the entry off in Task Manager.
fn switched_off(value: &[u8]) -> bool {
    value.first() == Some(&3)
}

/// The command line the entry runs: the executable, quoted, and the hidden-start argument.
pub fn command_line(exe: &Path) -> String {
    format!("\"{}\" {BACKGROUND_ARG}", exe.display())
}

/// Why `set` did nothing.
#[derive(Debug, thiserror::Error)]
pub enum AutostartError {
    #[error("start with Windows is a paid feature")]
    NotLicensed,
    #[error("registry: {0}")]
    Io(#[from] io::Error),
}

/// Turns the entry on (licensed only) or off (always: losing the license must not trap the
/// entry). Returns whether the entry exists afterwards.
pub fn set(
    autostart: &dyn Autostart,
    entitlements: &dyn Entitlements,
    exe: &Path,
    enabled: bool,
) -> Result<bool, AutostartError> {
    if enabled {
        if !entitlements.allows(Feature::Background) {
            return Err(AutostartError::NotLicensed);
        }
        autostart.write(&command_line(exe))?;
        autostart.approve()?;
    } else {
        autostart.disable()?;
    }
    Ok(autostart.is_enabled())
}

/// Points an existing entry at this executable. The entry keeps the path it was created with:
/// after an update that moves the app, or a portable copy that is moved, it would start
/// nothing while the option reads as on. Only with the license (without it the entry is left
/// alone) and without touching the Task Manager flag. Returns the command it replaced.
pub fn repair(
    autostart: &dyn Autostart,
    entitlements: &dyn Entitlements,
    exe: &Path,
) -> io::Result<Option<String>> {
    if !entitlements.allows(Feature::Background) {
        return Ok(None);
    }
    let Some(found) = autostart.command() else {
        return Ok(None);
    };
    let wanted = command_line(exe);
    if found.eq_ignore_ascii_case(&wanted) {
        return Ok(None);
    }
    autostart.write(&wanted)?;
    Ok(Some(found))
}

#[cfg(windows)]
pub use registry::RegistryAutostart;

#[cfg(windows)]
mod registry {
    use std::io;

    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_BINARY, RRF_RT_REG_SZ, RegDeleteKeyValueW,
        RegGetValueW, RegSetKeyValueW,
    };
    use windows::core::{PCWSTR, w};

    use super::Autostart;

    const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    const APPROVED_KEY: PCWSTR =
        w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run");
    const VALUE: PCWSTR = w!("squeezegram");

    /// `Ok` for success, and for a value that was not there when `missing_ok`.
    fn check(status: WIN32_ERROR, missing_ok: bool) -> io::Result<()> {
        if status == ERROR_SUCCESS || (missing_ok && status == ERROR_FILE_NOT_FOUND) {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status.0 as i32))
        }
    }

    pub struct RegistryAutostart;

    impl Autostart for RegistryAutostart {
        fn is_approved(&self) -> bool {
            // Windows writes 12 bytes: a status (2 or 6 enabled, 3 disabled) and a time.
            let mut data = [0u8; 12];
            let mut size = data.len() as u32;
            // SAFETY: the buffer and its byte size are valid for the call; the names are
            // NUL-ended literals.
            let status = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    APPROVED_KEY,
                    VALUE,
                    RRF_RT_REG_BINARY,
                    None,
                    Some(data.as_mut_ptr().cast()),
                    Some(&mut size),
                )
            };
            // No value, or one that cannot be read: nobody switched it off.
            !(status == ERROR_SUCCESS && super::switched_off(&data[..size as usize]))
        }

        fn command(&self) -> Option<String> {
            let mut size = 0u32;
            // SAFETY: no data buffer is given, so only the size is returned; `size` is a valid
            // out value. The key and value names are NUL-ended literals.
            let status = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    RUN_KEY,
                    VALUE,
                    RRF_RT_REG_SZ,
                    None,
                    None,
                    Some(&mut size),
                )
            };
            if status != ERROR_SUCCESS {
                return None;
            }
            let mut text = vec![0u16; (size as usize).div_ceil(2)];
            let mut size = (text.len() * 2) as u32;
            // SAFETY: the buffer and its byte size are valid for the call.
            let status = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    RUN_KEY,
                    VALUE,
                    RRF_RT_REG_SZ,
                    None,
                    Some(text.as_mut_ptr().cast()),
                    Some(&mut size),
                )
            };
            if status != ERROR_SUCCESS {
                return None;
            }
            // The size counts the closing NUL.
            let len = (size as usize / 2).saturating_sub(1).min(text.len());
            Some(String::from_utf16_lossy(&text[..len]))
        }

        fn write(&self, command: &str) -> io::Result<()> {
            let data: Vec<u16> = command.encode_utf16().chain(Some(0)).collect();
            // SAFETY: `data` is NUL-ended and its byte size is given; the names are literals.
            let status = unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    RUN_KEY,
                    VALUE,
                    REG_SZ.0,
                    Some(data.as_ptr().cast()),
                    (data.len() * 2) as u32,
                )
            };
            check(status, false)
        }

        fn approve(&self) -> io::Result<()> {
            // SAFETY: the names are NUL-ended literals.
            let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, APPROVED_KEY, VALUE) };
            check(status, true)
        }

        fn disable(&self) -> io::Result<()> {
            // SAFETY: the names are NUL-ended literals.
            let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE) };
            check(status, true)
        }
    }
}

/// No Run key outside Windows: the option is never on.
#[cfg(not(windows))]
pub struct RegistryAutostart;

#[cfg(not(windows))]
impl Autostart for RegistryAutostart {
    fn command(&self) -> Option<String> {
        None
    }

    fn is_approved(&self) -> bool {
        true
    }

    fn write(&self, _command: &str) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }

    fn approve(&self) -> io::Result<()> {
        Ok(())
    }

    fn disable(&self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use super::*;
    use crate::entitlements::{EntitlementStatus, FreeOnly};

    /// The registry value, in memory.
    #[derive(Default)]
    struct Fake {
        command: Mutex<Option<String>>,
        /// The Task Manager switch is off.
        switched_off: Mutex<bool>,
        fail: bool,
    }

    impl Autostart for Fake {
        fn command(&self) -> Option<String> {
            self.command.lock().unwrap().clone()
        }

        fn is_approved(&self) -> bool {
            !*self.switched_off.lock().unwrap()
        }

        fn write(&self, command: &str) -> io::Result<()> {
            if self.fail {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            *self.command.lock().unwrap() = Some(command.to_owned());
            Ok(())
        }

        fn approve(&self) -> io::Result<()> {
            *self.switched_off.lock().unwrap() = false;
            Ok(())
        }

        fn disable(&self) -> io::Result<()> {
            if self.fail {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            *self.command.lock().unwrap() = None;
            Ok(())
        }
    }

    /// Grants `background` only.
    struct Licensed;

    impl Entitlements for Licensed {
        fn allows(&self, feature: Feature) -> bool {
            feature == Feature::Background
        }

        fn status(&self) -> EntitlementStatus {
            EntitlementStatus::FREE_BUILD
        }
    }

    fn exe() -> PathBuf {
        PathBuf::from(r"C:\Program Files\squeezegram\squeezegram.exe")
    }

    #[test]
    fn the_entry_runs_the_quoted_executable_with_the_hidden_start_argument() {
        assert_eq!(
            command_line(&exe()),
            r#""C:\Program Files\squeezegram\squeezegram.exe" --background"#
        );
    }

    #[test]
    fn turning_it_on_needs_the_license_and_writes_nothing_without_it() {
        let fake = Fake::default();
        let error = set(&fake, &FreeOnly, &exe(), true).unwrap_err();
        assert!(matches!(error, AutostartError::NotLicensed));
        assert!(!fake.is_enabled());

        assert!(set(&fake, &Licensed, &exe(), true).unwrap());
        assert_eq!(
            fake.command.lock().unwrap().as_deref(),
            Some(command_line(&exe()).as_str())
        );
    }

    #[test]
    fn turning_it_off_removes_the_entry_even_after_the_license_is_lost() {
        let fake = Fake::default();
        set(&fake, &Licensed, &exe(), true).unwrap();
        // The license goes away: the entry stays until the user removes it...
        assert!(fake.is_enabled());
        // ...and removing it is never refused.
        assert!(!set(&fake, &FreeOnly, &exe(), false).unwrap());
        assert!(!fake.is_enabled());
        // Already gone is fine.
        assert!(!set(&fake, &FreeOnly, &exe(), false).unwrap());
    }

    #[test]
    fn an_entry_switched_off_in_task_manager_reads_as_off_until_turned_on_again() {
        let fake = Fake::default();
        set(&fake, &Licensed, &exe(), true).unwrap();
        *fake.switched_off.lock().unwrap() = true;
        assert!(fake.command().is_some());
        assert!(!fake.is_enabled(), "Windows will not start it");
        // The user turns the option on: the flag is cleared.
        assert!(set(&fake, &Licensed, &exe(), true).unwrap());
    }

    #[test]
    fn the_task_manager_flag_is_read_from_the_first_byte() {
        let enabled = [2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let enabled_by_policy = [6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let disabled = [3, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8];
        assert!(!switched_off(&enabled));
        assert!(!switched_off(&enabled_by_policy));
        assert!(switched_off(&disabled));
        assert!(!switched_off(&[]), "an empty value switches nothing off");
    }

    #[test]
    fn an_entry_pointing_elsewhere_is_rewritten_for_a_licensed_start_only() {
        let moved = r#""D:\Old\squeezegram.exe" --background"#;
        let fake = Fake::default();
        *fake.command.lock().unwrap() = Some(moved.to_owned());
        *fake.switched_off.lock().unwrap() = true;

        // Without the license the entry is left as it is.
        assert_eq!(repair(&fake, &FreeOnly, &exe()).unwrap(), None);
        assert_eq!(fake.command().as_deref(), Some(moved));

        // Licensed: it follows the executable, and the user's Task Manager choice stays.
        assert_eq!(
            repair(&fake, &Licensed, &exe()).unwrap().as_deref(),
            Some(moved)
        );
        assert_eq!(
            fake.command().as_deref(),
            Some(command_line(&exe()).as_str())
        );
        assert!(!fake.is_approved());

        // Already right (whatever the case of the path): nothing to do.
        assert_eq!(repair(&fake, &Licensed, &exe()).unwrap(), None);
        let upper = command_line(&exe()).to_uppercase();
        *fake.command.lock().unwrap() = Some(upper.clone());
        assert_eq!(repair(&fake, &Licensed, &exe()).unwrap(), None);
        assert_eq!(fake.command().as_deref(), Some(upper.as_str()));
    }

    #[test]
    fn no_entry_means_nothing_to_repair() {
        let fake = Fake::default();
        assert_eq!(repair(&fake, &Licensed, &exe()).unwrap(), None);
        assert!(fake.command().is_none(), "never creates the entry");
    }

    #[test]
    fn a_registry_failure_is_reported_and_leaves_the_state_as_it_was() {
        let fake = Fake {
            fail: true,
            ..Fake::default()
        };
        assert!(matches!(
            set(&fake, &Licensed, &exe(), true),
            Err(AutostartError::Io(_))
        ));
        assert!(!fake.is_enabled());
    }
}
