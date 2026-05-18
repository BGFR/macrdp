//! Input forwarding: RDP keyboard/mouse PDUs → macOS CGEvents.
//!
//! `ironrdp_server` hands us scancodes (PS/2 Set 1) and absolute mouse coords
//! in desktop-pixel space. We translate to macOS virtual keycodes and post via
//! `CGEventPost(kCGHIDEventTap)`. Non-macOS targets get a logging stub.

use ironrdp_server::{KeyboardEvent, MouseEvent, RdpServerInputHandler};
#[cfg(not(target_os = "macos"))]
use tracing::trace;

pub struct MacInputHandler {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    desktop_width: u16,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    desktop_height: u16,
    #[cfg(target_os = "macos")]
    inner: macos::Inner,
}

impl MacInputHandler {
    /// `target_display_id` identifies the macOS display we're posting
    /// events into: `None` means the current primary panel; `Some(id)`
    /// is the `CGDirectDisplayID` we should re-query bounds for on
    /// every event. Re-querying matters because the target's global
    /// origin can move *after* this handler is constructed — e.g.
    /// `--detach-primary` disables the built-in panel mid-session,
    /// which shifts the virtual display to `(0, 0)`.
    pub fn new(
        desktop_width: u16,
        desktop_height: u16,
        target_display_id: Option<u32>,
    ) -> anyhow::Result<Self> {
        #[cfg(target_os = "macos")]
        let inner = macos::Inner::new(target_display_id)?;
        #[cfg(not(target_os = "macos"))]
        let _ = target_display_id;
        Ok(Self {
            desktop_width,
            desktop_height,
            #[cfg(target_os = "macos")]
            inner,
        })
    }
}

impl RdpServerInputHandler for MacInputHandler {
    fn keyboard(&mut self, event: KeyboardEvent) {
        #[cfg(target_os = "macos")]
        self.inner.keyboard(event);
        #[cfg(not(target_os = "macos"))]
        trace!(?event, "keyboard event (stub)");
    }

    fn mouse(&mut self, event: MouseEvent) {
        #[cfg(target_os = "macos")]
        self.inner
            .mouse(event, self.desktop_width, self.desktop_height);
        #[cfg(not(target_os = "macos"))]
        trace!(?event, "mouse event (stub)");
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::time::{Duration, Instant};

    use anyhow::{anyhow, Result};
    use core_graphics::display::CGDisplay;
    use core_graphics::event::{
        CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, EventField,
        ScrollEventUnit,
    };
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use core_graphics::geometry::CGPoint;
    use ironrdp_server::{KeyboardEvent, MouseEvent};
    use tracing::{trace, warn};

    /// macOS's default `NSEvent.doubleClickInterval` is 0.5 s. We don't read
    /// the per-user setting (would need a CFPreferences call) — the default
    /// matches what most users have, and using less than the real threshold
    /// just means an aggressive double-click occasionally counts as two
    /// single clicks (the worse alternative is missing all double-clicks).
    const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);
    /// Pixels of cursor movement allowed between consecutive clicks for them
    /// to still count as a multi-click. macOS's slop is a few px; 5 is safe.
    const DOUBLE_CLICK_SLOP_PX: f64 = 5.0;

    #[derive(Clone, Copy)]
    struct ClickState {
        time: Instant,
        x: f64,
        y: f64,
        count: i64,
    }

    // CGEventSource wraps a thread-safe Core Foundation object; Apple documents
    // CF types as safe to send between threads. The crate doesn't impl Send
    // because the raw NonNull pointer isn't, but our usage is single-threaded
    // anyway (RdpServer serializes input callbacks).
    unsafe impl Send for Inner {}

    pub struct Inner {
        source: CGEventSource,
        last_x: f64,
        last_y: f64,
        left_down: bool,
        right_down: bool,
        middle_down: bool,
        flags: CGEventFlags,
        // `None` → use CGDisplay::main() bounds; `Some(id)` → look up
        // that specific display. Re-queried per mouse move so a
        // mid-session bounds change (e.g. --detach-primary disabling
        // the built-in mid-flight) doesn't strand events on stale
        // coords.
        target_display_id: Option<u32>,
        // Per-button click history so a quick second press at (roughly) the
        // same spot becomes click_count=2 and Finder recognises a double-
        // click. Without this every press has click_count=1 implicitly and
        // double-click actions never fire.
        click_left: Option<ClickState>,
        click_right: Option<ClickState>,
        click_middle: Option<ClickState>,
    }

    impl Inner {
        pub fn new(target_display_id: Option<u32>) -> Result<Self> {
            // HIDSystemState merges our events into the global HID stream so
            // they look like real hardware input (correct for an RDP server).
            let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
                .map_err(|_| anyhow!("CGEventSource::new failed"))?;
            Ok(Self {
                source,
                last_x: 0.0,
                last_y: 0.0,
                left_down: false,
                right_down: false,
                middle_down: false,
                flags: CGEventFlags::CGEventFlagNull,
                target_display_id,
                click_left: None,
                click_right: None,
                click_middle: None,
            })
        }

        /// Current global-coord bounds of the target display, queried
        /// fresh so a layout change since this handler was built (vd
        /// moving to (0,0) when --detach-primary disables the built-in
        /// panel) doesn't leave us posting to a stale rectangle.
        fn target_bounds(&self) -> (f64, f64, f64, f64) {
            let d = match self.target_display_id {
                Some(id) => CGDisplay::new(id),
                None => CGDisplay::main(),
            };
            let b = d.bounds();
            (b.origin.x, b.origin.y, b.size.width, b.size.height)
        }

        pub fn keyboard(&mut self, event: KeyboardEvent) {
            match event {
                KeyboardEvent::Pressed { code, extended } => self.key(code, extended, true),
                KeyboardEvent::Released { code, extended } => self.key(code, extended, false),
                KeyboardEvent::UnicodePressed(c) => self.unicode(c, true),
                KeyboardEvent::UnicodeReleased(c) => self.unicode(c, false),
                KeyboardEvent::Synchronize(_) => {}
            }
        }

        fn key(&mut self, scancode: u8, extended: bool, down: bool) {
            let Some(vk) = scancode_to_cgkeycode(scancode, extended) else {
                trace!(scancode, extended, "unmapped scancode");
                return;
            };
            // Track modifier state so CGEvent's flags reflect held modifiers.
            // macOS keyboard events need accurate flags for shift/cmd to take effect.
            let flag = modifier_flag(vk);
            if let Some(f) = flag {
                if down {
                    self.flags |= f;
                } else {
                    self.flags -= f;
                }
            }

            let Ok(ev) = CGEvent::new_keyboard_event(self.source.clone(), vk, down) else {
                warn!(vk, down, "CGEvent::new_keyboard_event failed");
                return;
            };
            ev.set_flags(self.flags);
            ev.post(CGEventTapLocation::HID);
        }

        fn unicode(&self, c: u16, down: bool) {
            // For unicode, send a "null" keycode and set the string. Only fire
            // on key-down — Mac doesn't have a release-side for typed text.
            if !down {
                return;
            }
            let Ok(ev) = CGEvent::new_keyboard_event(self.source.clone(), 0, true) else {
                warn!("unicode CGEvent create failed");
                return;
            };
            ev.set_string_from_utf16_unchecked(&[c]);
            ev.post(CGEventTapLocation::HID);
        }

        pub fn mouse(&mut self, event: MouseEvent, desktop_w: u16, desktop_h: u16) {
            match event {
                MouseEvent::Move { x, y } => self.move_to(x, y, desktop_w, desktop_h),
                MouseEvent::LeftPressed => self.button(CGMouseButton::Left, true),
                MouseEvent::LeftReleased => self.button(CGMouseButton::Left, false),
                MouseEvent::RightPressed => self.button(CGMouseButton::Right, true),
                MouseEvent::RightReleased => self.button(CGMouseButton::Right, false),
                MouseEvent::MiddlePressed => self.button(CGMouseButton::Center, true),
                MouseEvent::MiddleReleased => self.button(CGMouseButton::Center, false),
                MouseEvent::VerticalScroll { value } => self.scroll(i32::from(value), 0),
                MouseEvent::Scroll { x, y } => self.scroll(y, x),
                MouseEvent::Button4Pressed
                | MouseEvent::Button4Released
                | MouseEvent::Button5Pressed
                | MouseEvent::Button5Released => {
                    trace!(?event, "extra mouse buttons not implemented");
                }
                MouseEvent::RelMove { .. } => {
                    trace!(?event, "relative move not implemented");
                }
            }
        }

        fn move_to(&mut self, x: u16, y: u16, desktop_w: u16, desktop_h: u16) {
            // Scale desktop coords → screen points, then translate into the
            // target display's slot in the global coord space. The origin
            // offset is what makes CGEventPost route events to a non-primary
            // display (virtual or external) — the WindowServer dispatches by
            // which display contains the global coord.
            let (ox, oy, sw, sh) = self.target_bounds();
            let sx = ox + f64::from(x) * sw / f64::from(desktop_w.max(1));
            let sy = oy + f64::from(y) * sh / f64::from(desktop_h.max(1));
            self.last_x = sx;
            self.last_y = sy;

            let etype = if self.left_down {
                CGEventType::LeftMouseDragged
            } else if self.right_down {
                CGEventType::RightMouseDragged
            } else if self.middle_down {
                CGEventType::OtherMouseDragged
            } else {
                CGEventType::MouseMoved
            };
            let button = if self.middle_down {
                CGMouseButton::Center
            } else if self.right_down {
                CGMouseButton::Right
            } else {
                CGMouseButton::Left
            };
            let Ok(ev) =
                CGEvent::new_mouse_event(self.source.clone(), etype, CGPoint::new(sx, sy), button)
            else {
                return;
            };
            ev.post(CGEventTapLocation::HID);
        }

        fn button(&mut self, button: CGMouseButton, down: bool) {
            let etype = match (button, down) {
                (CGMouseButton::Left, true) => CGEventType::LeftMouseDown,
                (CGMouseButton::Left, false) => CGEventType::LeftMouseUp,
                (CGMouseButton::Right, true) => CGEventType::RightMouseDown,
                (CGMouseButton::Right, false) => CGEventType::RightMouseUp,
                (CGMouseButton::Center, true) => CGEventType::OtherMouseDown,
                (CGMouseButton::Center, false) => CGEventType::OtherMouseUp,
            };
            match button {
                CGMouseButton::Left => self.left_down = down,
                CGMouseButton::Right => self.right_down = down,
                CGMouseButton::Center => self.middle_down = down,
            }

            // Compute the click count: increment on a down event close in
            // time + space to the previous click; reset to 1 otherwise. The
            // matching up event reuses the count from the most recent down
            // so Finder sees a paired {down, up} with identical click_state.
            let state_slot = match button {
                CGMouseButton::Left => &mut self.click_left,
                CGMouseButton::Right => &mut self.click_right,
                CGMouseButton::Center => &mut self.click_middle,
            };
            let click_count = if down {
                let now = Instant::now();
                let count = match *state_slot {
                    Some(prev)
                        if now.duration_since(prev.time) <= DOUBLE_CLICK_INTERVAL
                            && (self.last_x - prev.x).abs() <= DOUBLE_CLICK_SLOP_PX
                            && (self.last_y - prev.y).abs() <= DOUBLE_CLICK_SLOP_PX =>
                    {
                        prev.count + 1
                    }
                    _ => 1,
                };
                *state_slot = Some(ClickState {
                    time: now,
                    x: self.last_x,
                    y: self.last_y,
                    count,
                });
                count
            } else {
                state_slot.map(|s| s.count).unwrap_or(1)
            };

            let Ok(ev) = CGEvent::new_mouse_event(
                self.source.clone(),
                etype,
                CGPoint::new(self.last_x, self.last_y),
                button,
            ) else {
                warn!("CGEvent mouse button create failed");
                return;
            };
            ev.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, click_count);
            ev.post(CGEventTapLocation::HID);
        }

        fn scroll(&self, vertical: i32, horizontal: i32) {
            // RDP wheel rotation units are 120 per "tick"; macOS wheel deltas
            // are unit-less but ~1-3 per tick feels normal. Divide by 40.
            let v = (vertical / 40).clamp(-10, 10);
            let h = (horizontal / 40).clamp(-10, 10);
            let Ok(ev) =
                CGEvent::new_scroll_event(self.source.clone(), ScrollEventUnit::LINE, 2, v, h, 0)
            else {
                return;
            };
            ev.post(CGEventTapLocation::HID);
        }
    }

    /// PS/2 Set 1 scancode → macOS virtual keycode (US ANSI layout).
    ///
    /// Reference scancodes: SDL2_scancode.h, kbdscan.txt. Reference virtual
    /// keycodes: <HIToolbox/Events.h> (kVK_*). Only the common US keys are
    /// mapped — non-US layouts, media keys, and IME need follow-on work.
    fn scancode_to_cgkeycode(scancode: u8, extended: bool) -> Option<u16> {
        let entry = if extended {
            SCANCODE_EXTENDED[scancode as usize]
        } else {
            SCANCODE_NORMAL[scancode as usize]
        };
        if entry == NONE {
            None
        } else {
            Some(entry)
        }
    }

    // Sentinel for "no mapping" — 0xFFFF is well outside the kVK_* range, so we
    // can pack the table as a flat `[u16; 256]` and avoid the niche-less
    // `Option<u16>` representation (which would still work, just larger and
    // slightly worse for branch prediction).
    const NONE: u16 = 0xFFFF;
    const SCANCODE_NORMAL: [u16; 256] = build_scancode_normal();
    const SCANCODE_EXTENDED: [u16; 256] = build_scancode_extended();

    const fn build_scancode_normal() -> [u16; 256] {
        let mut t = [NONE; 256];
        t[0x01] = 0x35; // Esc
        t[0x02] = 0x12; // 1
        t[0x03] = 0x13; // 2
        t[0x04] = 0x14; // 3
        t[0x05] = 0x15; // 4
        t[0x06] = 0x17; // 5
        t[0x07] = 0x16; // 6
        t[0x08] = 0x1A; // 7
        t[0x09] = 0x1C; // 8
        t[0x0A] = 0x19; // 9
        t[0x0B] = 0x1D; // 0
        t[0x0C] = 0x1B; // -
        t[0x0D] = 0x18; // =
        t[0x0E] = 0x33; // Backspace
        t[0x0F] = 0x30; // Tab
        t[0x10] = 0x0C; // Q
        t[0x11] = 0x0D; // W
        t[0x12] = 0x0E; // E
        t[0x13] = 0x0F; // R
        t[0x14] = 0x11; // T
        t[0x15] = 0x10; // Y
        t[0x16] = 0x20; // U
        t[0x17] = 0x22; // I
        t[0x18] = 0x1F; // O
        t[0x19] = 0x23; // P
        t[0x1A] = 0x21; // [
        t[0x1B] = 0x1E; // ]
        t[0x1C] = 0x24; // Enter
        t[0x1D] = 0x3B; // Left Ctrl
        t[0x1E] = 0x00; // A
        t[0x1F] = 0x01; // S
        t[0x20] = 0x02; // D
        t[0x21] = 0x03; // F
        t[0x22] = 0x05; // G
        t[0x23] = 0x04; // H
        t[0x24] = 0x26; // J
        t[0x25] = 0x28; // K
        t[0x26] = 0x25; // L
        t[0x27] = 0x29; // ;
        t[0x28] = 0x27; // '
        t[0x29] = 0x32; // `
        t[0x2A] = 0x38; // Left Shift
        t[0x2B] = 0x2A; // backslash
        t[0x2C] = 0x06; // Z
        t[0x2D] = 0x07; // X
        t[0x2E] = 0x08; // C
        t[0x2F] = 0x09; // V
        t[0x30] = 0x0B; // B
        t[0x31] = 0x2D; // N
        t[0x32] = 0x2E; // M
        t[0x33] = 0x2B; // ,
        t[0x34] = 0x2F; // .
        t[0x35] = 0x2C; // /
        t[0x36] = 0x3C; // Right Shift
        t[0x37] = 0x43; // numpad *
        t[0x38] = 0x3A; // Left Alt (Option)
        t[0x39] = 0x31; // Space
        t[0x3A] = 0x39; // CapsLock
        t[0x3B] = 0x7A; // F1
        t[0x3C] = 0x78; // F2
        t[0x3D] = 0x63; // F3
        t[0x3E] = 0x76; // F4
        t[0x3F] = 0x60; // F5
        t[0x40] = 0x61; // F6
        t[0x41] = 0x62; // F7
        t[0x42] = 0x64; // F8
        t[0x43] = 0x65; // F9
        t[0x44] = 0x6D; // F10
        t[0x47] = 0x59; // numpad 7
        t[0x48] = 0x5B; // numpad 8
        t[0x49] = 0x5C; // numpad 9
        t[0x4A] = 0x4E; // numpad -
        t[0x4B] = 0x56; // numpad 4
        t[0x4C] = 0x57; // numpad 5
        t[0x4D] = 0x58; // numpad 6
        t[0x4E] = 0x45; // numpad +
        t[0x4F] = 0x53; // numpad 1
        t[0x50] = 0x54; // numpad 2
        t[0x51] = 0x55; // numpad 3
        t[0x52] = 0x52; // numpad 0
        t[0x53] = 0x41; // numpad .
        t[0x57] = 0x67; // F11
        t[0x58] = 0x6F; // F12
        t
    }

    const fn build_scancode_extended() -> [u16; 256] {
        let mut t = [NONE; 256];
        t[0x1C] = 0x4C; // numpad Enter → kVK_ANSI_KeypadEnter
        t[0x1D] = 0x3E; // right Ctrl → kVK_RightControl
        t[0x35] = 0x4B; // numpad / → kVK_ANSI_KeypadDivide
        t[0x38] = 0x3D; // right Alt (Option) → kVK_RightOption
        t[0x47] = 0x73; // Home → kVK_Home
        t[0x48] = 0x7E; // Up arrow
        t[0x49] = 0x74; // PageUp
        t[0x4B] = 0x7B; // Left arrow
        t[0x4D] = 0x7C; // Right arrow
        t[0x4F] = 0x77; // End
        t[0x50] = 0x7D; // Down arrow
        t[0x51] = 0x79; // PageDown
        t[0x52] = 0x72; // Insert (no macOS equivalent — map to Help)
        t[0x53] = 0x75; // Delete (forward delete)
        t[0x5B] = 0x37; // left GUI / Windows → kVK_Command
        t[0x5C] = 0x36; // right GUI → kVK_RightCommand
        t
    }

    fn modifier_flag(vk: u16) -> Option<CGEventFlags> {
        Some(match vk {
            0x38 | 0x3C => CGEventFlags::CGEventFlagShift,
            0x3B | 0x3E => CGEventFlags::CGEventFlagControl,
            0x3A | 0x3D => CGEventFlags::CGEventFlagAlternate,
            0x37 | 0x36 => CGEventFlags::CGEventFlagCommand,
            0x39 => CGEventFlags::CGEventFlagAlphaShift,
            _ => return None,
        })
    }
}

/// Probe whether this process has Accessibility (AX) permission, prompting if
/// not. Without it, posted CGEvents are silently dropped by the WindowServer.
#[cfg(target_os = "macos")]
pub fn ensure_accessibility_access() -> bool {
    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::CFString;
    use std::os::raw::c_void;

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
    }

    // kAXTrustedCheckOptionPrompt — passing true makes macOS surface the
    // "Allow X to control this computer" prompt the first time we ask.
    let key = CFString::from_static_string("AXTrustedCheckOptionPrompt");
    let value = CFBoolean::true_value();
    let opts = CFDictionary::from_CFType_pairs(&[(key, value)]);
    unsafe { AXIsProcessTrustedWithOptions(opts.as_concrete_TypeRef().cast()) }
}

#[cfg(not(target_os = "macos"))]
pub fn ensure_accessibility_access() -> bool {
    true
}
