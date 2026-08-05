# vendor/ironrdp-async — divergence log

Local fork of `ironrdp-async` **0.8.0**, copied verbatim from upstream
`Devolutions/IronRDP@879ffed` (the same rev the root `Cargo.toml` git-pins the
rest of the ironrdp crates to). Pulled in via a **two-sided** `[patch]` in the
root `Cargo.toml` (both `[patch.crates-io]` and
`[patch."https://github.com/Devolutions/IronRDP.git"]`, mirroring
`vendor/ironrdp-dvc`) because the git-pinned ironrdp crates depend on
`ironrdp-async` within the IronRDP workspace, so the git-source resolution has to
be redirected too.

## Why this vendor dir exists (the ONLY divergence)

**A defensive guard in `Framed::read_by_hint` (`src/framed.rs`) against a
zero-length UNMATCHED PDU — an unauthenticated, pre-TLS, remote DoS.**

`read_by_hint` skips a PDU that doesn't match the requested hint by reading
`length` bytes and looping. When a `PduHint` reports `Some((matched: false,
length: 0))` — an unmatched, **zero-length** PDU — `read_exact(0)` consumes
nothing and performs no I/O, so the loop **spins forever at 100% CPU**: it never
yields to the runtime and never observes EOF, which also starves the executor
driving it (the acceptor's connection loop → **the whole server wedges**, and the
health-check watchdog misses it because the runtime probe still passes on the
other workers). This is reachable from a **single malformed 2-byte fast-path
frame** (`04 00` / `00 00` — first byte `& 0b11 == 0`, length byte 0), *before*
TLS or CredSSP, so the auth-guard never sees it.

The guard, right after the `Some((matched, length))` arm, fails instead of
hanging:

```rust
if length == 0 && !matched {
    return Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "PduHint reported a zero-length unmatched PDU; cannot make progress",
    ));
}
```

This is **macrdp's own upstream PR #1556** (commit `c541d09e`, "fix(async):
reject a zero-length unmatched PDU in read_by_hint"). It complements upstream
**#1515** (commit `33506e61`, in `ironrdp-pdu`), which hardened `find_size` so the
built-in X.224 / fast-path hint no longer returns a zero-length PDU — but
`read_by_hint` accepts arbitrary `dyn PduHint` impls, so guarding the framing
primitive itself is the robust fix (either fix alone breaks the loop; this one is
the lighter vendor — `ironrdp-async` is a 4-file crate vs `ironrdp-pdu`'s 113).

**Verified (2026-08-05):** the exact 2-byte trigger fired ×10 at the fixed server
→ CPU stayed 0.0% (no spin, no pegged thread), the accept loop stayed live
(`OK-still-accepting`), and the log shows the guard firing (`PduHint reported a
zero-length unmatched PDU; cannot make progress` → `accept_begin failed`) — a
clean per-connection rejection instead of a whole-server wedge.

## When to delete this vendor dir

Both fixes land upstream past the current pin: **#1515 is already in
`a5d1c682`** (the `chore/pin-bump-a5d1c682` branch carries it), and **#1556** is
this guard. When the IronRDP pin bumps past both, drop this vendor dir and the
two `[patch]` `ironrdp-async` entries — the fix comes from upstream then.

## Keep verbatim otherwise

Everything else (`connector.rs`, `lib.rs`, `session.rs`, and the rest of
`framed.rs`) is byte-identical to `879ffed`. Do **not** add unrelated changes
here — this fork exists solely to carry the DoS guard until the pin bump.
