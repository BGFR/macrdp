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
    pub fn new(desktop_width: u16, desktop_height: u16) -> anyhow::Result<Self> {
        #[cfg(target_os = "macos")]
        let inner = macos::Inner::new()?;
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
        self.inner.mouse(event, self.desktop_width, self.desktop_height);
        #[cfg(not(target_os = "macos"))]
        trace!(?event, "mouse event (stub)");
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use anyhow::{anyhow, Result};
    use core_graphics::display::CGDisplay;
    use core_graphics::event::{
        CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, ScrollEventUnit,
    };
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use core_graphics::geometry::CGPoint;
    use ironrdp_server::{KeyboardEvent, MouseEvent};
    use tracing::{trace, warn};

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
        screen_width_pts: f64,
        screen_height_pts: f64,
    }

    impl Inner {
        pub fn new() -> Result<Self> {
            // HIDSystemState merges our events into the global HID stream so
            // they look like real hardware input (correct for an RDP server).
            let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
                .map_err(|_| anyhow!("CGEventSource::new failed"))?;
            let main = CGDisplay::main();
            Ok(Self {
                source,
                last_x: 0.0,
                last_y: 0.0,
                left_down: false,
                right_down: false,
                middle_down: false,
                flags: CGEventFlags::CGEventFlagNull,
                screen_width_pts: main.pixels_wide() as f64,
                screen_height_pts: main.pixels_high() as f64,
            })
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
            // Scale desktop coords → screen points. ScreenCaptureKit captures at
            // the display's full pixel resolution, but CGEvent takes points (which
            // equal pixels on non-Retina and equal pixels/scale on Retina).
            // We rely on the screen being the same aspect ratio as the desktop.
            let sx = f64::from(x) * self.screen_width_pts / f64::from(desktop_w.max(1));
            let sy = f64::from(y) * self.screen_height_pts / f64::from(desktop_h.max(1));
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
            let Ok(ev) = CGEvent::new_mouse_event(
                self.source.clone(),
                etype,
                CGPoint::new(self.last_x, self.last_y),
                button,
            ) else {
                warn!("CGEvent mouse button create failed");
                return;
            };
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
        if extended {
            return Some(match scancode {
                0x1C => 0x4C, // numpad Enter → kVK_ANSI_KeypadEnter
                0x1D => 0x3E, // right Ctrl → kVK_RightControl
                0x35 => 0x4B, // numpad / → kVK_ANSI_KeypadDivide
                0x38 => 0x3D, // right Alt (Option) → kVK_RightOption
                0x47 => 0x73, // Home → kVK_Home
                0x48 => 0x7E, // Up arrow
                0x49 => 0x74, // PageUp
                0x4B => 0x7B, // Left arrow
                0x4D => 0x7C, // Right arrow
                0x4F => 0x77, // End
                0x50 => 0x7D, // Down arrow
                0x51 => 0x79, // PageDown
                0x52 => 0x72, // Insert (no macOS equivalent — map to Help)
                0x53 => 0x75, // Delete (forward delete)
                0x5B => 0x37, // left GUI / Windows → kVK_Command
                0x5C => 0x36, // right GUI → kVK_RightCommand
                _ => return None,
            });
        }
        Some(match scancode {
            0x01 => 0x35, // Esc
            0x02 => 0x12, // 1
            0x03 => 0x13, // 2
            0x04 => 0x14, // 3
            0x05 => 0x15, // 4
            0x06 => 0x17, // 5
            0x07 => 0x16, // 6
            0x08 => 0x1A, // 7
            0x09 => 0x1C, // 8
            0x0A => 0x19, // 9
            0x0B => 0x1D, // 0
            0x0C => 0x1B, // -
            0x0D => 0x18, // =
            0x0E => 0x33, // Backspace
            0x0F => 0x30, // Tab
            0x10 => 0x0C, // Q
            0x11 => 0x0D, // W
            0x12 => 0x0E, // E
            0x13 => 0x0F, // R
            0x14 => 0x11, // T
            0x15 => 0x10, // Y
            0x16 => 0x20, // U
            0x17 => 0x22, // I
            0x18 => 0x1F, // O
            0x19 => 0x23, // P
            0x1A => 0x21, // [
            0x1B => 0x1E, // ]
            0x1C => 0x24, // Enter
            0x1D => 0x3B, // Left Ctrl
            0x1E => 0x00, // A
            0x1F => 0x01, // S
            0x20 => 0x02, // D
            0x21 => 0x03, // F
            0x22 => 0x05, // G
            0x23 => 0x04, // H
            0x24 => 0x26, // J
            0x25 => 0x28, // K
            0x26 => 0x25, // L
            0x27 => 0x29, // ;
            0x28 => 0x27, // '
            0x29 => 0x32, // `
            0x2A => 0x38, // Left Shift
            0x2B => 0x2A, // backslash
            0x2C => 0x06, // Z
            0x2D => 0x07, // X
            0x2E => 0x08, // C
            0x2F => 0x09, // V
            0x30 => 0x0B, // B
            0x31 => 0x2D, // N
            0x32 => 0x2E, // M
            0x33 => 0x2B, // ,
            0x34 => 0x2F, // .
            0x35 => 0x2C, // /
            0x36 => 0x3C, // Right Shift
            0x37 => 0x43, // numpad *
            0x38 => 0x3A, // Left Alt (Option)
            0x39 => 0x31, // Space
            0x3A => 0x39, // CapsLock
            0x3B => 0x7A, // F1
            0x3C => 0x78, // F2
            0x3D => 0x63, // F3
            0x3E => 0x76, // F4
            0x3F => 0x60, // F5
            0x40 => 0x61, // F6
            0x41 => 0x62, // F7
            0x42 => 0x64, // F8
            0x43 => 0x65, // F9
            0x44 => 0x6D, // F10
            0x47 => 0x59, // numpad 7
            0x48 => 0x5B, // numpad 8
            0x49 => 0x5C, // numpad 9
            0x4A => 0x4E, // numpad -
            0x4B => 0x56, // numpad 4
            0x4C => 0x57, // numpad 5
            0x4D => 0x58, // numpad 6
            0x4E => 0x45, // numpad +
            0x4F => 0x53, // numpad 1
            0x50 => 0x54, // numpad 2
            0x51 => 0x55, // numpad 3
            0x52 => 0x52, // numpad 0
            0x53 => 0x41, // numpad .
            0x57 => 0x67, // F11
            0x58 => 0x6F, // F12
            _ => return None,
        })
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
