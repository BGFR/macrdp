# Drive redirection

Mount the connecting client's local drives as real read-write volumes in
Finder (`--enable-drive-redirection`).

Opt-in with **`--enable-drive-redirection`** (off by default). The connecting
client redirects its **local** drive(s) and the Mac mounts each as a real
**read-write volume** in Finder — the inverse of file copy: instead of moving
bytes through the clipboard, you browse the client's filesystem live. Enable it
on the client too (mstsc: *Local Resources → More → Drives*; FreeRDP:
`/drive:NAME,PATH`).

Under the hood each redirected drive is served by an **in-process NFSv3 server**
that translates NFS operations into RDPDR (MS-RDPEFS) requests, mounted via the
built-in `mount_nfs` — **no root, no kext, no FUSE**. The kernel drives lazy
lookups as you browse, so full subdirectory navigation works, and reads/writes
reuse a kept-open handle so large sequential transfers don't re-open per chunk.
Reading, editing, creating, `mkdir`, rename, and delete all work where the
**redirected Windows user has permission** — e.g. write to `Users\<you>\Documents`,
not the `C:\` root (which an unelevated mstsc session can't write; that surfaces
as a normal "permission denied", not an error). Mounts are torn down when the
client disconnects.

> macOS-only. Every redirected filesystem device gets its own volume.
> `/Volumes` isn't writable without root on a stock Mac, so the mountpoint
> falls back to a per-session folder under `$TMPDIR` (it still shows as a
> volume in Finder).
