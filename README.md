<!-- LOGO -->
<h1>
<p align="center">
  <img src="https://rioterm.com/assets/rio-logo.png" alt="Rio terminal logo" width="128">
  <br>Rio Terminal
</h1>
  <p align="center">
    Rio is a modern terminal built to run everywhere.
    <br />
    <a href="#about">About</a>
    ·
    <a href="https://rioterm.com/docs/install">Install</a>
    ·
    <a href="https://rioterm.com/docs/config">Config</a>
    ·
    <a href="https://rioterm.com/changelog">Changelog</a>
    ·
    <a href="https://github.com/sponsors/raphamorim">Sponsor</a>
  </p>
</p>

Documentation: [rioterm.com](https://rioterm.com).

## Isolated Linux Runtime

This branch runs each terminal in a session worker and renders its immutable
passive snapshots through resident Sugarloaf grids owned by the window. The
configured `renderer.use-cpu` and backend settings select the normal window
renderer path; terminal cells are not copied through a renderer subprocess or
pixel readback path. The GUI owns native windows and input, not the PTY or
terminal parser. A window/compositor failure can take the GUI process down, but
the session worker remains the PTY/parser owner and can be recovered while its
retention period lasts. Windows is currently explicitly unsupported by this
runtime; there is no in-process fallback.

On Linux, open the command palette with `Ctrl+Shift+P`:

- **Merge Tab** immediately arms authenticated live Rio windows for pointer
  targeting. Move onto a target window and click to commit the selected tab and
  its splits; no live-window list is shown, and the source keeps its other tabs.
- **Recover Saved Session** is the separate saved-session picker. Committing a
  transfer preserves the original shell; the source GUI exits if no windows or
  pending transfers remain.
- **Move Current Tab to New Window** launches a separate GUI and transfers the
  existing sessions without creating an extra shell.
- Recovery after GUI death is explicit, not automatic shell respawning. Select
  the saved session; its original worker must still be alive within its retention
  period. Ordinary context/window close explicitly closes its sessions.

Repeatable private Xvfb acceptance is provided by
`scripts/session-isolation-acceptance.sh`. It exercises palette merge/detach,
multi-tab and nested-split transfer, target cancellation, direct resident-grid
repaint, dynamic resize, hint repaint, and shell PID/ACK continuity.
`scripts/session-isolation-visual-parity.sh` is a smaller current-versus-upstream
CPU/Xvfb fixture. It captures deterministic colors/underline, alternate-screen,
cursor, configured selection, and hint states, and records same-variant deltas
plus cross-variant image metrics. IME, Kitty/Sixel, and GPU-filter parity remain
unverified by that fixture.
`scripts/session-isolation-wayland-acceptance.sh` separately starts a private
nested KWin Wayland compositor and verifies direct Sugarloaf GPU window startup
with an AMD/Vulkan hardware summary. It does not claim native Wayland pointer
or drag input. Native Wayland drag remains a separate capability check requiring
a private nested compositor and a native Wayland input/drag driver; X11 tools
are not evidence for that path.
`scripts/session-isolation-wayland-visual-parity.sh` compares current and
upstream WGPU binaries using the existing graphics fixture for OSC-4, Kitty,
Sixel, alternate-screen, cursor, filter, and advanced-underline states. It
checks dedicated regions with expected-color pixel predicates and AE deltas;
the unfiltered current/upstream variants additionally check the fixture's known
Kitty red/green/blue/white quadrants and Sixel red/blue bands at fixed content
geometry. Filtered variants intentionally retain AE-only graphics evidence
because the filter changes those canonical colors. The harness does not claim
whole-window pixel identity. Wayland text-input-v3/input-method-v1 and Qt
input-method protocol XML plus KWin's `--inputmethod` option are installed,
but no ready private input-method server or canonical IME helper is available;
`fcitx5`, `ibus`, `wtype`, `xvkbd`, and a virtual-keyboard injector are absent,
while `ydotool` is global uinput. A private input-method server remains
feasible but was not added, so IME preedit/commit and GPU selection remain
unverified when the private compositor has no suitable input path. Profile
frames and CPU/RSS/Radeon activity are bounded workload evidence only. The
repeatable private `vkcube --wsi wayland --display_timing --c 60` probe selected
the AMD Radeon 780M but reported `VK_GOOGLE_display_timing extension NOT
AVAILABLE`; MangoHud, PresentMon, vkmark, and RenderDoc are absent. The
private KWin/Xvfb setup therefore exposes no presentation timestamps, so this
harness makes no FPS, frame-latency, or input-latency claim.
`scripts/session-isolation-wayland-x11-pointer-acceptance.sh` is a stricter
pointer probe: it hosts KWin's X11 backend on a private Xvfb display, applies the
known private keymap, and injects global XTest events only into that display. It
requires target-local arm, highlight, click, and source-exit evidence. A failed
probe is not a Wayland input claim; native Wayland DnD still requires a native
Wayland injector.
`scripts/session-isolation-wayland-dnd-acceptance.sh` separately exercises the
current native Wayland drag path through that private KWin/Xvfb setup. It records
target-local drag events, direct-frame readiness, authenticated opaque-token
commit, source GUI exit, survival of the transferred worker and shell, and a
fresh post-drop ACK through private primary-selection middle-click input. It
does not claim native post-drop keyboard ACK delivery because this compositor
setup has no reliable native keyboard injector.
`scripts/session-isolation-worker-kill-acceptance.sh` kills one recorded
session-worker in a private two-tab GUI and verifies the other pane's fresh ACK
and GUI continuity. It then closes the killed tab and the remaining tab through
the public paths, waiting for the asynchronous session pumps and GUI to cleanly
exit before reporting a pass.

For a bounded comparison against an `upstream/main` build, use
`scripts/session-isolation-direct-benchmark.sh` with private Xvfb binaries. It
records five fresh shell-ACK probes, interval process-tree CPU/RSS samples at
250 ms, and equal PTY workload bytes for 2.0 s idle, 3.5 s scroll, and 2.5 s
cursor phases. Both variants use the same 800x490 CPU/Xvfb path. ACK probes
inject one command with per-character `xdotool` events and a 100 ms marker poll;
their elapsed time includes injector, focus, event-loop, worker, and file-poll
overhead. It is not actual keystroke latency, frame/present latency, or a
throughput measurement. PTY bytes are workload accounting only; do not infer
instantaneous behavior or performance dominance from one bounded run.

A separate saved private KWin/Xvfb run recorded native Wayland DnD with an
opaque 16-byte payload and a fresh destination shell ACK
(`~/dev/rio-agent-artifacts/native-foreign-dnd-results.md`). The historical
record is protocol/transport evidence only; use the current DnD harness for
direct-render acceptance and the pointer harness for merge click-target input.

Panes render from immutable worker-published frames into resident Sugarloaf
grids owned by the window. The configured backend selects Sugarloaf's normal
CPU/native/WGPU window path; on Linux WGPU normally reports a Vulkan adapter
such as RADV, not a separate native Vulkan or Metal effect implementation.
Filter and advanced shader effect parity remains explicitly scoped. See
[Session Isolation](specs/session-isolation.md) for protocol, bootstrap,
recovery, testing, and platform details.

## Supporting the Project

If you use and like Rio, please consider sponsoring it: your support helps to cover the fees required to maintain the project and to validate the time spent working on it!

[![Sponsor Rio terminal](https://img.shields.io/github/sponsors/raphamorim?label=Sponsor%20Rio&logo=github&style=for-the-badge)](https://github.com/sponsors/raphamorim)

## Packaging

[![Packaging status](https://repology.org/badge/vertical-allrepos/rio-terminal.svg?columns=3)](https://repology.org/project/rio-terminal/versions)

> Demo with split and CRT on MacOS

![Demo Rio 0.2.0 on MacOS](https://rioterm.com/assets/posts/0.2.0/demo-rio.png)

> Demo with blurred background on Linux

![Demo blurred background](https://rioterm.com/assets/demos/demos-nixos-blur.png)

> Demo of Rio running on a Steam Deck

![Demo of Rio running on a Steam Deck](https://rioterm.com/assets/demos/demo-flatpak-steamdeck.jpg)

## Minimal stable rust version

Rio's MSRV is 1.96.1.
