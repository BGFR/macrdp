# File copy (clipboard)

Bidirectional Mac↔Windows file copy over MS-RDPECLIP — how each direction
works and the two Windows-side limitations to know about.

Bidirectional via MS-RDPECLIP. Both directions support single files and folder trees.

**Mac → Windows.** `Cmd-C` a file or folder in Finder, `Ctrl-V` in Windows Explorer. The pasteboard walk recurses into directories (skipping symlinks, capped at 10 000 descriptors per copy) and emits the right `relative_path` so Explorer reconstructs the tree. Bytes stream on demand via `FileContentsRequest` chunks (4 MiB per chunk). Windows shows its native "Copying…" progress dialog.

**Windows → Mac (lazy, default).** `Ctrl-C` in Explorer, `Cmd-V` in Finder. The server pre-allocates an empty temp file per leaf at its declared size and registers each one with `NSFileCoordinator` via `NSFilePresenter`. Bytes only start streaming when Finder asks for them on `Cmd-V`, and macOS shows its **native "Preparing to paste" progress dialog** during the wait. Folder trees and multi-file selections both work. Lower chunk parallelism is used than the eager path so the RDP session stays responsive (mouse / keyboard / video) while a multi-hundred-MB paste is in flight. If you'd rather have files downloaded eagerly the moment Windows announces a copy (and `Cmd-V` auto-fired into Finder when ready, with an audible Glass-chime cue), pass `--no-lazy-paste`.

### Known limitations

- **`Ctrl-C` on a *folder* in Windows Explorer doesn't reach the Mac.** Explorer puts only the Shell IDList format on the clipboard and delay-renders `FileGroupDescriptorW`, which `mstsc` doesn't request — so nothing is forwarded over the RDP clipboard channel and you'll hear a beep on `Cmd-V`. Windows + mstsc behavior, not fixable server-side. **Workaround:** open the folder in Explorer, `Ctrl-A` to select its contents, then `Ctrl-C` — that path uses `FileGroupDescriptorW` directly and folder structure is preserved.
- **Some Windows shell extensions silently swallow specific files from the clipboard.** Archive tools (7-Zip, WinRAR, built-in Compressed Folders) commonly hook extensions like `.zip`, `.gz`, `.7z`, `.bz`, `.bz2`, `.rar`, `.tar` and intercept Explorer's clipboard so `Ctrl-C` either sends no `FileGroupDescriptorW` to mstsc or sends none at all. The Mac side detects the clipboard transition and clears the pasteboard, so `Cmd-V` in Finder beeps clearly instead of silently re-pasting the previous file. **Workaround:** rename the file to a neutral extension (e.g. `.bin`) and Windows will publish it normally.
