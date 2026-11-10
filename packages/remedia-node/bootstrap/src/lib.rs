//! Minimal native library loader for `@remedia/node`.
//!
//! The Remedia engine addon links FFmpeg dynamically, so the operating system must be able to
//! resolve the FFmpeg shared libraries before the addon can initialize. This crate has no FFmpeg
//! dependency. It is loaded first and opens already-selected FFmpeg libraries by explicit path, so
//! the platform loader later satisfies the engine addon's dependencies from the loaded images:
//!
//! - glibc, musl, FreeBSD and Android match `DT_NEEDED` entries against loaded `SONAME`s.
//! - macOS matches `@rpath/...` install names against loaded images.
//! - Windows matches import-table names against loaded module base names.
//!
//! Discovery, ABI checks and diagnostics live in TypeScript; this crate only performs the
//! operations that require native loader APIs.

use std::sync::Mutex;

use napi_derive::napi;

#[cfg(unix)]
mod platform {
    use std::ffi::{CStr, CString, c_void};

    use super::{LoadOutcome, RawFailure};

    /// Open `target` and, if requested, read the `unsigned (*)(void)` version function.
    ///
    /// A bare name (no path separator) is resolved by the system loader's own search rules; an
    /// absolute path is opened as is. Images are opened `RTLD_LOCAL`, so their symbols do not leak
    /// into the process-wide scope and cannot interpose another addon's libraries.
    pub(super) fn load(
        target: &str,
        version_symbol: Option<&str>,
    ) -> Result<LoadOutcome, RawFailure> {
        let target_c = CString::new(target).map_err(|_| RawFailure {
            code: 0,
            message: "library path contains a NUL byte".to_owned(),
        })?;

        // SAFETY: `target_c` is a valid NUL-terminated string. The returned handle is
        // intentionally never closed: unloading FFmpeg while the engine uses it would be unsound,
        // and the libraries must stay resident for the lifetime of the process.
        let handle = unsafe { libc::dlopen(target_c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return Err(RawFailure {
                code: 0,
                message: last_dl_error(),
            });
        }

        let mut version = None;
        let mut path = None;

        if let Some(symbol) = version_symbol {
            let symbol_c = CString::new(symbol).map_err(|_| RawFailure {
                code: 0,
                message: "version symbol contains a NUL byte".to_owned(),
            })?;

            // SAFETY: `handle` was returned by `dlopen` above and `symbol_c` is NUL-terminated.
            let address = unsafe { libc::dlsym(handle, symbol_c.as_ptr()) };
            if address.is_null() {
                return Err(RawFailure {
                    code: 0,
                    message: format!("{target} does not export {symbol}: {}", last_dl_error()),
                });
            }

            // SAFETY: FFmpeg's `*_version` functions have the C signature `unsigned (*)(void)`.
            // The caller selects only those documented symbols.
            let function: extern "C" fn() -> libc::c_uint = unsafe { std::mem::transmute(address) };
            version = Some(function());
            path = image_path(address);
        }

        Ok(LoadOutcome {
            handle: handle as usize,
            path,
            version,
        })
    }

    /// Release one reference to a handle returned by [`load`].
    pub(super) fn close(handle: usize) {
        // SAFETY: `handle` came from `dlopen` in `load` and is closed at most once, because the
        // registry that owns it drains each entry exactly once.
        unsafe { libc::dlclose(handle as *mut c_void) };
    }

    fn image_path(address: *mut c_void) -> Option<String> {
        let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };

        // SAFETY: `address` points into a loaded image and `info` is a valid out-parameter.
        if unsafe { libc::dladdr(address, &mut info) } == 0 || info.dli_fname.is_null() {
            return None;
        }

        // SAFETY: `dli_fname` is a NUL-terminated string owned by the loader.
        Some(
            unsafe { CStr::from_ptr(info.dli_fname) }
                .to_string_lossy()
                .into_owned(),
        )
    }

    fn last_dl_error() -> String {
        // SAFETY: `dlerror` returns null or a NUL-terminated string valid until the next
        // dl* call on this thread, and it is copied immediately.
        let message = unsafe { libc::dlerror() };
        if message.is_null() {
            "unknown dynamic loader error".to_owned()
        } else {
            unsafe { CStr::from_ptr(message) }
                .to_string_lossy()
                .into_owned()
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::ffi::OsStr;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::Path;

    use windows_sys::Win32::Foundation::{FreeLibrary, GetLastError, MAX_PATH};
    use windows_sys::Win32::System::LibraryLoader::{
        GetModuleFileNameW, GetProcAddress, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
        LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LoadLibraryExW,
    };

    use super::{LoadOutcome, RawFailure};

    /// Open the absolute DLL `target` and, if requested, read its version function.
    ///
    /// `LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR` resolves the DLL's own dependencies from its directory,
    /// and `LOAD_LIBRARY_SEARCH_DEFAULT_DIRS` limits the rest to the application and system
    /// directories. The process `PATH` and current directory are neither searched nor modified.
    pub(super) fn load(
        target: &str,
        version_symbol: Option<&str>,
    ) -> Result<LoadOutcome, RawFailure> {
        if !Path::new(target).is_absolute() {
            return Err(RawFailure {
                code: 0,
                message: format!("{target} is not an absolute path; refusing to search for it"),
            });
        }

        let wide: Vec<u16> = OsStr::new(target).encode_wide().chain(Some(0)).collect();

        // SAFETY: `wide` is a NUL-terminated UTF-16 path. The module is intentionally never freed
        // because the engine addon depends on it for the lifetime of the process.
        let module = unsafe {
            LoadLibraryExW(
                wide.as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            )
        };
        if module.is_null() {
            return Err(last_error());
        }

        let mut version = None;
        let mut path = None;

        if let Some(symbol) = version_symbol {
            let mut symbol_c = symbol.as_bytes().to_vec();
            symbol_c.push(0);

            // SAFETY: `module` is a valid module handle and `symbol_c` is NUL-terminated.
            let address = unsafe { GetProcAddress(module, symbol_c.as_ptr()) };
            let Some(address) = address else {
                return Err(RawFailure {
                    code: 127,
                    message: format!("{target} does not export {symbol}"),
                });
            };

            // SAFETY: FFmpeg's `*_version` functions have the C signature `unsigned (*)(void)`.
            let function: extern "system" fn() -> u32 = unsafe { std::mem::transmute(address) };
            version = Some(function());

            let mut buffer = vec![0u16; (MAX_PATH as usize) * 4];

            // SAFETY: `buffer` is writable for its stated length.
            let length =
                unsafe { GetModuleFileNameW(module, buffer.as_mut_ptr(), buffer.len() as u32) }
                    as usize;
            if length > 0 && length < buffer.len() {
                path = Some(
                    std::ffi::OsString::from_wide(&buffer[..length])
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }

        Ok(LoadOutcome {
            handle: module as usize,
            path,
            version,
        })
    }

    /// Release one reference to a module returned by [`load`].
    pub(super) fn close(handle: usize) {
        // SAFETY: `handle` came from `LoadLibraryExW` in `load` and is freed at most once, because
        // the registry that owns it drains each entry exactly once.
        unsafe { FreeLibrary(handle as *mut std::ffi::c_void) };
    }

    fn last_error() -> RawFailure {
        // SAFETY: `GetLastError` has no preconditions.
        let code = unsafe { GetLastError() };
        RawFailure {
            code: code as i32,
            message: std::io::Error::from_raw_os_error(code as i32).to_string(),
        }
    }
}

/// Handles opened by [`load_library`] since the last [`commit_loaded`] or [`rollback_loaded`].
static PENDING: Mutex<Vec<usize>> = Mutex::new(Vec::new());

struct LoadOutcome {
    handle: usize,
    path: Option<String>,
    version: Option<u32>,
}

struct RawFailure {
    code: i32,
    message: String,
}

/// Result of one [`load_library`] call.
#[napi(object)]
pub struct LoadedLibrary {
    /// Whether the library was opened (and, if requested, its version symbol was read).
    pub ok: bool,
    /// File path of the loaded image as reported by the platform loader.
    pub path: Option<String>,
    /// Packed FFmpeg version (`major << 16 | minor << 8 | micro`) when a version symbol was given.
    pub version: Option<u32>,
    /// Operating-system error code (Windows `GetLastError`); `0` where the platform has none.
    pub code: Option<i32>,
    /// Loader diagnostic when `ok` is `false`.
    pub message: Option<String>,
}

/// Interface version of this bootstrap, bumped when the exported functions change shape.
#[napi]
pub fn bootstrap_version() -> u32 {
    1
}

/// Open one shared library and keep it resident for the life of the process.
///
/// `target` is an absolute path, or a bare soname on Unix-like systems to ask the system loader.
/// When `version_symbol` is set it is called as `unsigned (*)(void)` and reported in `version`.
/// Failure is returned in the result rather than thrown so callers can classify it.
#[napi]
pub fn load_library(target: String, version_symbol: Option<String>) -> LoadedLibrary {
    match platform::load(&target, version_symbol.as_deref()) {
        Ok(outcome) => {
            pending().push(outcome.handle);

            LoadedLibrary {
                ok: true,
                path: outcome.path,
                version: outcome.version,
                code: None,
                message: None,
            }
        }
        Err(failure) => LoadedLibrary {
            ok: false,
            path: None,
            version: None,
            code: Some(failure.code),
            message: Some(failure.message),
        },
    }
}

fn pending() -> std::sync::MutexGuard<'static, Vec<usize>> {
    // A poisoned lock only means another call panicked; the handle list itself stays valid.
    PENDING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Keep every library opened since the previous commit or rollback resident for the process.
///
/// Call this once a complete FFmpeg installation has loaded and been verified.
#[napi]
pub fn commit_loaded() {
    pending().clear();
}

/// Close, newest first, every library opened since the previous commit or rollback.
///
/// This lets discovery abandon a partially loaded candidate without leaving some of its libraries
/// resident, where they could satisfy the dependencies of a different candidate. Libraries that
/// are still referenced by another loaded image stay mapped, as with any `dlclose`.
#[napi]
pub fn rollback_loaded() {
    let handles = std::mem::take(&mut *pending());

    for handle in handles.into_iter().rev() {
        platform::close(handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A library every supported host has, used where a load must succeed.
    #[cfg(target_os = "macos")]
    const SYSTEM_LIBRARY: &str = "/usr/lib/libSystem.B.dylib";
    #[cfg(all(target_os = "linux", not(target_env = "musl")))]
    const SYSTEM_LIBRARY: &str = "libc.so.6";
    #[cfg(target_os = "freebsd")]
    const SYSTEM_LIBRARY: &str = "libc.so.7";

    /// Successful loads share the process-wide pending list, so tests that load must not overlap.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn missing_library_reports_a_loader_failure() {
        #[cfg(unix)]
        let target = "/nonexistent/remedia/libmissing.so.1";
        #[cfg(windows)]
        let target = "C:\\nonexistent\\remedia\\missing.dll";

        let result = load_library(target.to_owned(), None);

        assert!(!result.ok);
        assert!(result.message.is_some_and(|message| !message.is_empty()));
    }

    #[test]
    fn nul_bytes_are_rejected_without_calling_the_loader() {
        let result = load_library("lib\0evil".to_owned(), None);

        assert!(!result.ok);
    }

    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        all(target_os = "linux", not(target_env = "musl"))
    ))]
    #[test]
    fn system_library_loads_by_path_or_name() {
        let _guard = serial();

        let target = SYSTEM_LIBRARY;

        let result = load_library(target.to_owned(), None);

        assert!(result.ok, "{:?}", result.message);
    }

    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        all(target_os = "linux", not(target_env = "musl"))
    ))]
    #[test]
    fn rollback_and_commit_drain_the_pending_list() {
        let _guard = serial();

        let target = SYSTEM_LIBRARY;

        assert!(load_library(target.to_owned(), None).ok);
        rollback_loaded();
        assert!(pending().is_empty());

        assert!(load_library(target.to_owned(), None).ok);
        commit_loaded();
        assert!(pending().is_empty());
    }

    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        all(target_os = "linux", not(target_env = "musl"))
    ))]
    #[test]
    fn missing_version_symbol_is_a_failure() {
        let target = SYSTEM_LIBRARY;

        let result = load_library(target.to_owned(), Some("remedia_no_such_symbol".to_owned()));

        assert!(!result.ok);
    }

    #[cfg(windows)]
    #[test]
    fn relative_names_are_never_searched() {
        let result = load_library("avcodec-63.dll".to_owned(), None);

        assert!(!result.ok);
    }

    #[cfg(windows)]
    #[test]
    fn system_dll_loads_by_absolute_path() {
        let _guard = serial();

        let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_owned());
        let target = format!("{root}\\System32\\kernel32.dll");

        let result = load_library(target, Some("GetVersion".to_owned()));

        assert!(result.ok, "{:?}", result.message);
    }
}
