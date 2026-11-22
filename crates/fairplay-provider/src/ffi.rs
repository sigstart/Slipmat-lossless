// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! `libdrm_client.so`, opened at run time.
//!
//! Loading it with `dlopen` rather than linking it means the workspace builds,
//! lints and tests on machines that never built the C library. The library is
//! never closed: libhybris cannot be unloaded from a running process.

use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::path::Path;

use anyhow::{Result, anyhow};

/// `void (*)(const char *type, char *out_buf, int out_size, void *ud)`.
pub type AuthCallback = unsafe extern "C" fn(*const c_char, *mut c_char, c_int, *mut c_void);

type BackendInit = unsafe extern "C" fn(*const c_char, *const c_char, *const c_char) -> c_int;
type FairplayInit = unsafe extern "C" fn(
    *const c_char,
    *const c_char,
    *const c_char,
    *const c_char,
    *const c_char,
    Option<AuthCallback>,
    *mut c_void,
) -> c_int;
type GetAccount = unsafe extern "C" fn() -> *mut c_char;
type OpenKeyContext =
    unsafe extern "C" fn(*const c_char, *const c_char, *mut c_uint) -> *mut c_void;
type Decrypt = unsafe extern "C" fn(*mut c_void, c_uint, *mut u8, u32) -> c_int;
type RefreshKeys = unsafe extern "C" fn() -> c_int;

/// `hybris_backend_decrypt`: a lease refresh freed the key context.
pub const STALE_CONTEXT: c_int = -2;

#[derive(Clone, Copy)]
pub struct Library {
    pub backend_init: BackendInit,
    pub fairplay_init: FairplayInit,
    pub get_account: GetAccount,
    pub open_key_context: OpenKeyContext,
    pub decrypt: Decrypt,
    pub refresh_keys: RefreshKeys,
}

impl Library {
    pub fn open(path: &Path) -> Result<Self> {
        let path = CString::new(path.as_os_str().as_encoded_bytes())?;
        // RTLD_DEEPBIND: the library calls its own `login`, and glibc exports
        // one too. Linked, the library's would win; loaded later, glibc's is
        // already in the global scope and would be called instead. RTLD_GLOBAL
        // keeps the rest as linking would, for the Android loader's lookups.
        // SAFETY: `path` is NUL-terminated. Running the library's constructors
        // is the point of loading it.
        let handle = unsafe {
            libc::dlopen(
                path.as_ptr(),
                libc::RTLD_NOW | libc::RTLD_GLOBAL | libc::RTLD_DEEPBIND,
            )
        };
        if handle.is_null() {
            return Err(anyhow!("the FairPlay library could not be loaded"));
        }
        // SAFETY: each type matches the declaration in `drm_hybris.h` or
        // `native/drm_lib.h`.
        unsafe {
            Ok(Self {
                backend_init: symbol(handle, c"hybris_backend_init")?,
                fairplay_init: symbol(handle, c"hybris_fairplay_init")?,
                get_account: symbol(handle, c"drm_lib_get_account")?,
                open_key_context: symbol(handle, c"hybris_open_kd_ctx_from_uri")?,
                decrypt: symbol(handle, c"hybris_backend_decrypt")?,
                refresh_keys: symbol(handle, c"hybris_refresh_keys")?,
            })
        }
    }
}

/// # Safety
///
/// `T` must be the function pointer type of the symbol called `name`.
unsafe fn symbol<T: Copy>(handle: *mut c_void, name: &CStr) -> Result<T> {
    const { assert!(size_of::<T>() == size_of::<*mut c_void>()) };
    // SAFETY: `handle` came from a successful `dlopen`.
    let address = unsafe { libc::dlsym(handle, name.as_ptr()) };
    if address.is_null() {
        return Err(anyhow!("the FairPlay library has no {name:?}"));
    }
    // SAFETY: the caller names the symbol's type, and the size is asserted.
    Ok(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&address) })
}

/// Copies a string the library returned with `malloc`, zeroises it and frees it.
///
/// # Safety
///
/// `raw` must be null or a NUL-terminated string allocated with `malloc`.
pub unsafe fn take_malloced(raw: *mut c_char) -> Option<zeroize::Zeroizing<Vec<u8>>> {
    if raw.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees a NUL-terminated allocation.
    let bytes = unsafe { CStr::from_ptr(raw) }.to_bytes();
    let copy = zeroize::Zeroizing::new(bytes.to_vec());
    // SAFETY: `raw` owns `bytes.len()` bytes before its NUL, and nothing else
    // refers to it once it is freed.
    unsafe {
        std::ptr::write_bytes(raw, 0, bytes.len());
        libc::free(raw.cast());
    }
    Some(copy)
}

/// A NUL-terminated copy of a secret, zeroised on drop.
pub struct SecretCString(zeroize::Zeroizing<Vec<u8>>);

impl SecretCString {
    /// `None` when the text holds a NUL, which C would silently truncate at.
    pub fn new(text: &str) -> Option<Self> {
        if text.as_bytes().contains(&0) {
            return None;
        }
        let mut bytes = zeroize::Zeroizing::new(Vec::with_capacity(text.len() + 1));
        bytes.extend_from_slice(text.as_bytes());
        bytes.push(0);
        Some(Self(bytes))
    }

    pub fn as_ptr(&self) -> *const c_char {
        self.0.as_ptr().cast()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_library_is_an_error_not_a_panic() {
        assert!(Library::open(Path::new("/nonexistent/libdrm_client.so")).is_err());
    }

    #[test]
    fn a_library_without_the_symbols_is_refused() {
        assert!(Library::open(Path::new("libc.so.6")).is_err());
    }

    #[test]
    fn secret_strings_refuse_interior_nuls() {
        assert!(SecretCString::new("pass\0word").is_none());
        let secret = SecretCString::new("password").unwrap();
        // SAFETY: `SecretCString` is NUL-terminated.
        assert_eq!(unsafe { CStr::from_ptr(secret.as_ptr()) }, c"password");
    }

    #[test]
    fn malloced_strings_are_copied_and_freed() {
        // SAFETY: strdup returns a malloc'd copy of a NUL-terminated string.
        let raw = unsafe { libc::strdup(c"{\"x\":1}".as_ptr()) };
        // SAFETY: `raw` is from strdup.
        let copy = unsafe { take_malloced(raw) }.unwrap();
        assert_eq!(copy.as_slice(), b"{\"x\":1}");
        // SAFETY: null is accepted.
        assert!(unsafe { take_malloced(std::ptr::null_mut()) }.is_none());
    }
}
