//! The four C functions of `tdjson` behind a trait:
//! `DynLib` loads the real `tdjson.dll` lazily, `Replay` (tests and debug builds) plays
//! recorded fixtures. Nothing here parses JSON: strings in, strings out.

use std::ffi::{CStr, CString, c_char, c_int};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use crate::transport::TransportError;

/// `td_json_client.h`. Every method is cheap and never blocks longer than `receive`'s timeout.
/// `receive` is called from a single thread only (TDLib requirement for `td_receive`).
pub trait TdBindings: Send + Sync + 'static {
    /// `td_create_client_id`: the instance is really created by the first `send` to it.
    fn create_client_id(&self) -> Result<i32, TransportError>;
    /// `td_send`: never blocks; the answer comes through `receive` with the same `@extra`.
    fn send(&self, client_id: i32, request: &str) -> Result<(), TransportError>;
    /// `td_receive`: one response or update of any client, `None` after `timeout`.
    fn receive(&self, timeout: Duration) -> Result<Option<String>, TransportError>;
    /// `td_execute`: synchronous requests only (`setLogVerbosityLevel`, `setLogStream`…).
    fn execute(&self, request: &str) -> Result<Option<String>, TransportError>;
}

type CreateFn = unsafe extern "C" fn() -> c_int;
type SendFn = unsafe extern "C" fn(c_int, *const c_char);
type ReceiveFn = unsafe extern "C" fn(f64) -> *const c_char;
type ExecuteFn = unsafe extern "C" fn(*const c_char) -> *const c_char;

struct Loaded {
    create: CreateFn,
    send: SendFn,
    receive: ReceiveFn,
    execute: ExecuteFn,
    /// Kept last: the function pointers above are valid while the library stays loaded.
    _library: libloading::Library,
}

/// `tdjson.dll` loaded on first use: a release without the DLL starts, and every call then
/// answers `TG_LIB_MISSING` instead of failing at process start (no import library).
pub struct DynLib {
    path: PathBuf,
    loaded: OnceLock<Result<Loaded, String>>,
}

impl DynLib {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            loaded: OnceLock::new(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn loaded(&self) -> Result<&Loaded, TransportError> {
        self.loaded
            .get_or_init(|| {
                let started = std::time::Instant::now();
                let result = load(&self.path);
                tracing::info!(
                    event = "tdjson_loaded",
                    ok = result.is_ok(),
                    dur_ms = crate::logging::ms_since(started),
                    os_thread = super::os_thread_id(),
                    "tdjson.dll loaded"
                );
                result
            })
            .as_ref()
            .map_err(|error| TransportError::LibMissing(error.clone()))
    }
}

/// Dropping a loaded `DynLib` calls `FreeLibrary`: the journal says when and on which thread.
impl Drop for DynLib {
    fn drop(&mut self) {
        if matches!(self.loaded.get(), Some(Ok(_))) {
            tracing::info!(
                event = "tdjson_unload",
                os_thread = super::os_thread_id(),
                "tdjson.dll released"
            );
        }
    }
}

fn load(path: &Path) -> Result<Loaded, String> {
    let shown = path.display();
    if !path.is_file() {
        return Err(format!("{shown} not found"));
    }
    let library = open(path).map_err(|error| format!("{shown}: {error}"))?;
    // SAFETY: the types are those of `td_json_client.h` (TDLib 1.8.x, `extern "C"`); the
    // pointers are copied out and live as long as `_library`, which `Loaded` owns.
    unsafe {
        let create = *library
            .get::<CreateFn>(b"td_create_client_id\0")
            .map_err(|error| format!("{shown}: {error}"))?;
        let send = *library
            .get::<SendFn>(b"td_send\0")
            .map_err(|error| format!("{shown}: {error}"))?;
        let receive = *library
            .get::<ReceiveFn>(b"td_receive\0")
            .map_err(|error| format!("{shown}: {error}"))?;
        let execute = *library
            .get::<ExecuteFn>(b"td_execute\0")
            .map_err(|error| format!("{shown}: {error}"))?;
        Ok(Loaded {
            create,
            send,
            receive,
            execute,
            _library: library,
        })
    }
}

/// Dependencies (OpenSSL and zlib of a local TDLib build) are searched next to the DLL and in
/// the system folders, never in the current directory.
#[cfg(windows)]
fn open(path: &Path) -> Result<libloading::Library, String> {
    use libloading::os::windows::{
        LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, Library,
    };
    let absolute = std::path::absolute(path).map_err(|error| error.to_string())?;
    // SAFETY: loading runs the DLL's initialisers; `tdjson.dll` is the TDLib build the app
    // ships (its SHA-256 is pinned at packaging).
    unsafe {
        Library::load_with_flags(
            &absolute,
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
        )
    }
    .map(Into::into)
    .map_err(|error| error.to_string())
}

#[cfg(not(windows))]
fn open(path: &Path) -> Result<libloading::Library, String> {
    // SAFETY: as above.
    unsafe { libloading::Library::new(path) }.map_err(|error| error.to_string())
}

fn c_string(request: &str) -> Result<CString, TransportError> {
    CString::new(request).map_err(|_| TransportError::Protocol("request contains NUL".into()))
}

/// Copies the string TDLib owns (valid until the next call on this thread) into Rust.
///
/// # Safety
/// `pointer` is null or a NUL-terminated string returned by `td_receive` / `td_execute` on
/// the calling thread, not yet invalidated by another call.
unsafe fn owned(pointer: *const c_char) -> Result<Option<String>, TransportError> {
    if pointer.is_null() {
        return Ok(None);
    }
    // SAFETY: guaranteed by the caller.
    let text = unsafe { CStr::from_ptr(pointer) };
    text.to_str()
        .map(|text| Some(text.to_owned()))
        .map_err(|_| TransportError::Protocol("TDLib answer is not UTF-8".into()))
}

impl TdBindings for DynLib {
    fn create_client_id(&self) -> Result<i32, TransportError> {
        let loaded = self.loaded()?;
        // SAFETY: no argument; thread-safe per TDLib.
        Ok(unsafe { (loaded.create)() })
    }

    fn send(&self, client_id: i32, request: &str) -> Result<(), TransportError> {
        let loaded = self.loaded()?;
        let request = c_string(request)?;
        // SAFETY: `request` is NUL-terminated and outlives the call; `td_send` copies it.
        unsafe { (loaded.send)(client_id, request.as_ptr()) };
        Ok(())
    }

    fn receive(&self, timeout: Duration) -> Result<Option<String>, TransportError> {
        let loaded = self.loaded()?;
        // SAFETY: called from the single receive thread; the result is copied at once.
        unsafe { owned((loaded.receive)(timeout.as_secs_f64())) }
    }

    fn execute(&self, request: &str) -> Result<Option<String>, TransportError> {
        let loaded = self.loaded()?;
        let request = c_string(request)?;
        // SAFETY: as `send`; the result belongs to this thread until its next `td_execute`,
        // and it is copied before returning.
        unsafe { owned((loaded.execute)(request.as_ptr())) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_dll_is_a_typed_error_not_a_crash() {
        let lib = DynLib::new(r"C:\squeezegram-test\does-not-exist\tdjson.dll");
        for result in [
            lib.create_client_id().map(|_| ()),
            lib.send(1, "{}"),
            lib.receive(Duration::from_millis(1)).map(|_| ()),
            lib.execute("{}").map(|_| ()),
        ] {
            let error = result.unwrap_err();
            assert!(matches!(error, TransportError::LibMissing(_)), "{error}");
            assert_eq!(error.code_and_params().0, "TG_LIB_MISSING");
        }
    }

    #[test]
    fn a_file_that_is_not_a_dll_is_lib_missing() {
        let dir = std::env::temp_dir().join(format!("sg-dynlib-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("tdjson.dll");
        std::fs::write(&fake, b"not a PE file").unwrap();
        let error = DynLib::new(&fake).create_client_id().unwrap_err();
        assert!(matches!(error, TransportError::LibMissing(_)), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
