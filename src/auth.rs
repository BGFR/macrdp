//! Validate (username, password) against the macOS account database via PAM.
//!
//! We use the `checkpw` PAM service (auth + account, no session) so we don't
//! need root or special entitlements. The expected `username` is whatever the
//! user provided on the command line or defaulted to via `whoami`.
//!
//! On success the returned password is the verified Mac password; we hand it
//! to `ironrdp_server::RdpServer::set_credentials` so the per-connection
//! `ClientInfoPdu` comparison passes for clients that supply the same creds.

#[cfg(target_os = "macos")]
pub fn authenticate(username: &str, password: &str) -> anyhow::Result<()> {
    pam_impl::authenticate("checkpw", username, password)
}

#[cfg(not(target_os = "macos"))]
pub fn authenticate(_username: &str, _password: &str) -> anyhow::Result<()> {
    // On non-macOS targets we don't gate at startup; the protocol layer is
    // the only thing we can compile-test there.
    Ok(())
}

#[cfg(target_os = "macos")]
mod pam_impl {
    use std::ffi::{c_char, c_int, c_void, CStr, CString};
    use std::ptr;

    use anyhow::{anyhow, bail, Result};

    // libpam typedefs (see /usr/include/pam/pam_appl.h on macOS).
    #[repr(C)]
    struct PamMessage {
        msg_style: c_int,
        msg: *const c_char,
    }

    #[repr(C)]
    struct PamResponse {
        resp: *mut c_char,
        resp_retcode: c_int,
    }

    #[repr(C)]
    struct PamConv {
        conv: extern "C" fn(
            num_msg: c_int,
            msg: *mut *const PamMessage,
            resp: *mut *mut PamResponse,
            appdata_ptr: *mut c_void,
        ) -> c_int,
        appdata_ptr: *mut c_void,
    }

    const PAM_SUCCESS: c_int = 0;
    // Asks for the password (echo off). Used to know when to return the
    // stored password as the response.
    const PAM_PROMPT_ECHO_OFF: c_int = 1;
    // pam_set_item key for the password (PAM_AUTHTOK = 6 on macOS / OpenPAM).
    // /etc/pam.d/checkpw uses `use_first_pass`, so pam_opendirectory expects
    // the authtok already set on the handle and never calls our conv.
    const PAM_AUTHTOK: c_int = 6;

    #[link(name = "pam")]
    extern "C" {
        fn pam_start(
            service: *const c_char,
            user: *const c_char,
            conv: *const PamConv,
            handle: *mut *mut c_void,
        ) -> c_int;
        fn pam_set_item(handle: *mut c_void, item_type: c_int, item: *const c_void) -> c_int;
        fn pam_authenticate(handle: *mut c_void, flags: c_int) -> c_int;
        fn pam_acct_mgmt(handle: *mut c_void, flags: c_int) -> c_int;
        fn pam_end(handle: *mut c_void, status: c_int) -> c_int;
        fn pam_strerror(handle: *mut c_void, errnum: c_int) -> *const c_char;
    }

    /// PAM conversation callback. The `checkpw` service uses `use_first_pass`,
    /// so pam_opendirectory normally reads the password from PAM_AUTHTOK
    /// (set in `authenticate`) and never invokes this — but other PAM services
    /// we might switch to do prompt, so we keep it real: malloc one
    /// `PamResponse` per message, copying our stored password (`appdata_ptr`
    /// points at a heap-allocated CString). libpam frees both the array and
    /// each `resp` buffer.
    extern "C" fn conv(
        num_msg: c_int,
        msgs: *mut *const PamMessage,
        out_resp: *mut *mut PamResponse,
        appdata_ptr: *mut c_void,
    ) -> c_int {
        if num_msg <= 0 || msgs.is_null() || out_resp.is_null() {
            return 1; // PAM_CONV_ERR
        }
        let arr = unsafe { libc::calloc(num_msg as usize, std::mem::size_of::<PamResponse>()) }
            as *mut PamResponse;
        if arr.is_null() {
            return 1;
        }
        let pw_cstr = unsafe { &*(appdata_ptr as *const CString) };
        for i in 0..(num_msg as isize) {
            let m = unsafe { *msgs.offset(i) };
            if m.is_null() {
                continue;
            }
            let style = unsafe { (*m).msg_style };
            if style == PAM_PROMPT_ECHO_OFF {
                let bytes = pw_cstr.as_bytes_with_nul();
                let buf = unsafe { libc::malloc(bytes.len()) } as *mut c_char;
                if buf.is_null() {
                    unsafe { libc::free(arr as *mut c_void) };
                    return 1;
                }
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        bytes.as_ptr() as *const c_char,
                        buf,
                        bytes.len(),
                    );
                    (*arr.offset(i)).resp = buf;
                    (*arr.offset(i)).resp_retcode = 0;
                }
            }
        }
        unsafe { *out_resp = arr };
        PAM_SUCCESS
    }

    pub fn authenticate(service: &str, username: &str, password: &str) -> Result<()> {
        use zeroize::Zeroizing;

        let service_c = CString::new(service).map_err(|_| anyhow!("service contains NUL"))?;
        let user_c = CString::new(username).map_err(|_| anyhow!("username contains NUL"))?;
        let pw_c = CString::new(password).map_err(|_| anyhow!("password contains NUL"))?;

        let conv_struct = PamConv {
            conv,
            appdata_ptr: &pw_c as *const _ as *mut c_void,
        };

        let mut handle: *mut c_void = ptr::null_mut();
        let rc = unsafe {
            pam_start(
                service_c.as_ptr(),
                user_c.as_ptr(),
                &conv_struct,
                &mut handle,
            )
        };
        if rc != PAM_SUCCESS {
            return Err(anyhow!("pam_start failed: rc={rc}"));
        }

        let set_rc = unsafe { pam_set_item(handle, PAM_AUTHTOK, pw_c.as_ptr() as *const c_void) };
        if set_rc != PAM_SUCCESS {
            unsafe { pam_end(handle, set_rc) };
            return Err(anyhow!("pam_set_item(AUTHTOK) failed: rc={set_rc}"));
        }

        let auth_rc = unsafe { pam_authenticate(handle, 0) };
        let acct_rc = if auth_rc == PAM_SUCCESS {
            unsafe { pam_acct_mgmt(handle, 0) }
        } else {
            auth_rc
        };

        let err = if acct_rc != PAM_SUCCESS {
            let msg_ptr = unsafe { pam_strerror(handle, acct_rc) };
            let msg = if msg_ptr.is_null() {
                "unknown PAM error".to_string()
            } else {
                unsafe { CStr::from_ptr(msg_ptr) }
                    .to_string_lossy()
                    .into_owned()
            };
            Some(msg)
        } else {
            None
        };

        unsafe { pam_end(handle, acct_rc) };

        // Overwrite the password buffer before drop. CString's Drop frees
        // but doesn't zero; converting into its underlying Vec<u8> and
        // wrapping with Zeroizing lets the bytes be wiped at scope exit.
        // Safe ordering: pam_end has already returned, so libpam no longer
        // holds the pointer set via pam_set_item.
        let _wiped_pw = Zeroizing::new(pw_c.into_bytes_with_nul());

        if let Some(msg) = err {
            bail!("authentication failed: {msg}");
        }
        Ok(())
    }
}
