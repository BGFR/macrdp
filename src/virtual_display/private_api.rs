//! All touches to undocumented CoreGraphics private API live here.
//!
//! When Apple renames a symbol or restructures `CGVirtualDisplay*` in a
//! future macOS, this is the only file that should need to change. The
//! public side of the module (in `mod.rs`) talks to this layer through
//! stable Rust types only — no `*mut AnyObject`, no `dlsym`, no
//! private class names leak out.
//!
//! Maintenance notes:
//! - Function pointers are loaded via `dlsym(RTLD_DEFAULT, ...)` at
//!   `PrivateApi::load()`. A missing symbol surfaces as an Err string
//!   identifying the symbol; we never panic on private-API absence.
//! - Obj-C classes are looked up via `AnyClass::get(...)`; same fallback.
//! - Reference: BetterDisplay's Swift wrappers are the most current
//!   public source-of-truth for this private API. If something drifts,
//!   diff against that project.

#![cfg(target_os = "macos")]

use std::ffi::{c_void, CString};
use std::ptr::null_mut;

use anyhow::{anyhow, Result};
use objc2::msg_send;
use objc2::runtime::{AnyClass, AnyObject};
use objc2_foundation::{CGSize, NSString};

type CGDirectDisplayID = u32;

/// Runtime-loaded private CoreGraphics C functions. Held by value so
/// the public layer can call them without re-resolving on every use.
pub(super) struct PrivateApi {
    create: unsafe extern "C" fn(*mut AnyObject) -> *mut c_void,
    apply_settings: unsafe extern "C" fn(*mut c_void, *mut AnyObject) -> bool,
    get_display_id: unsafe extern "C" fn(*mut c_void) -> CGDirectDisplayID,
    cf_release: unsafe extern "C" fn(*mut c_void),
}

impl PrivateApi {
    /// Resolve every required symbol up front. Either all succeed or
    /// we return an error naming the first missing one — we never want
    /// to be halfway loaded.
    pub(super) fn load() -> Result<Self> {
        unsafe {
            // RTLD_DEFAULT is documented as -2 on Darwin: dlsym searches
            // all libraries already loaded into the process. CoreGraphics
            // is pulled in by the core-graphics crate, so its symbols
            // (public and private) are resolvable without dlopen.
            let rtld_default = -2isize as *mut c_void;
            let create = dlsym_required(rtld_default, "CGVirtualDisplayCreate")?;
            let apply = dlsym_required(rtld_default, "CGVirtualDisplayApplySettings")?;
            let get_id = dlsym_required(rtld_default, "CGVirtualDisplayGetDisplayID")?;
            let release = dlsym_required(rtld_default, "CFRelease")?;
            Ok(Self {
                create: std::mem::transmute::<*mut c_void, _>(create),
                apply_settings: std::mem::transmute::<*mut c_void, _>(apply),
                get_display_id: std::mem::transmute::<*mut c_void, _>(get_id),
                cf_release: std::mem::transmute::<*mut c_void, _>(release),
            })
        }
    }
}

unsafe fn dlsym_required(handle: *mut c_void, name: &str) -> Result<*mut c_void> {
    let c = CString::new(name).expect("symbol name has no interior NUL");
    let p = libc::dlsym(handle, c.as_ptr());
    if p.is_null() {
        Err(anyhow!(
            "private CoreGraphics symbol `{name}` not found — virtual-display \
             support is unavailable on this macOS (Apple may have renamed or \
             removed the symbol; check BetterDisplay's source for the new name)"
        ))
    } else {
        Ok(p)
    }
}

fn class_required(name: &str) -> Result<&'static AnyClass> {
    AnyClass::get(name).ok_or_else(|| {
        anyhow!(
            "private Obj-C class `{name}` not registered in the runtime — \
             virtual-display support is unavailable on this macOS"
        )
    })
}

/// Opaque CG handle; releases via CFRelease on Drop so callers don't
/// have to remember.
pub(super) struct Handle {
    raw: *mut c_void,
    cf_release: unsafe extern "C" fn(*mut c_void),
    display_id: CGDirectDisplayID,
}

// SAFETY: the underlying CF object is documented thread-safe; our usage
// is single-threaded but the wrapper appears in Send contexts in main.
unsafe impl Send for Handle {}

impl Handle {
    pub(super) fn display_id(&self) -> u32 {
        self.display_id
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe { (self.cf_release)(self.raw) };
            self.raw = null_mut();
        }
    }
}

/// Allocate a virtual display at the requested pixel size, apply a
/// single-mode settings object, and return the CG handle + displayID.
///
/// The descriptor / mode / settings / modes-NSArray temporaries are
/// intentionally leaked: they're tiny (a few hundred bytes total),
/// only created once per process, and avoiding manual `release` calls
/// keeps the code obviously free of double-free hazards.
pub(super) fn create(
    api: &PrivateApi,
    width: u32,
    height: u32,
    refresh_hz: u32,
    name: &str,
) -> Result<Handle> {
    let desc_class = class_required("CGVirtualDisplayDescriptor")?;
    let settings_class = class_required("CGVirtualDisplaySettings")?;
    let mode_class = class_required("CGVirtualDisplayMode")?;
    let nsarray_class = class_required("NSArray")?;

    let ns_name = NSString::from_str(name);

    unsafe {
        // 1. Descriptor: alloc/init, then setters for every property
        //    Displays.app shows for an attached monitor.
        let desc: *mut AnyObject = msg_send![desc_class, alloc];
        let desc: *mut AnyObject = msg_send![desc, init];
        if desc.is_null() {
            return Err(anyhow!("CGVirtualDisplayDescriptor init returned nil"));
        }
        let _: () = msg_send![desc, setName: &*ns_name];
        let _: () = msg_send![desc, setMaxPixelsWide: width];
        let _: () = msg_send![desc, setMaxPixelsHigh: height];
        // Physical size is cosmetic — System Settings just uses it for
        // the "X inch display" label. 600x338mm ≈ 27-inch 16:9.
        let _: () = msg_send![desc, setSizeInMillimeters: CGSize { width: 600.0, height: 338.0 }];
        let _: () = msg_send![desc, setProductID: 0x6D616372u32]; // "macr"
        let _: () = msg_send![desc, setVendorID: 0x6D616372u32];
        let _: () = msg_send![desc, setSerialNum: 1u32];

        // 2. Create the CG-side handle.
        let raw: *mut c_void = (api.create)(desc);
        if raw.is_null() {
            return Err(anyhow!(
                "CGVirtualDisplayCreate returned null — descriptor was \
                 rejected by the system"
            ));
        }

        // 3. One mode at the requested resolution, then a settings object
        //    holding that single-mode array.
        let mode: *mut AnyObject = msg_send![mode_class, alloc];
        let mode: *mut AnyObject = msg_send![
            mode,
            initWithWidth: width,
            height: height,
            refreshRate: f64::from(refresh_hz),
        ];
        if mode.is_null() {
            (api.cf_release)(raw);
            return Err(anyhow!("CGVirtualDisplayMode init returned nil"));
        }
        let modes: *mut AnyObject = msg_send![nsarray_class, arrayWithObject: mode];

        let settings: *mut AnyObject = msg_send![settings_class, alloc];
        let settings: *mut AnyObject = msg_send![settings, init];
        if settings.is_null() {
            (api.cf_release)(raw);
            return Err(anyhow!("CGVirtualDisplaySettings init returned nil"));
        }
        let _: () = msg_send![settings, setHiDPI: 0u32];
        let _: () = msg_send![settings, setModes: modes];

        // 4. Apply.
        let ok: bool = (api.apply_settings)(raw, settings);
        if !ok {
            (api.cf_release)(raw);
            return Err(anyhow!(
                "CGVirtualDisplayApplySettings returned false — mode may be \
                 unsupported (try a different resolution or refresh rate)"
            ));
        }

        // 5. Resolve displayID.
        let display_id = (api.get_display_id)(raw);
        if display_id == 0 {
            (api.cf_release)(raw);
            return Err(anyhow!(
                "CGVirtualDisplayGetDisplayID returned 0 — display registered \
                 but the system didn't assign a directDisplayID"
            ));
        }

        Ok(Handle {
            raw,
            cf_release: api.cf_release,
            display_id,
        })
    }
}
