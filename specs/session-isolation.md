# Session Isolation

**Status:** the standalone worker/client boundary is implemented as protocol v4
and exercised on Linux. The default `rioterm` compositor owns one session
client per pane, renders validated passive frames through resident Sugarloaf
grids, and supports authenticated cross-window transfer and explicit
saved-session recovery. Runtime transport and native window control remain
Unix-only; Windows is explicitly unsupported.

This document describes the long-term process boundaries around Rio's
terminal core. [`librio`](librio.md) describes an embeddable terminal API; it
is not the authenticated inter-process protocol described here.

## Ownership decision

Rio has two process roles; rendering is a compositor-owned subsystem:

| Role | Owns | Must not own |
| --- | --- | --- |
| **Session worker** (one per terminal) | PTY/child, VT parser/`Machine`, `Crosswords`, scrollback, terminal input semantics, terminal graphics | Native windows, display handles, font atlases, renderer buffers |
| **Window compositor** (one per native window) | Native event loop/window, Sugarloaf, font/atlas state, resident per-pane grids, pane layout, focus/input routing, window UI, presentation | PTY, parser, `Crosswords`, session lifetime |

There is no central terminal daemon or shared central session owner. A launcher
may supervise processes, but it is not authoritative terminal state.

The intended data flow is:

```text
native events ──► window compositor ──► one SessionClient ──► session worker
                                        ◄── immutable FullFrame/deltas/events ──┘
                                       ──► passive frame data ──► resident Sugarloaf grids
```

The compositor is the sole worker client for a pane. It consumes immutable
snapshots and emits cells, graphics, and UI into window-local Sugarloaf
resources; no second process attaches to the worker and no process outside the
worker receives a live PTY or mutable `Crosswords` instance.

## Distinct generations and sequences

These identifiers must not be conflated:

- **Session attachment generation:** the worker's fenced command/event
  authority. A compositor takeover gets a new non-wrapping generation; stale
  commands and events are rejected. This is the generation used by the current
  `Offer`/`Claim`/`Claimed`/`Initial`/`Commit`/`Ready` handshake.
- **GPU resource lifetime:** the window-local Sugarloaf grid and image/atlas
  resources for a route. Rebuilding or resizing them does not change the
  worker attachment or input owner.
- **Terminal state sequence:** the monotonic sequence carried by `FullFrame` or
  `FrameDelta`. It identifies terminal snapshots, not attachment ownership or
  pixel-buffer lifetime. A delta names its immediate base sequence.

## Current worker/client contract

The current `rio-session` crate provides one durable worker and one active
`SessionClient` attachment. The worker retains a detached session for five
minutes; dropping a client detaches, while `close()` is explicit. Current
commands cover structured snapshots, committed byte/text input, platform-
neutral key input, resize (including pixel dimensions), scroll, mouse,
selection, search, VI navigation, focus, terminal requests, child PID, and
terminal options. Semantic cells, cursor blink state, bounded graphics, and
serializable callback requests are part of the v4 frame/event contract. Current
events are bounded and coalesced, with explicit lifecycle, request refusal and
expiry, notifications, and clipboard-overflow reporting.

The wire protocol is **v4**, using native bincode 2 `Encode`/`Decode`, not
serde. It has four-byte little-endian length framing, strict exact/trailing-byte
checks, a 16 MiB semantic frame limit, absolute I/O deadlines, authenticated private
Unix sockets, and nested bounds for messages, collections, terminal frames,
graphics, strings, dimensions, and input. `poll_event()` returns
`Result<Option<SessionEvent>, SessionError>` so protocol and transport failures
are explicit.

After the initial `FullFrame`, `snapshot_since(base_sequence)` may return a
bounded `FrameDelta` containing strictly ordered changed rows and complete
cursor, selection, palette, viewport, mode, title, and working-directory
metadata. Graphics changes, resize, attachment/recovery, stale bases, and any
publication mismatch return a complete frame. The worker keeps one active base,
does not retain an unbounded history, and the delta is subject to the same
encoded 16 MiB limit. Version 3 peers are rejected explicitly; there is no
legacy delta shim.

The first attachment completes `Claimed`/`Initial`/`Commit`/`Ready` without an
`Offer`. A takeover is:

```text
Offer(current) → Claim(current) → Claimed(new) → Initial(new FullFrame)
  → Commit(new) → Ready(new)
```

The old attachment remains active while the new snapshot is prepared and until
the commit. Failed preparation leaves it active. Commit is the ownership
point: if the post-commit `Ready` write is lost, rollback could create two
writers, so reconnect/reclaim of the current generation is the recovery path.

## Input and view boundaries

Raw `Write` and `Paste` are not a substitute for key semantics. The v4 `Key`
command uses platform-neutral key types and lets the worker's terminal core
perform mode/Kitty/Alt-meta encoding.

IME preedit, occlusion, and content scale are view/compositor state, not
terminal-worker commands. The compositor sends committed text or semantic key
events to the worker. Cell and pixel dimensions are sent through the worker's
resize command. The current Linux compositor preserves this split instead of
adding renderer input ownership.

## Direct Sugarloaf rendering

The window compositor keeps one passive `RemoteView` per pane. Each view
publishes the newest immutable `FullFrame`; the compositor takes its rows,
styles, extras, colors, cursor, selection, hints, search state, and graphics
placements into resident Sugarloaf grid and image resources owned by the
window. Session workers remain the sole PTY/parser/grid owners. The frontend
does not create a renderer subprocess, shared pixel slot, or pixel readback
path.

The window owns one resident grid per route and rebuilds all rows for full
damage, only affected rows for partial damage, and no rows for cursor/UI-only
updates. Dynamic pane dimensions are taken from the current layout and passed
to the grid uniforms; there is no fixed `4096x1024` caller contract or
per-pane pixel-buffer protocol. Graphics images are converted to Sugarloaf
RGBA handles and kitty, atlas, and virtual-placeholder placements are clipped
to each pane before presentation.

The configured main Sugarloaf backend remains authoritative. `renderer.use-cpu`
selects the CPU path; otherwise the configured native/WGPU path is used. On
Linux, WGPU normally reports a Vulkan adapter such as RADV, but this is not a
claim that WGPU provides native Vulkan or Metal effect implementations. Filters
are passed to the configured Sugarloaf path when the frontend `wgpu` feature is
enabled; unsupported configuration remains an explicit renderer error rather
than silent frontend stripping. GPU effect and advanced underline parity require
the dedicated visual harness; they must not be inferred from a compile or unit
test. That harness provides content-crop AE evidence, not whole-window pixel
identity.

Import preparation uses the same direct grid/resource path as presentation. It
prepares every imported tab, keeps source ownership until each destination
route has a usable prepared frame, and commits sessions independently. A
failed route does not discard successful commits or remove an uncommitted
source session. Private Linux GUI claims must be backed by a recorded run of
the repository acceptance harness, not by a unit test alone.

## Cross-window transfer and recovery

Moving a tab or split between windows is not renderer-buffer handoff. Rio uses a
distinct authenticated window-control protocol. The current channel is
**WindowControl v4**, separate from the session wire protocol v4 even though
both currently use version number 4. Its bounded transfer request carries the
target insertion index; the destination validates that index against the live
terminal route and rejects stale targets before commit. The same prepared
attachment path is used when a compositor explicitly chooses a saved recovery
descriptor:

1. Each compositor exposes an authenticated discovery endpoint. The source and
   destination exchange a transferable tab/split layout plus session IDs and
   descriptors over that authenticated channel.
2. An OS drag payload contains only an opaque transfer token. It never contains
   a descriptor, endpoint, or capability. On Wayland, the opaque token maps in
   the source's protected internal table to the source discovery endpoint.
3. The source retains each session's controlling attachment while the
   destination prepares the complete layout and a usable first frame for every
   pane.
4. Each session then performs its own generation-fenced commit. A multi-pane
   transfer may partially commit: committed sessions belong to the destination
   and uncommitted sessions remain at the source. Recovery queries current
   generations and never rolls back a committed session or creates two writers.

The Linux command palette actions are **Merge Tab** and **Recover Saved Session**.
Merge Tab immediately arms authenticated foreign Rio windows for pointer
targeting; it does not open a live-window picker. The target is highlighted only
while the pointer is inside it, and a click commits the selected tab and its
splits. Foreign window discovery authenticates each endpoint and asks its GUI to
confirm that the advertised native window still exists. Stale descriptors are
ignored, not deleted based on PID-only or racing filesystem checks.

**Move Current Tab to New Window** starts the same executable with internal
`--window-bootstrap` mode. A nonzero transfer identity is delivered over inherited
stdin, not a session capability in argv. The new GUI initially contains only a
sessionless placeholder and accepts its matching initial transfer. It creates no
bootstrap shell. Discovery selects the launched child's PID and then uses the
authenticated window-control channel; PID matching alone is not authentication.
The placeholder is removed after prepared contexts are installed. Normal launches
without this internal mode retain their ordinary initial-session behavior.

Recovery is never automatic at startup. **Recover Saved Session** discovers
private saved descriptors, probes them without committing, labels sessions with a
bounded title or working directory, and marks an active owner as an explicit
`active owner; takeover` choice. Sessions already owned by the current process
are excluded. Probe attachments are dropped after obtaining labels; the picker
retains descriptors, not expiring prepared claims. Selecting a candidate starts
a fresh asynchronous preparation. The selected session is decoded and prepared
through the destination's direct Sugarloaf resources before its attachment
commits. Failed preparation or direct-frame readiness leaves the worker and
its recovery descriptor available.

This protocol does not introduce a central owner; the source and destination
compositors remain the owners of their windows, and the session worker remains
the owner of terminal state. It does not restore an entire crashed window
layout automatically; recovery is one explicitly selected pane at a time.

## Lifecycle and failure rules

- Renderer or compositor failure must not terminate a session worker.
- Direct Sugarloaf rendering is resident in the window compositor, so a
  compositor panic, abort, or process failure can terminate that GUI. This is
  distinct from the worker boundary: the worker owns the PTY/parser and remains
  available for reattachment or explicit recovery while retained.
- Ordinary frontend context/window close sends explicit session `Close` commands.
  The final GUI exit is deferred until the corresponding session pumps finish
  that close handoff, with a bounded exit deadline. A crash or transport loss
  instead detaches. Transferred source contexts are disarmed before removal, so
  their destruction sends no `Close`.
- After a successful transfer removes its last window, the source GUI uses the
  normal event-loop exit path only when no bootstrap, incoming/outgoing transfer,
  recovery, or background session preparation remains. Destination ownership and
  the original worker/shell survive that process exit.
- The worker continues PTY/parser processing while detached and retains final
  child-exit status/output for bounded reattachment. Normal child exit is
  delivered before worker shutdown.
- Worker failure is a session failure, not a reason to recreate the PTY or
  child implicitly. Recreating it creates a new session.
- Graceful close waits for the worker's direct PTY child cleanup, but arbitrary
  descendants are not guaranteed to die. External `SIGKILL` bypasses Rust
  destructors and endpoint cleanup; stale endpoint paths must be treated as
  possible and reconnect failure must be handled.
- Slow or dead control peers are closed after bounded deadlines. The worker
  must not wait for the window compositor to consume a frame.

## Runtime validation

Repeatable acceptance uses the actual Linux `rio` executable, not an IPC-only
substitute. The harness starts a private Xvfb display, gives every run a fresh
runtime/config/TMPDIR tree under `~/dev/rio-agent-artifacts`, and records exact
owned PIDs, logs, ACK files, and a bounded process snapshot:

```sh
CARGO_PROFILE_DEV_CODEGEN_BACKEND=llvm \
  cargo +nightly build -p rioterm --features x11,wgpu
RIO_BIN="$PWD/target/debug/rio" \
RIO_ACCEPT_USE_CPU=1 \
  ./scripts/session-isolation-acceptance.sh
```

The Xvfb harness uses CPU presentation because Xvfb does not provide an X11
DRI3 swapchain. It covers two real GUI processes, multi-tab and nested-split
palette transfer, authenticated target highlight/cancel, Escape cleanup,
source-shell PID/ACK continuity, direct-grid geometry repaint, dynamic resize,
and hint repaint. The
per-run summary identifies idle RSS and `%CPU` snapshots as measurements, not
as benchmark-grade performance claims. Review the generated log directory
before reporting a runtime result.

For a repeatable nested Wayland GPU check, use the dedicated harness rather
than the Xvfb script:

```sh
RIO_BIN="$PWD/target/debug/rio" \
  ./scripts/session-isolation-wayland-acceptance.sh
```

It starts a private KWin virtual compositor with a private D-Bus session,
launches the real Wayland frontend with `use-cpu = false`, captures the Vulkan
hardware summary, and records a bounded RSS/`%CPU` snapshot. A successful run
validates direct Sugarloaf GPU window startup only; it does not exercise native
pointer or drag input.

For a bounded current-versus-upstream comparison, build an `upstream/main`
binary in a separate worktree and pass both paths to
`scripts/session-isolation-direct-benchmark.sh` with
`RIO_CURRENT_BIN` and `RIO_UPSTREAM_BIN`. The private Xvfb run records fresh
shell-ACK latency, interval CPU from process CPU ticks, RSS/process counts, and
equal PTY byte workloads for 2.0 s idle, 3.5 s scroll, and 2.5 s cursor phases.
The harness uses five ACK probes, per-character `xdotool` input, a 100 ms
marker poll, and 250 ms CPU/RSS samples. Both binaries use the same 800x490
CPU/Xvfb path; this is not a native/GPU comparison. ACK elapsed time includes
input injection, focus, event-loop, worker, and file-poll overhead, not actual
keystroke or frame/presentation latency. PTY bytes measure workload accounting,
not throughput. A short run must not be reported as instantaneous behavior or
performance dominance.

For a bounded pointer probe through KWin's X11 host backend, use
`scripts/session-isolation-wayland-x11-pointer-acceptance.sh`. It starts
Xvfb and KWin entirely on private artifact-local resources, applies the known
private keymap, and injects global XTest events only into that display. It
requires source-side palette input, target-local arm, highlight, pointer entry,
click, source exit, and a fresh transferred-shell ACK from private primary
selection middle-click input. A run that cannot deliver host input is reported
as failed/unverified, not as a passing native Wayland result. This host-input
probe does not validate native Wayland DnD payload delivery.

For GPU visual and effect evidence, use
`scripts/session-isolation-wayland-visual-parity.sh`. It runs current and
upstream WGPU-enabled binaries in private KWin virtual Wayland, captures fixed
terminal-content crops for deterministic text, cursor, alternate-screen,
Kitty, Sixel, OSC-4 palette mutation, and advanced underline states in plain
and configured-filter phases, and records bounded 600-frame workloads with
process and Radeon activity samples. It asserts dedicated palette/underline
regions with expected-color pixel counts as well as AE deltas, not just
capture-file existence. The unfiltered current and upstream variants also assert
the fixture's known Kitty 2x2 RGBA quadrants (red, green, blue, and white) and
Sixel red/blue bands at fixed content geometry; filtered variants intentionally
use AE only because the filter changes those canonical colors. GPU selection capture is available
only under private Xvfb/KWin-X11 mode, where the public SelectAll binding is
driven through that display; virtual KWin runs report GPU selection as
unverified. `RIO_ACCEPT_GPU_SELECTION_ONLY=1` requests the WGPU backend and
still requires positive AE changes in both a dedicated selection region and the
content crop. On the current host, native Wayland selection remains unverified:
the private KWin compositor exposes no virtual-keyboard/text-input injector,
and an X11-client retry failed at `Surface::configure` because KWin/Xvfb
reported no DRI3 presentation support. A host-window XTest event is not
selection evidence. The comparison is content-crop AE and expected-pixel
evidence rather than whole-window equality. Wayland text-input-v3 and
input-method-v1 protocol XML, Qt's input-method XML, and KWin's
`--inputmethod` option are installed, but no ready private input-method server
or canonical IME helper is available. `fcitx5`, `ibus`, `wtype`, `xvkbd`, and a
Wayland virtual-keyboard injector are absent; `ydotool`/`ydotoold` use global
uinput and do not generate isolated preedit events. A private input-method
server remains feasible but was not added, so IME preedit/commit is unverified
and native keyboard injection remains unverified. Profile markers, CPU/RSS
samples, and RadeonTop activity are not presentation timing. The repeatable
private `vkcube --wsi wayland --display_timing --c 60` probe selected the AMD
Radeon 780M but reported `VK_GOOGLE_display_timing extension NOT AVAILABLE`;
MangoHud, PresentMon, vkmark, and RenderDoc are also absent. The private
KWin/Xvfb path and current Rio logs therefore expose no presentation or
swapchain-completion timestamps, so the harness makes no FPS, frame-latency,
or input-latency claim. A future Wayland frame-callback probe would measure
compositor callback intervals, not Rio swapchain presentation or input latency.

For focused direct-render visual evidence, use
`scripts/session-isolation-visual-parity.sh`. It compares real current and
`upstream/main` binaries on private CPU/Xvfb windows for deterministic colors,
underline/bold text, alternate screen, cursor, configured selection, and hint
states. It requires nonzero within-variant state deltas and records
cross-variant image metrics, but does not claim complete pixel parity; IME and
GPU-filter parity remain unverified by this CPU harness. Use the dedicated GPU
harness above for Kitty/Sixel and filter-path evidence.

`scripts/session-isolation-worker-kill-acceptance.sh` kills one exact recorded
session-worker in a private two-tab GUI and verifies the other pane's fresh ACK
and GUI continuity. It then closes the killed tab and the remaining tab through
the public paths, waiting for the asynchronous session pumps and GUI to cleanly
exit before reporting a pass. A run that cannot observe normal worker/GUI
cleanup must remain failed.

For an interrupted multi-pane transfer, use
`scripts/session-isolation-partial-transfer-acceptance.sh`. It creates a
second public tab, arms the public merge target, kills only that selected tab's
recorded session worker before the target click, and then requires the source
GUI, retained source tab, and destination shell to remain live. It also
requires exactly one pre-interruption ACK from the killed tab, a fresh ACK from
the retained source tab, and a fresh destination ACK. This is an interruption
boundary test, not evidence that a killed session can be recovered without a
separate recovery acceptance.

Native Wayland drag acceptance is a separate capability check. The saved
`~/dev/rio-agent-artifacts/native-foreign-dnd-results.md` record confirms one
private KWin/Xvfb run with an opaque 16-byte payload, authenticated commit,
source GUI exit, and a fresh destination shell ACK. That historical record is
protocol/transport evidence only: it validates DnD, not the merge click-target
probe or current direct-render acceptance. For current direct-render DnD, run
`scripts/session-isolation-wayland-dnd-acceptance.sh`; it requires target-local
drag events, direct-frame readiness, authenticated commit, source GUI exit, and
worker/shell PID survival. It also requires post-drop private primary-selection
middle-click input to produce a fresh ACK from the transferred shell. XTest
keyboard input is probed separately and recorded as `PASS` or `UNSUPPORTED`;
the tested KWin setup advertises no virtual-keyboard global, so `UNSUPPORTED`
is an honest input-boundary result rather than a transport failure. Other
compositor/input setups still require a private nested compositor plus a
native Wayland input/drag driver; X11 input tools on a user's desktop are not
substitutes. If those tools are unavailable, report that setup as unverified
rather than claiming a Wayland result.

Wayland's implicit pointer grab can suppress Leave until button release. External
tab dragging therefore starts on out-of-surface motion before coordinate clamping,
while the original press serial is valid. A foreign destination tracks its own offer
without requiring a local source drag. It resolves the token against authenticated
source endpoints, not the destination window ID, and finishes the native offer only
after session commit. Source-side absence of local hover is not evidence of an
outside drop while a foreign transfer is active.

Testing must not use a user's active desktop. Use a fresh private Xvfb display,
explicit `DISPLAY`, unset `WAYLAND_DISPLAY`, and a private `XDG_RUNTIME_DIR` for
every X11 input/capture command. For nested Wayland, explicitly select the private
Wayland socket, runtime, and D-Bus address; unset `DISPLAY` for native clients.
Keep runtime/config/cache/log data and `TMPDIR` on disk under a private
development artifact directory. The harness keeps its summary, stdout, and
Vulkan logs outside each process's `RIO_CONFIG_HOME`; the live Rio application log
is intentionally at that config home's `log/rio.log`, because that is the path
selected by `--enable-log-file`. Match the private X server's keyboard layout
to the nested compositor before input tests.

The tested developer environment defaults to Cranelift, whose unsupported SSE2
intrinsic aborted CPU presentation. Tests/builds used
`CARGO_PROFILE_DEV_CODEGEN_BACKEND=llvm` and
`CARGO_PROFILE_TEST_CODEGEN_BACKEND=llvm`, without changing user configuration.
Build with `cargo +nightly build -p rioterm --features x11,wgpu`; default features
also include Wayland. The `wgpu` feature is needed for the frontend to parse and
forward configured librashader filters. The focused frontend lifecycle test is
`transferred_gui_exits_only_after_windows_and_preparations_are_gone`.

## Authentication and platform scope

The current transport uses private Unix endpoint directories/sockets, same-UID
peer credentials, and a fresh capability delivered through a bootstrap pipe.
Descriptors use bounded reads, private metadata checks, no-follow file opens,
and mode 0600 creation. Capabilities are not process arguments and are
redacted from descriptor/client debug output.

The implementation is Unix-only and the integration suite has been run on
Linux only. Unix conditional compilation is not evidence of macOS or BSD
validation. Wayland native surfaces remain owned by their creating compositor;
X11 reparenting is not an isolation baseline. Linux Flatpak's
`flatpak-spawn` path does not support exact forwarding of configured shell
arguments/environment, and non-UTF-8 environment values are unsupported by
the current PTY backend; neither limitation may be silently dropped.

## Migration status

### M0 — Contract and seams (documented)

- [x] Define worker, compositor-owned rendering, IDs, authentication, handoff, and
      backpressure boundaries.
- [x] Replace the desktop terminal path while retaining compositor-owned chrome.

### M1 — Standalone worker/client foundation (complete on Linux)

- [x] Worker owns the PTY/parser/grid and survives client detach on Linux.
- [x] v4 bounded/authenticated protocol, generation-fenced takeover, explicit
       close, final child status, and hostile transport cases are tested on
       Linux.
- [x] PTY input queue and selection extraction are bounded before upstream
      allocation.
- [ ] Full desktop terminal/input/graphics parity.

### M2 — Compositor-facing frontend adapter (complete on Linux)

- [x] Replace GUI-side terminal ownership with one worker client per pane.
- [x] Forward immutable snapshots to compositor-owned resident Sugarloaf grids.
- [x] Keep IME preedit and window state out of the worker protocol.

### M3 — Direct window rendering (Linux runtime integrated)

- [x] Render passive rows, styles, graphics, and cursor state through resident
      per-route Sugarloaf grids with dynamic dimensions.
- [x] Prepare hidden imported tabs through the same direct resource path before
      independent session commits.
- [ ] Complete GPU effect/underline parity and zero-copy resource paths.

### M4 — Native compositor and cross-window transfer (Linux runtime integrated)

- [x] Move native window/layout/input ownership into the compositor.
- [x] Add authenticated discovery and per-session fenced multi-pane transfer.
- [x] Add explicit command-palette recovery of saved private descriptors.
- [ ] Restore a complete crashed-window layout automatically.

### M5 — Optional acceleration (integrated; parity remains scoped)

- [x] Use the configured Sugarloaf CPU/native/WGPU window backend.
- [ ] Add GPU zero-copy paths and complete CPU/GPU effect parity.

## Migration hotspots

The first adapter must remove terminal ownership from the existing paths in
`frontends/rioterm`: `application.rs`, `context/`, `screen/`, `hints.rs`,
`layout/`, `renderer/`, bindings, IME, title/cwd handling, and `messenger.rs`.
The initial slice is `Context`/`Renderable`: consume `FullFrame`, route worker
commands, and leave layout/window presentation in the compositor. No GUI
isolation claim is valid until those paths stop reaching `Crosswords`.

## Acceptance checklist

- [x] One worker owns the live PTY/parser/grid; workers survive detach on Linux.
- [x] Bounded native-bincode framing, peer authentication, capabilities, and
      generation fencing are covered by Linux integration tests.
- [x] Failed takeover preparation leaves the old attachment usable.
- [x] The window compositor contains no `Crosswords` ownership.
- [x] Resident Sugarloaf grids cannot take window input ownership from the
      compositor or session worker.
- [x] Direct-render resource rebuilds leave the worker attachment fenced.
- [x] Window/compositor failures leave detached workers available for recovery.
- [x] Cross-window transfer handles partial per-session commits without two
       writers or exposing capabilities in drag payloads.
- [x] Interrupted selected-tab transfer acceptance passes with the current
       frontend after the selected-tab ownership fix.
- [x] PTY input and selection serialization are bounded before upstream
       allocation; broad desktop input/selection parity remains open.
- [ ] macOS/BSD transport behavior is separately validated; Windows transport is
      unsupported.
