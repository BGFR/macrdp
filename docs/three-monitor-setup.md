# Three-monitor Windows RDP ↔ macOS setup

This branch extends the experimental `--multimon-virtual` macOS RDP server mode
from two to three true virtual displays. The default remains **two** virtual
screens, preserving existing deployments.

## Windows (mstsc)

Use the supplied `rdp-3-monitors.rdp` profile, or add these settings to your
existing `.rdp` file:

```ini
screen mode id:i:2
use multimon:i:1
selectedmonitors:s:1,2,3
redirectclipboard:i:1
```

The Windows monitor list in the example has screen **3 above 2 above 1**,
all 1920x1080. The server maps the primary RDP monitor to the bottom of its
combined desktop and creates three macOS virtual displays in top-to-bottom order
(3, 2, 1). The Windows desktop's unrelated monitor 0 is not included.

## macOS server launch

The `--height` value is the **combined vertical** size, not the height of one
monitor. With 1920x1080 panels, run:

```bash
macrdp --virtual-display --multimon-virtual --multimon-count 3 \
  --no-client-resolution --width 1920 --height 3240 \
  --enable-h264
```

Include your usual authentication, bind, TLS, and other options. The `--enable-h264`
flag is optional (bitmap capture is also supported). Multimon cannot currently
be combined with `--make-primary`, `--detach-primary`, `--capture-primary`, or
`--shield-primary`.

For a packaged app using `config.env`, set:

```ini
VIRTUAL_DISPLAY=1
MULTIMON_VIRTUAL=1
MULTIMON_COUNT=3
VD_WIDTH=1920
VD_HEIGHT=3240
PRIMARY_MODE=none
```

The config-file loader automatically adds `--no-client-resolution` in multimon
virtual mode. `--multimon-count 2 --height 2160` retains the old two-monitor mode.

## Clipboard changes

When a clipboard item has both a bitmap rendering and actual Unicode text, the
server now selects text first instead of converting the copy to a BMP. For
macOS→Windows, text and images are no longer advertised simultaneously for a
single pasteboard item: text is chosen when present, image when image-only.

Remote→macOS pasteboard writes record the NSPasteboard change counter while
holding the pasteboard lock, preventing the local poller from treating the
server's own clipboard write as a fresh copy to send back to Windows.

## Validation

1. Build and run on a macOS machine with Screen Recording and Accessibility
   permissions granted.
2. Confirm the server log reports three attached virtual displays and three
   EGFX surfaces (if using H.264).
3. Connect with the three-monitor `.rdp` profile and verify that apps can be
   moved/maximized independently on Windows monitors 1, 2 and 3, and that
   cursor/click positions match each virtual display.
4. Copy plain text in both directions (including content with both text and
   picture representations), then copy a genuine image-only clipboard item.
5. Reconnect and check that clipboard updates still flow in both directions.

These source changes are **not compiled or macOS runtime-tested** in the
preparation environment. Validate on a test instance before replacing your
currently working RDP server.
