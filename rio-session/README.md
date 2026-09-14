# rio-session

`rio-session` is Rio's standalone per-terminal worker/client boundary. One
worker owns one PTY, child process, VT parser, grid, scrollback, and terminal
graphics. A client controls that state over an authenticated private Unix
socket; the client never owns a `Crosswords` replica. The Linux `rioterm`
compositor is integrated with this boundary and uses one client per pane. There
is no central session daemon.

## Current boundary

`SessionClient::spawn` starts a worker for one session. Dropping the client
detaches and leaves the worker alive; `SessionClient::close` is explicit. A
detached worker retains its state for five minutes. Only one attachment may
issue commands. A takeover is fenced by generations and uses this sequence:

```text
Offer(current) → Claim(current) → Claimed(new) → Initial(new FullFrame)
  → Commit(new) → Ready(new)
```

The first attachment has no `Offer`, but still completes `Claimed`/`Initial`/
`Commit`/`Ready`. The old attachment remains active while the new initial frame
is prepared and until `Commit`; failed preparation leaves it usable. Commit is
the ownership point. If the subsequent `Ready` write is lost, rollback could
create two writers, so the client must reconnect and claim the current
generation.

Handshake/frame I/O keeps a three-second absolute deadline. After sending
`Initial`, the authenticated worker waits up to **30 seconds** for `Commit`
(`rio_session::PREPARED_ATTACHMENT_TIMEOUT`) so cold renderer startup and initial
rasterization do not consume the short handshake budget. The eight-connection
limit still applies, partial writes do not extend the deadline, and preparation
does not renew the five-minute detached retention timer. Expiry drops only the
pending connection; it does not take ownership or close the old owner's shell.

`PreparedSessionAttachment::commit` has a three-second write deadline. It returns
after sending `Commit`; the first command or event poll checks the deferred
`Ready` with a separate three-second read deadline. Do not retry an uncertain
commit automatically. Start preparation after user selection and leave room for
delivery/acknowledgement inside the frontend's outer transfer timeout. A
30-second window-control timeout is an outer budget, not an additional renderer
lease; the existing shorter frontend readiness cutoff can remain conservative.

Public operations include `snapshot`, `snapshot_since`, `write`, `key`, `paste`, `resize`,
`scroll`, mouse and selection commands, focus, VI/search navigation, terminal
request responses, `set_alt_is_meta`, and `child_pid`. `poll_event()` returns
`Result<Option<SessionEvent>, SessionError>`: transport and protocol failures
are explicit, not silently treated as no event. Snapshots are structured
`FullFrame` values containing semantic terminal cells/styles, metadata, modes,
colors, selection/cursor/blink state, and bounded graphics data.

## Wire and security contract

The current protocol is **version 5** and uses native bincode 2
`Encode`/`Decode` (not serde), a four-byte little-endian length prefix, strict
exact decoding, a 16 MiB frame limit, and absolute read/write deadlines.
Commands, responses, events, frames, graphics, strings, dimensions, and
collection sizes are validated before use. The transport implementation is
Unix-only; the integration suite has been exercised on **Linux only**. Do not
infer macOS or BSD validation from the Unix cfgs.

Version 5 preserves both the ID and URI of each OSC 8 hyperlink. This keeps
link spans intact across frame decoding, deltas, and attachment transfer.
Version-4 workers require a version-4 client; start new sessions with the
updated binary rather than attempting to attach across protocol versions.

`snapshot_since(base_sequence)` returns a typed `FrameUpdate`. A `Delta` carries
strictly ordered changed rows and the complete bounded cursor, selection,
palette, viewport, mode, title, and working-directory metadata. Graphics,
resize, attach/recovery, and a stale base return `Full`; deltas never carry
partial graphics state. `FrameUpdate::apply_to` fences the base sequence before
mutating a cached complete frame. Workers retain only the active publication
base, not a frame history, and the same 16 MiB limit applies to encoded deltas.

Workers receive the 32-byte capability through a bootstrap pipe, never argv.
Unix peer credentials, private endpoint metadata, no-follow descriptor loads,
bounded descriptor reads, and redacted descriptor/client debug output protect
the authenticated boundary. Descriptor files are create-new and mode 0600.
The endpoint is removed on orderly worker cleanup, but a stale endpoint can
remain after an external `SIGKILL`; callers must handle failed reconnects.

`SessionSpec` carries shell/arguments, an absolute working directory, raw
environment entries, cell/pixel dimensions, scrollback, and grapheme mode.
The worker launches with a cleared environment and applies the validated
entries plus the terminal defaults. Non-UTF-8 environment values are rejected
by the current PTY backend. Exact environment forwarding and configured shell
arguments are unsupported through the Linux Flatpak `flatpak-spawn` path; this
must not become silent data loss.

## Control binary

`rio-sessionctl` consumes a descriptor produced by a host. It currently has
only `snapshot` and `write`; it does not spawn sessions:

```sh
cargo +nightly run -p rio-session --bin rio-sessionctl -- snapshot /path/to/descriptor
cargo +nightly run -p rio-session --bin rio-sessionctl -- write /path/to/descriptor $'printf hello\n'
```

Hosts create a `SessionClient`, save `client.descriptor()` with
`descriptor.save(path)`, and pass that file through a private/authenticated
channel. Do not put descriptor capabilities in an OS drag payload.

## Verification and scope

The Linux integration tests launch the real worker and cover cwd/environment,
structured snapshots and resize, takeover/reattach continuity, descriptor
attach, child-exit output/status and detached retention, malformed
authentication/framing, native-bincode allocation limits, absolute partial
frame deadlines, numeric input validation, stalled peers, failed takeover,
descriptor symlinks, worker isolation, and debug redaction.

The Linux `frontends/rioterm` compositor is migrated: it runs a bounded
background client pump, forwards immutable snapshots to a read-only isolated
renderer, and handles authenticated cross-window transfer. Prepared
attachments are decoded and rendered before commit. The command palette also
offers explicit recovery of private saved descriptors after a GUI crash; it
does not recover sessions automatically at startup, and labels active-owner
takeovers explicitly. Full crashed-window layout restoration is not provided.
Replacing a renderer changes only its render-buffer generation and must not
steal window input ownership or create a second worker controller. IME preedit,
occlusion, and content scale remain view/compositor state; committed text or
platform-neutral encoded keys go to the worker. Cell and pixel resize remains
a worker command.

Window control and session transport are Unix-only. Runtime behavior and
integration tests have been exercised on Linux; macOS/BSD behavior is not
validated and Windows transport is explicitly unsupported. A failed recovery
or takeover does not start a replacement shell. The saved descriptor remains
available when ownership is uncertain, subject to the bounded worker retention
period and explicit reconnect.

Graceful close waits for the worker's direct PTY child cleanup, but cannot
promise cleanup of arbitrary descendants. A worker killed externally cannot
run Rust destructors or endpoint cleanup. The worker's opt-in input budget is
enforced at the librio performer queue: reservations cover queued and partial
input, are released only as bytes are consumed or dropped, and return an
explicit bounded-input error rather than silently dropping writes. Selection
text is bounded while it is extracted, before the complete result is built.
Atlas pixel caches are reconciled on every snapshot against the authoritative
retained image keys for both main and alternate screens, captured under the
same terminal lock as placements and uploads. Removal deltas are not required
for correctness: `FullFrame` carries the complete image/placement state, and
bounded removal hints are derived from previously published images. Overflow
of transient removal bookkeeping therefore recovers on the next snapshot,
without recreating the session or shell.

Real image budgets still apply. If retained pixels were discarded because an
upload exceeded the retention budget, or an image exceeds the wire budget, the
worker explicitly rejects snapshots while the affected image remains retained.
Clearing that image allows snapshots to resume; no partial frame is substituted.
Aggregate frame/item limits likewise reject oversized live state, not historical
removal counts. These are bounded-resource limits, not permanent protocol damage.
