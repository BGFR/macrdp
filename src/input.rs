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
    use std::collections::HashSet;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use anyhow::{anyhow, Result};
    use core_graphics::display::CGDisplay;
    use core_graphics::event::{
        CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, EventField,
        ScrollEventUnit,
    };
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use core_graphics::geometry::CGPoint;
    use ironrdp_pdu::input::fast_path::SynchronizeFlags;
    use ironrdp_server::{KeyboardEvent, MouseEvent};
    use objc2_app_kit::{NSApplicationActivationPolicy, NSWorkspace};
    use tracing::{debug, trace, warn};

    // kVK_* constants we match on for symbolic-hotkey interception.
    const VK_TAB: u16 = 0x30;
    const VK_SPACE: u16 = 0x31;
    const VK_3: u16 = 0x14;
    const VK_4: u16 = 0x15;
    const VK_5: u16 = 0x17;
    const VK_GRAVE: u16 = 0x32;
    const VK_CAPS_LOCK: u16 = 0x39;

    // macOS virtual keycodes for the left/right halves of each modifier.
    // Used by ModifierState to track which physical key is held so we can
    // accurately re-derive both the masked CGEventFlag bits (Shift, Ctrl,
    // …) and the device-dependent NX_DEVICE{L,R}*KEYMASK bits a real
    // keyboard would put on a flagsChanged event.
    const VK_LSHIFT: u16 = 0x38;
    const VK_RSHIFT: u16 = 0x3C;
    const VK_LCTRL: u16 = 0x3B;
    const VK_RCTRL: u16 = 0x3E;
    const VK_LALT: u16 = 0x3A;
    const VK_RALT: u16 = 0x3D;
    const VK_LCMD: u16 = 0x37;
    const VK_RCMD: u16 = 0x36;

    // Device-dependent modifier bits — lifted from <IOKit/hidsystem/IOLLEvent.h>
    // (NX_DEVICE{L,R}{SHIFT,CTL,ALT,CMD}KEYMASK). They sit below the public
    // CGEventFlag bits (which start at 0x0100), so `from_bits_retain` keeps them
    // through CGEventFlags. Apps that distinguish left vs. right modifiers
    // (Karabiner, some games, accessibility tools) read these — a real
    // keyboard's flagsChanged event has them set; ours wouldn't without this.
    const NX_DEVICE_L_CTRL: u64 = 0x0001;
    const NX_DEVICE_L_SHIFT: u64 = 0x0002;
    const NX_DEVICE_R_SHIFT: u64 = 0x0004;
    const NX_DEVICE_L_CMD: u64 = 0x0008;
    const NX_DEVICE_R_CMD: u64 = 0x0010;
    const NX_DEVICE_L_ALT: u64 = 0x0020;
    const NX_DEVICE_R_ALT: u64 = 0x0040;
    const NX_DEVICE_R_CTRL: u64 = 0x2000;

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

    /// Per-side modifier state. We track left/right halves of each modifier
    /// separately for two reasons:
    ///   1) When the user holds *both* left and right Shift and releases
    ///      one, the masked `CGEventFlagShift` bit must stay set. A single
    ///      `flags |= / -= CGEventFlagShift` toggle can't represent that —
    ///      releasing one half wrongly drops the bit while the other half
    ///      is still down.
    ///   2) Real keyboards include device-dependent left/right bits on
    ///      `flagsChanged` events (`NX_DEVICE{L,R}*KEYMASK`). Apps that
    ///      key off left-only vs. right-only modifiers (Karabiner,
    ///      remappers, some games) read those. Without per-side tracking
    ///      we can't produce them.
    ///
    /// `caps_lock` is a *toggle*, not a held-down bool: pressing the key
    /// flips it; the release is a no-op. That matches how real Mac
    /// keyboards report Caps Lock and what Cocoa apps assume when they
    /// check `CGEventFlagAlphaShift`.
    #[derive(Default, Clone, Copy)]
    struct ModifierState {
        l_shift: bool,
        r_shift: bool,
        l_ctrl: bool,
        r_ctrl: bool,
        l_alt: bool,
        r_alt: bool,
        l_cmd: bool,
        r_cmd: bool,
        caps_lock: bool,
    }

    impl ModifierState {
        /// Bitfield to put on every CGEvent. Combines the public masked
        /// bits (the ones apps query via `[NSEvent modifierFlags] &
        /// NSEventModifierFlagShift` and friends) with the
        /// device-dependent NX_DEVICE* left/right bits below 0x100.
        fn cg_flags(&self) -> CGEventFlags {
            let mut bits = 0u64;
            if self.l_shift || self.r_shift {
                bits |= CGEventFlags::CGEventFlagShift.bits();
            }
            if self.l_ctrl || self.r_ctrl {
                bits |= CGEventFlags::CGEventFlagControl.bits();
            }
            if self.l_alt || self.r_alt {
                bits |= CGEventFlags::CGEventFlagAlternate.bits();
            }
            if self.l_cmd || self.r_cmd {
                bits |= CGEventFlags::CGEventFlagCommand.bits();
            }
            if self.caps_lock {
                bits |= CGEventFlags::CGEventFlagAlphaShift.bits();
            }
            if self.l_shift {
                bits |= NX_DEVICE_L_SHIFT;
            }
            if self.r_shift {
                bits |= NX_DEVICE_R_SHIFT;
            }
            if self.l_ctrl {
                bits |= NX_DEVICE_L_CTRL;
            }
            if self.r_ctrl {
                bits |= NX_DEVICE_R_CTRL;
            }
            if self.l_alt {
                bits |= NX_DEVICE_L_ALT;
            }
            if self.r_alt {
                bits |= NX_DEVICE_R_ALT;
            }
            if self.l_cmd {
                bits |= NX_DEVICE_L_CMD;
            }
            if self.r_cmd {
                bits |= NX_DEVICE_R_CMD;
            }
            CGEventFlags::from_bits_retain(bits)
        }

        /// True if `vk` is a known modifier (incl. Caps Lock). The caller
        /// uses this to branch into the FlagsChanged path.
        fn is_modifier_vk(vk: u16) -> bool {
            matches!(
                vk,
                VK_LSHIFT
                    | VK_RSHIFT
                    | VK_LCTRL
                    | VK_RCTRL
                    | VK_LALT
                    | VK_RALT
                    | VK_LCMD
                    | VK_RCMD
                    | VK_CAPS_LOCK
            )
        }

        /// Update state for a press/release of a modifier or Caps Lock.
        /// Returns true if anything changed (callers skip emitting a
        /// FlagsChanged event when nothing did — e.g. a Caps Lock release).
        fn apply(&mut self, vk: u16, down: bool) -> bool {
            // Caps Lock is a toggle: flip on press, ignore release. This is
            // what real Mac keyboards do — the up event carries no new
            // information, and emitting one would unset AlphaShift mid-press.
            if vk == VK_CAPS_LOCK {
                if down {
                    self.caps_lock = !self.caps_lock;
                    return true;
                }
                return false;
            }
            let slot = match vk {
                VK_LSHIFT => &mut self.l_shift,
                VK_RSHIFT => &mut self.r_shift,
                VK_LCTRL => &mut self.l_ctrl,
                VK_RCTRL => &mut self.r_ctrl,
                VK_LALT => &mut self.l_alt,
                VK_RALT => &mut self.r_alt,
                VK_LCMD => &mut self.l_cmd,
                VK_RCMD => &mut self.r_cmd,
                _ => return false,
            };
            if *slot == down {
                return false; // idempotent — auto-repeat of a held modifier
            }
            *slot = down;
            true
        }
    }

    pub struct Inner {
        source: CGEventSource,
        // Secondary source used *only* to mirror modifier FlagsChanged
        // events. See `Inner::new` for why one source isn't enough.
        source_hid: CGEventSource,
        last_x: f64,
        last_y: f64,
        left_down: bool,
        right_down: bool,
        middle_down: bool,
        mods: ModifierState,
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
        // Virtual keycodes whose key-down we intercepted as a symbolic
        // hotkey (Cmd+Tab, Cmd+Space, screencapture combos). The matching
        // key-up gets swallowed too so the focused app doesn't see a bare
        // release with no preceding press.
        consumed_keys: HashSet<u16>,
    }

    impl Inner {
        pub fn new(target_display_id: Option<u32>) -> Result<Self> {
            // Two sources because macOS has two independent modifier-state
            // machines and different consumers read from different ones:
            //
            //   CombinedSessionState backs `[NSEvent modifierFlags]` — the
            //   query Cocoa apps use to decide "is Cmd held right now?" for
            //   app-level shortcuts (Cmd+C, Cmd+Q, Cmd+~).
            //
            //   HIDSystemState backs the HID-level modifier view that the
            //   WindowServer's *symbolic hotkey* dispatcher (Dock, Spotlight,
            //   screenshot, Mission Control, Show Desktop) checks before
            //   firing. Cmd+Tab is the canonical case.
            //
            // A flagsChanged event posted from one source does NOT update the
            // other. So we maintain both: the session source is the canonical
            // one used for every event, and `source_hid` exists solely to
            // mirror modifier flagsChanged so symbolic hotkeys also see Cmd
            // as held. Without the mirror, Cmd+Tab is a no-op; with it, both
            // classes of shortcut work.
            let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
                .map_err(|_| anyhow!("CGEventSource::new(CombinedSessionState) failed"))?;
            let source_hid = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
                .map_err(|_| anyhow!("CGEventSource::new(HIDSystemState) failed"))?;
            Ok(Self {
                source,
                source_hid,
                last_x: 0.0,
                last_y: 0.0,
                left_down: false,
                right_down: false,
                middle_down: false,
                mods: ModifierState::default(),
                target_display_id,
                click_left: None,
                click_right: None,
                click_middle: None,
                consumed_keys: HashSet::new(),
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
                KeyboardEvent::Synchronize(sync) => self.synchronize(sync),
            }
        }

        /// MS-RDPBCGR Synchronize event — the client tells us its current
        /// lock-key state (Caps Lock, Num Lock, etc.). Only Caps Lock has a
        /// CGEventFlag equivalent on macOS, so we reconcile that one: if our
        /// internal toggle disagrees with the client, flip ours and emit a
        /// FlagsChanged so any focused app's AlphaShift query is correct
        /// from the first keystroke. (Num/Scroll/Kana Lock are kernel-side
        /// concepts on Windows that macOS doesn't model.)
        fn synchronize(&mut self, sync: SynchronizeFlags) {
            let want_caps = sync.contains(SynchronizeFlags::CAPS_LOCK);
            if want_caps != self.mods.caps_lock {
                debug!(
                    have = self.mods.caps_lock,
                    want = want_caps,
                    "syncing CapsLock to client state"
                );
                self.mods.caps_lock = want_caps;
                self.post_flags_changed(VK_CAPS_LOCK);
            }
        }

        fn key(&mut self, scancode: u8, extended: bool, down: bool) {
            let Some(vk) = scancode_to_cgkeycode(scancode, extended) else {
                tracing::debug!(scancode, extended, down, "unmapped scancode");
                return;
            };

            // Modifier path: update per-side state and emit FlagsChanged
            // mirrored to both sources. We do this BEFORE any of the
            // symbolic-hotkey logic so the held-modifier check in
            // try_symbolic_hotkey sees the just-pressed modifier.
            if ModifierState::is_modifier_vk(vk) {
                let changed = self.mods.apply(vk, down);
                tracing::debug!(
                    scancode = format!("0x{scancode:02X}"),
                    extended,
                    down,
                    vk = format!("0x{vk:02X}"),
                    is_modifier = true,
                    flags = format!("0x{:08X}", self.mods.cg_flags().bits()),
                    changed,
                    "key post (modifier)"
                );
                if changed {
                    self.post_flags_changed(vk);
                }
                return;
            }

            tracing::debug!(
                scancode = format!("0x{scancode:02X}"),
                extended,
                down,
                vk = format!("0x{vk:02X}"),
                is_modifier = false,
                flags = format!("0x{:08X}", self.mods.cg_flags().bits()),
                "key post"
            );

            // Symbolic-hotkey interception: WindowServer's internal hotkey
            // dispatcher (which fires Cmd+Tab, Cmd+Space, Cmd+Shift+3/4/5,
            // Mission Control, etc.) only triggers on kernel-injected HID
            // events — user-space CGEventPost cannot wake it, regardless of
            // source state or tap location. For the common combos we
            // re-implement the action in user space and swallow the
            // keystroke. Triggers on key-down; the matching key-up is
            // tracked in `consumed_keys` so the bare key-up doesn't reach
            // the focused app either.
            if down && self.try_symbolic_hotkey(vk) {
                self.consumed_keys.insert(vk);
                return;
            }
            if !down && self.consumed_keys.remove(&vk) {
                return;
            }

            let Ok(ev) = CGEvent::new_keyboard_event(self.source.clone(), vk, down) else {
                warn!(vk, down, "CGEvent::new_keyboard_event failed");
                return;
            };
            let mut flags = self.mods.cg_flags();
            // macOS adds CGEventFlagNumericPad on events from the numeric
            // keypad. Some apps (Finder for arrow navigation, games) key
            // off it. Layout the table by macOS vk range — kVK_ANSI_Keypad*
            // sits in 0x41..=0x5C with a few gaps, see the
            // build_scancode_normal table above.
            if is_numeric_pad_vk(vk) {
                flags |= CGEventFlags::CGEventFlagNumericPad;
            }
            ev.set_flags(flags);
            ev.post(CGEventTapLocation::HID);
        }

        /// Emit a FlagsChanged event for `vk` from both sources. We send
        /// the same event twice so two different parts of macOS see the
        /// modifier as held:
        ///   - CombinedSessionState backs `[NSEvent modifierFlags]` —
        ///     what Cocoa apps query for Cmd+C / Cmd+Q / Cmd+~.
        ///   - HIDSystemState backs the modifier view that WindowServer's
        ///     symbolic-hotkey dispatcher (Dock, Spotlight, screencapture,
        ///     Mission Control, Show Desktop) checks before firing.
        ///
        /// A FlagsChanged posted from one source does NOT update the other,
        /// so without the mirror you get either app shortcuts or symbolic
        /// hotkeys but not both.
        fn post_flags_changed(&self, vk: u16) {
            let flags = self.mods.cg_flags();
            for source in [&self.source, &self.source_hid] {
                // `down` parameter is ignored for FlagsChanged: macOS
                // derives press-vs-release purely from the diff between
                // the prior flags state and the new flags carried on the
                // event. We pass `true` for shape only.
                let Ok(ev) = CGEvent::new_keyboard_event(source.clone(), vk, true) else {
                    warn!(vk, "CGEvent::new_keyboard_event failed (modifier)");
                    continue;
                };
                ev.set_flags(flags);
                ev.set_type(CGEventType::FlagsChanged);
                ev.post(CGEventTapLocation::HID);
            }
        }

        /// Match the current modifier state + non-modifier vk against the
        /// set of symbolic hotkeys we re-implement. Returns true if we
        /// handled it (and the caller should suppress the keystroke).
        fn try_symbolic_hotkey(&self, vk: u16) -> bool {
            let f = self.mods.cg_flags();
            let cmd = f.contains(CGEventFlags::CGEventFlagCommand);
            let shift = f.contains(CGEventFlags::CGEventFlagShift);
            let ctrl = f.contains(CGEventFlags::CGEventFlagControl);
            let opt = f.contains(CGEventFlags::CGEventFlagAlternate);

            if !cmd {
                return false;
            }
            // Only Cmd / Cmd+Shift are interesting here — any other extra
            // modifier means the combo isn't one of the symbolic hotkeys
            // WindowServer would have caught.
            if ctrl || opt {
                return false;
            }

            match (shift, vk) {
                (false, VK_TAB) => {
                    cycle_apps(false);
                    true
                }
                (true, VK_TAB) => {
                    cycle_apps(true);
                    true
                }
                (false, VK_SPACE) => {
                    invoke_spotlight();
                    true
                }
                (true, VK_3) => {
                    screencapture(&["-x"]);
                    true
                }
                (true, VK_4) => {
                    screencapture(&["-i"]);
                    true
                }
                (true, VK_5) => {
                    open_screenshot_app();
                    true
                }
                // Cmd+` / Cmd+Shift+` — cycle windows of the currently
                // frontmost app, native macOS semantics. Lets the user
                // explicitly step through windows within (e.g.) VSCode
                // without affecting the inter-app cycle. Whether the
                // RDP client forwards the backtick scancode at all
                // depends on its key-passthrough setting; mstsc treats
                // Win+` as a local "switch input source" combo by
                // default in some Windows versions, so it may need to
                // be re-bound on the client side to reach us.
                (false, VK_GRAVE) => {
                    ax_cycle_windows_of_front(false);
                    true
                }
                (true, VK_GRAVE) => {
                    ax_cycle_windows_of_front(true);
                    true
                }
                _ => false,
            }
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

    /// True for any macOS vk that hardware would mark with
    /// `CGEventFlagNumericPad` — keypad digits, operators, decimal,
    /// Enter, Clear. The cluster sits in the kVK_ANSI_Keypad* range
    /// 0x41..=0x5C with two gaps (0x46, 0x48) where macOS reserves
    /// no usage; we just exclude those.
    fn is_numeric_pad_vk(vk: u16) -> bool {
        matches!(
            vk,
            0x41 | 0x43
                | 0x45
                | 0x47
                | 0x4B
                | 0x4C
                | 0x4E
                | 0x51
                | 0x52
                | 0x53
                | 0x54
                | 0x55
                | 0x56
                | 0x57
                | 0x58
                | 0x59
                | 0x5B
                | 0x5C
        )
    }

    /// Process-wide cursor for `cycle_apps`. macOS's
    /// `NSWorkspace.frontmostApplication` does not update for app
    /// activations driven through the Accessibility API on the macOS
    /// versions we've seen — every press of Cmd+Tab resolves the same
    /// app as "current front" and we cycle in place. By remembering the
    /// app *we* last activated and starting from there, the cycle
    /// advances regardless of whether the OS's frontmost tracker
    /// catches up. Cleared automatically if the target later quits.
    static LAST_CYCLE_PID: std::sync::Mutex<Option<libc::pid_t>> =
        std::sync::Mutex::new(None);

    /// Per-bundle "most recently in front" PID. Updated each Cmd+Tab
    /// press from whatever NSWorkspace currently reports as frontmost,
    /// AND each time we AX-activate a target. Used during dedup to pick
    /// the right instance when multiple processes share a bundle ID
    /// (two VSCode windows opened as separate projects, two Firefox
    /// profiles, etc.) — without this we'd keep the first-launched
    /// instance and the cycle would route the user to the "wrong"
    /// VSCode. Note: we only learn about user-initiated switches
    /// (Dock click, mouse) at the next Cmd+Tab — if they swap A→B by
    /// clicking and immediately Cmd+Tab away, that press updates MRU
    /// to B before activating the next app. Good enough in practice.
    fn mru_map() -> &'static std::sync::Mutex<std::collections::HashMap<String, libc::pid_t>> {
        use std::collections::HashMap;
        use std::sync::OnceLock;
        static MRU: OnceLock<std::sync::Mutex<HashMap<String, libc::pid_t>>> = OnceLock::new();
        MRU.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
    }

    /// Cycle to the next (or previous) regular-policy running app and
    /// activate it. Replaces Cmd+Tab / Cmd+Shift+Tab, which WindowServer
    /// won't fire for CGEvent-posted keystrokes.
    fn cycle_apps(reverse: bool) {
        // SAFETY: NSWorkspace / NSRunningApplication are documented thread-
        // safe for these read-only queries.
        unsafe {
            let workspace = NSWorkspace::sharedWorkspace();
            let all = workspace.runningApplications();
            // `NSRunningApplication.isActive` lives on each snapshot
            // returned by runningApplications() and reflects WindowServer's
            // active-state property at the moment that snapshot was built.
            // After we trigger an AX activation, the next snapshot's
            // `isActive` does NOT update to reflect the new frontmost —
            // empirically Terminal kept showing active=true even after
            // Firefox was visibly on top, which made front_idx always
            // resolve back to Terminal and cycling stuck on the same
            // target. `NSWorkspace.frontmostApplication` is the
            // live-queryable replacement: it walks the active app fresh
            // each call. We compare by PID since the NSRunningApplication
            // identity from frontmostApplication isn't the same object
            // as the one in the runningApplications array.
            let front_pid = workspace
                .frontmostApplication()
                .map(|a| a.processIdentifier())
                .unwrap_or(0);
            // Pass 1: gather every running app's metadata. We do dedup
            // in a separate pass so we can prefer the MRU instance of a
            // bundle (the VSCode the user was last actually in) over
            // whichever launched first.
            struct AppMeta {
                bundle: String,
                name: String,
                pid: libc::pid_t,
                policy: NSApplicationActivationPolicy,
                terminated: bool,
            }
            let mut metas: Vec<AppMeta> = Vec::with_capacity(all.count());
            for i in 0..all.count() {
                let app = all.objectAtIndex(i);
                metas.push(AppMeta {
                    bundle: app
                        .bundleIdentifier()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "<no-bundle-id>".to_string()),
                    name: app
                        .localizedName()
                        .map(|s| s.to_string())
                        .unwrap_or_default(),
                    pid: app.processIdentifier(),
                    policy: app.activationPolicy(),
                    terminated: app.isTerminated(),
                });
            }

            // Refresh MRU. Whichever bundle is currently frontmost is the
            // one the user just left (or was working in if this is their
            // first Cmd+Tab) — that's the instance Cmd+Tab back should
            // land on later.
            {
                let mru = mru_map();
                let mut guard = mru.lock().expect("MRU mutex poisoned");
                if let Some(m) = metas.iter().find(|m| m.pid == front_pid) {
                    if m.bundle != "<no-bundle-id>" {
                        guard.insert(m.bundle.clone(), m.pid);
                    }
                }
                // Drop stale entries pointing at quit / terminated PIDs.
                guard.retain(|_, pid| {
                    metas
                        .iter()
                        .any(|m| m.pid == *pid && !m.terminated)
                });
            }
            let mru_snapshot: std::collections::HashMap<String, libc::pid_t> = {
                let mru = mru_map();
                mru.lock().expect("MRU mutex poisoned").clone()
            };

            // Pass 2: dedup by bundle, preferring the MRU instance. Apps
            // without a bundle ID pass through individually (rare for
            // regular-policy apps).
            let mut regular: Vec<(String, String, libc::pid_t)> = Vec::new();
            let mut by_bundle: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            let mut dup_pids: HashSet<libc::pid_t> = HashSet::new();
            for m in &metas {
                if m.policy != NSApplicationActivationPolicy::Regular || m.terminated {
                    continue;
                }
                if m.bundle == "<no-bundle-id>" {
                    regular.push((m.bundle.clone(), m.name.clone(), m.pid));
                    continue;
                }
                match by_bundle.get(&m.bundle).copied() {
                    None => {
                        by_bundle.insert(m.bundle.clone(), regular.len());
                        regular.push((m.bundle.clone(), m.name.clone(), m.pid));
                    }
                    Some(existing_idx) => {
                        // Two instances of the same bundle. Replace the
                        // kept entry only if the new candidate is the
                        // bundle's MRU pid; otherwise keep what we have.
                        if mru_snapshot.get(&m.bundle) == Some(&m.pid) {
                            dup_pids.insert(regular[existing_idx].2);
                            regular[existing_idx] = (m.bundle.clone(), m.name.clone(), m.pid);
                        } else {
                            dup_pids.insert(m.pid);
                        }
                    }
                }
            }

            // Build the human-readable dump after dedup so DUP tags reflect
            // actual decisions.
            let all_summary: Vec<String> = metas
                .iter()
                .map(|m| {
                    let mut tags = String::new();
                    if m.pid == front_pid {
                        tags.push_str(", FRONT");
                    }
                    if m.terminated {
                        tags.push_str(", TERMINATED");
                    }
                    if dup_pids.contains(&m.pid) {
                        tags.push_str(", DUP");
                    }
                    if mru_snapshot.get(&m.bundle) == Some(&m.pid) {
                        tags.push_str(", MRU");
                    }
                    format!(
                        "{bundle} (name={name:?}, policy={policy:?}, pid={pid}{tags})",
                        bundle = m.bundle,
                        name = m.name,
                        policy = m.policy.0,
                        pid = m.pid,
                    )
                })
                .collect();
            debug!(
                count = all.count(),
                regular_count = regular.len(),
                front_pid,
                "cycle_apps: full app list:\n  {}",
                all_summary.join("\n  ")
            );
            if regular.is_empty() {
                warn!("cycle_apps: no regular apps running");
                return;
            }
            // Pick the cycle cursor. Preference order:
            //   1. The app WE most recently activated, if it still exists.
            //      Lets repeated Cmd+Tab presses advance through the list
            //      even when the OS's frontmost tracker hasn't caught up
            //      with the AX activation we just did.
            //   2. The live frontmost via NSWorkspace (handles the first
            //      press after macrdp launches, or the case where the
            //      user manually clicked into another app via the Dock
            //      since our last activation).
            //   3. Index 0 as a final fallback (frontmost not in the
            //      regular-policy candidate set, e.g. Spotlight overlay).
            let mut last_cycle_guard = LAST_CYCLE_PID
                .lock()
                .expect("cycle PID mutex poisoned");
            let cursor_pid = last_cycle_guard
                .filter(|p| regular.iter().any(|(_, _, pid)| pid == p))
                .unwrap_or(front_pid);
            let front_idx = regular
                .iter()
                .position(|(_, _, pid)| *pid == cursor_pid)
                .unwrap_or(0);
            let n = regular.len();
            let next_idx = if reverse {
                (front_idx + n - 1) % n
            } else {
                (front_idx + 1) % n
            };
            let (target_bundle, target_name, target_pid) = &regular[next_idx];
            let target_bundle = target_bundle.clone();
            let target_name = target_name.clone();
            let target_pid = *target_pid;
            // Record our pick now so a subsequent Cmd+Tab advances from
            // here regardless of whether the OS's frontmost tracker
            // reflects our activation.
            *last_cycle_guard = Some(target_pid);
            drop(last_cycle_guard);
            // Also pin the MRU for this bundle to the instance we're
            // about to activate. If the user cycles away and back, the
            // bundle should resolve to this same instance — not the
            // first-launched one.
            if target_bundle != "<no-bundle-id>" {
                let mru = mru_map();
                mru.lock()
                    .expect("MRU mutex poisoned")
                    .insert(target_bundle.clone(), target_pid);
            }
            debug!(
                reverse,
                front_idx,
                next_idx,
                cursor_pid,
                os_front_pid = front_pid,
                target = %target_bundle,
                name = %target_name,
                pid = target_pid,
                "cycle_apps activating"
            );
            // Three activation paths exist on modern macOS:
            //   - NSRunningApplication.activateWithOptions: silently no-ops
            //     when the caller isn't already the front app (macOS 14+
            //     front-app-only enforcement).
            //   - osascript `tell application "X" to activate`: Apple
            //     Event, gated by TCC Automation. Unsigned macrdp running
            //     headless never gets the first-run grant prompt and the
            //     command silently fails.
            //   - `/usr/bin/open -b <bundle-id>`: LaunchServices route.
            //     The target's self-activate hits the same front-app-only
            //     gate, so it doesn't activate — BUT it does *launch* the
            //     bundle if no process is running, which gets in our way:
            //     when the user has just quit an app, falling through here
            //     re-launches it instead of skipping. Removed.
            //
            // Accessibility API is the one that works. macrdp already
            // holds the Accessibility TCC grant (required for CGEventPost
            // to take effect at all); AX permission lets us set
            // kAXFrontmostAttribute on a target app's AXUIElement, which
            // is the same gesture the Dock uses internally. No
            // front-app-only block, and — critically — doesn't relaunch
            // dead processes.
            let ax_err = ax_make_frontmost(target_pid);
            if ax_err == AX_ERROR_SUCCESS {
                debug!(target = %target_bundle, pid = target_pid, "cycle_apps: AX activation ok");
            } else {
                warn!(
                    target = %target_bundle,
                    pid = target_pid,
                    ax_err,
                    "cycle_apps: AX activation failed — skipping (no relaunch fallback)"
                );
            }
        }
    }

    const AX_ERROR_SUCCESS: i32 = 0;
    const AX_ERROR_ILLEGAL_ARGUMENT: i32 = -25201; // close enough — only used for null guard

    // Shared AX FFI surface used by the activation/window-cycling
    // helpers below. macrdp already holds the Accessibility TCC grant
    // (CGEventPost wouldn't work without it), so these calls don't
    // trip a permission gate. All return `AXError`:
    //   0       = kAXErrorSuccess
    //   -25201  = kAXErrorAPIDisabled (AX permission missing)
    //   -25204  = kAXErrorAttributeUnsupported (target has no AX
    //             attribute — e.g., a freshly-launched app whose main
    //             run loop hasn't installed AX yet)
    //   -25205  = kAXErrorNotImplemented
    //   -25211  = kAXErrorIllegalArgument (bad PID / null pointer)
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXUIElementCreateApplication(pid: libc::pid_t) -> *mut std::ffi::c_void;
        fn AXUIElementSetAttributeValue(
            element: *mut std::ffi::c_void,
            attribute: core_foundation::base::CFTypeRef,
            value: core_foundation::base::CFTypeRef,
        ) -> i32;
        fn AXUIElementCopyAttributeValue(
            element: *mut std::ffi::c_void,
            attribute: core_foundation::base::CFTypeRef,
            value: *mut core_foundation::base::CFTypeRef,
        ) -> i32;
        fn AXUIElementPerformAction(
            element: *mut std::ffi::c_void,
            action: core_foundation::base::CFTypeRef,
        ) -> i32;
        fn CFRelease(cf: *const std::ffi::c_void);
        fn CFEqual(a: *const std::ffi::c_void, b: *const std::ffi::c_void) -> u8;
        fn CFArrayGetCount(arr: *const std::ffi::c_void) -> isize;
        fn CFArrayGetValueAtIndex(
            arr: *const std::ffi::c_void,
            idx: isize,
        ) -> *const std::ffi::c_void;
    }

    /// Activate the target app via AX (kAXFrontmost) AND explicitly
    /// raise its focused window. The two-step gesture matters for apps
    /// with multiple windows in one process (the canonical case is two
    /// VSCode projects opened via File→New Window — same pid, two
    /// windows): setting kAXFrontmost activates the *process*, but
    /// macOS picks which window pops up, and it doesn't always pick
    /// the one the user was last working in. Following up with an
    /// AXRaise on `AXFocusedWindow` pins the result to the window AX
    /// tracks as focused — i.e. the one the user actually touched
    /// last. Without this the user perceives the cycle as "switching
    /// between two VSCodes" because the cycle keeps activating the
    /// same app but a different window pops up each time.
    fn ax_make_frontmost(pid: libc::pid_t) -> i32 {
        use core_foundation::base::{CFTypeRef, TCFType};
        use core_foundation::boolean::CFBoolean;
        use core_foundation::string::CFString;
        use std::ffi::c_void;
        use std::ptr;

        let app_ref = unsafe { AXUIElementCreateApplication(pid) };
        if app_ref.is_null() {
            return AX_ERROR_ILLEGAL_ARGUMENT;
        }

        let frontmost_attr = CFString::from_static_string("AXFrontmost");
        let true_val = CFBoolean::true_value();
        let frontmost_err = unsafe {
            AXUIElementSetAttributeValue(
                app_ref,
                frontmost_attr.as_concrete_TypeRef().cast(),
                true_val.as_CFTypeRef() as CFTypeRef,
            )
        };

        // Best-effort: raise the focused window. AX returns a +1 retain
        // on Copy* calls, so we must release the result. Failure here
        // shouldn't fail the overall activation — the caller cares
        // about whether the app got activated, not which window came up.
        let focused_attr = CFString::from_static_string("AXFocusedWindow");
        let mut focused: CFTypeRef = ptr::null();
        let copy_err = unsafe {
            AXUIElementCopyAttributeValue(
                app_ref,
                focused_attr.as_concrete_TypeRef().cast(),
                &mut focused,
            )
        };
        if copy_err == AX_ERROR_SUCCESS && !focused.is_null() {
            let raise_action = CFString::from_static_string("AXRaise");
            unsafe {
                AXUIElementPerformAction(
                    focused as *mut c_void,
                    raise_action.as_concrete_TypeRef().cast(),
                );
                CFRelease(focused.cast());
            }
        }

        unsafe { CFRelease(app_ref as *const c_void) };
        frontmost_err
    }

    /// Cycle through windows of the currently-frontmost app, the way
    /// native Cmd+` does. Pulls the app's window list via
    /// `kAXWindowsAttribute`, locates `kAXFocusedWindow` in that list,
    /// and AXRaises the next (or previous) one. Returns `false` if the
    /// app has fewer than two windows so the caller can fall back to a
    /// no-op rather than re-raising the same window.
    fn ax_cycle_windows_of_front(reverse: bool) -> bool {
        use core_foundation::base::{CFTypeRef, TCFType};
        use core_foundation::boolean::CFBoolean;
        use core_foundation::string::CFString;
        use std::ffi::c_void;
        use std::ptr;

        // Source-of-truth for "what app is the user actually in":
        //   1. LAST_CYCLE_PID — set every time we AX-activate via
        //      Cmd+Tab. This is the authoritative answer when the user
        //      has been navigating via macrdp's cycle.
        //   2. NSWorkspace.frontmostApplication — bootstrap before any
        //      Cmd+Tab has happened, or fallback when LAST_CYCLE_PID
        //      is unset.
        // NSWorkspace.frontmostApplication is NOT trustworthy after we
        // AX-activate: it keeps reporting the pre-activation app, which
        // is why we previously saw Cmd+` cycle through Terminal's
        // windows when the user was visually in VSCode (Terminal was
        // still NSWorkspace's notion of frontmost).
        let pid = {
            let cycle_pid = *LAST_CYCLE_PID
                .lock()
                .expect("cycle PID mutex poisoned");
            match cycle_pid {
                Some(p) => p,
                None => unsafe {
                    let ws = objc2_app_kit::NSWorkspace::sharedWorkspace();
                    match ws.frontmostApplication() {
                        Some(app) => app.processIdentifier(),
                        None => return false,
                    }
                },
            }
        };
        let app_ref = unsafe { AXUIElementCreateApplication(pid) };
        if app_ref.is_null() {
            return false;
        }

        let windows_attr = CFString::from_static_string("AXWindows");
        let mut windows: CFTypeRef = ptr::null();
        let copy_err = unsafe {
            AXUIElementCopyAttributeValue(
                app_ref,
                windows_attr.as_concrete_TypeRef().cast(),
                &mut windows,
            )
        };
        if copy_err != AX_ERROR_SUCCESS || windows.is_null() {
            unsafe { CFRelease(app_ref as *const c_void) };
            return false;
        }
        let count = unsafe { CFArrayGetCount(windows.cast()) };
        if count < 2 {
            unsafe {
                CFRelease(windows.cast());
                CFRelease(app_ref as *const c_void);
            }
            return false;
        }

        // Find the focused window's index in the array. If we can't
        // resolve it, fall back to position 0 — pressing Cmd+` ought to
        // do *something* visible.
        let focused_attr = CFString::from_static_string("AXFocusedWindow");
        let mut focused: CFTypeRef = ptr::null();
        let _ = unsafe {
            AXUIElementCopyAttributeValue(
                app_ref,
                focused_attr.as_concrete_TypeRef().cast(),
                &mut focused,
            )
        };
        let mut focused_idx: isize = 0;
        if !focused.is_null() {
            for i in 0..count {
                let w = unsafe { CFArrayGetValueAtIndex(windows.cast(), i) };
                if unsafe { CFEqual(w, focused) } != 0 {
                    focused_idx = i;
                    break;
                }
            }
            unsafe { CFRelease(focused.cast()) };
        }
        let next_idx = if reverse {
            (focused_idx + count - 1) % count
        } else {
            (focused_idx + 1) % count
        };
        let next_window = unsafe { CFArrayGetValueAtIndex(windows.cast(), next_idx) };
        // Multi-strategy window raise. Native Cocoa apps (Terminal,
        // Finder) honor AXRaise on a window — that alone is enough.
        // Electron apps (VSCode, Slack, Discord) expose AXRaise but
        // the implementation is a no-op on most versions; for those
        // we need to:
        //   - Set AXMain=true on the target window (Electron's window
        //     controller activates the window when this transitions).
        //   - Set the app's AXMainWindow attribute to point at the
        //     target window (canonical "make this the main window"
        //     gesture that some AX bridges only honor at the app level).
        // We apply all three and log which succeeded so it's obvious
        // from the trace which path actually moved the window.
        let raise_action = CFString::from_static_string("AXRaise");
        let main_attr = CFString::from_static_string("AXMain");
        let main_window_attr = CFString::from_static_string("AXMainWindow");
        let true_val = CFBoolean::true_value();
        let raise_err = unsafe {
            AXUIElementPerformAction(
                next_window as *mut c_void,
                raise_action.as_concrete_TypeRef().cast(),
            )
        };
        let set_main_err = unsafe {
            AXUIElementSetAttributeValue(
                next_window as *mut c_void,
                main_attr.as_concrete_TypeRef().cast(),
                true_val.as_CFTypeRef() as CFTypeRef,
            )
        };
        let set_main_window_err = unsafe {
            AXUIElementSetAttributeValue(
                app_ref,
                main_window_attr.as_concrete_TypeRef().cast(),
                next_window.cast(),
            )
        };
        debug!(
            pid,
            count,
            focused_idx,
            next_idx,
            raise_err,
            set_main_err,
            set_main_window_err,
            reverse,
            "ax_cycle_windows_of_front"
        );

        unsafe {
            CFRelease(windows.cast());
            CFRelease(app_ref as *const c_void);
        }
        raise_err == AX_ERROR_SUCCESS
    }

    /// Cmd+Space → invoke Spotlight. WindowServer's symbolic-hotkey
    /// dispatcher won't fire here either, and there's no public API to
    /// open the Spotlight UI directly. AppleScript's `key code` route
    /// internally posts via the Accessibility API, which (anecdotally)
    /// sometimes triggers the dispatcher where raw CGEventPost doesn't.
    /// Best-effort — if this still fails on a given macOS version, the
    /// user can rebind Spotlight in System Settings → Keyboard → Shortcuts
    /// to a custom binding (which goes through the normal app-level path
    /// our CGEventPost handles fine).
    fn invoke_spotlight() {
        let script =
            r#"tell application "System Events" to key code 49 using {command down}"#;
        let res = Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(script)
            .spawn();
        if let Err(e) = res {
            warn!(error = %e, "invoke_spotlight: osascript spawn failed");
        }
    }

    /// Run /usr/sbin/screencapture with the given args. `-x` = silent
    /// (no shutter sound), `-i` = interactive region. Output path is
    /// the default (~/Desktop) when not provided.
    fn screencapture(args: &[&str]) {
        // Default to the same path Cmd+Shift+3 produces natively.
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let Some(home) = home else {
            warn!("screencapture: $HOME unset");
            return;
        };
        let now = chrono_like_filename();
        let path = home.join("Desktop").join(format!("Screenshot {now}.png"));
        let res = Command::new("/usr/sbin/screencapture")
            .args(args)
            .arg(&path)
            .spawn();
        if let Err(e) = res {
            warn!(error = %e, "screencapture spawn failed");
        }
    }

    fn open_screenshot_app() {
        let res = Command::new("/usr/bin/open")
            .arg("-a")
            .arg("/System/Applications/Utilities/Screenshot.app")
            .spawn();
        if let Err(e) = res {
            warn!(error = %e, "open Screenshot.app failed");
        }
    }

    /// "2026-05-18 at 14.32.05" — matches macOS's native screenshot
    /// filename suffix closely enough that the result feels indigenous
    /// in Finder's sort order.
    fn chrono_like_filename() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Local-time conversion without pulling in chrono: use libc.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        let t = secs as libc::time_t;
        unsafe {
            libc::localtime_r(&t, &mut tm);
        }
        format!(
            "{:04}-{:02}-{:02} at {:02}.{:02}.{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
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
