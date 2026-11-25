# Verification report — 2026-11-24

A review of the whole repository on the target laptop (ASUS TUF F16, Ubuntu 26.04,
kernel 7.0.0-44, GNOME Shell 50.1, Wayland), with the installed build running.

## What was done

* Baseline first: `./scripts/run-tests.sh --all` — 21 suites, all passing.
* Three independent read-throughs (daemon core; collectors, helpers and packaging;
  the two cluster surfaces), each asked for defects with a concrete wrong outcome,
  plus a read of the main window. Findings were checked against the running system
  before being acted on.
* Fixes with a regression test each, then the full suite again: 23 suites, all
  passing.
* The new daemon binary was run for three minutes in isolation (its own runtime,
  config and data directories, notifications off, no session bus) and queried over
  its socket.
* The reworked window pages were rendered against the live daemon with
  `scripts/render-window.py` and read.

## What was verified, and how

| Claim | Evidence |
|---|---|
| The Shell extension is in ERROR as installed | `gnome-extensions info jamsys@jamsys.org` → `State: ERROR`; `Clutter.DragAction` and `Clutter.Event.prototype.get_click_count` are `undefined` when the Mutter 18 typelib is loaded in `gjs` |
| The replacement API exists | `tests/real-api-test.js`: `ClickGesture`, `PanGesture`, their properties and signals, checked against the installed typelib |
| `position='custom'` is now storable | the same test writes it to the compiled schema through a memory backend |
| The sandbox is what zeroes per-process I/O | `readlink /proc/<jamsysd>/ns/user` differs from the session's while `ns/mnt` is identical; `systemd-run --user -p <directive>` one at a time: `PrivateTmp`, `ProtectSystem`, `ProtectHome`, `ProtectKernelTunables`, `ProtectControlGroups` each give a new user namespace and a denied `/proc/<pid>/io`; the seccomp set together gives neither |
| The corrected setup reads I/O | isolated run of the new binary outside any namespace: 6 of 24 top processes with non-zero I/O, against 0 of 24 from the installed service |
| `thresholds` / `set_threshold` | isolated run: 23 thresholds listed; `disk.full.pcnt` → `not_found`; `disk.full.pct = 5` → `out_of_range`; `= 97` stored and read back; `= 95` (the default) removed the override from `config.toml` |
| No spurious route-change events | isolated run: 0 `route_change` events in three minutes, against one every five minutes in the installed daemon's log |
| Large replies complete | unit test pushing a 3 MiB reply through a real socket and epoll loop, and checking the loop goes quiet afterwards |
| The unread-socket spin | integration test with a real pipe and epoll: the wakeup repeats under the old dispatch and stops under the new one |

## What was not verified

* **The extension has not been loaded.** The build was installed afterwards (see
  below), but GNOME Shell keeps the old module until the session restarts, so
  `gnome-extensions info` still reports `State: ERROR`.
* **The extension in a live Shell.** The gesture port is checked against the real
  API's shape, not against a compositor. Whether a pan and a double-click gesture
  coexist on one actor the way the code assumes is the first thing to look at after
  logging back in.
* **Per-process GPU time** was zero in the isolated run as well, for all 24
  processes. The sandbox explains the installed daemon's zeros; it does not explain
  that. On this boot the panel is driven by the NVIDIA card, whose `fdinfo` may not
  carry DRM engine counters. Unresolved.
* **The window's behaviour under a real pointer.** Tests drive the widgets'
  methods; nobody has dragged the slider or opened the menu on the new build.
* No hardware write, fault injection, suspend, or privileged path was exercised.
* The .deb was not rebuilt.

## After installing

`scripts/install-user.sh` was then run and the daemon restarted on the new binary.

| Check | Result |
|---|---|
| Daemon's user namespace | the same as the session's (it was a separate one before) |
| `jamsys --json thresholds` | 23 thresholds |
| Coverage | every collector `Full`; no warnings or errors in the unit's journal |
| Per-process I/O | 0 of 12 at 149 s, 3 of 15 at about 385 s. The machine was idle on battery, so the slow tier ran every 180 s and a rate needs two samples |
| Per-process GPU time | still zero for every process |
| Shell extension | still `State: ERROR`: old module, not reloaded |

The cluster window that was already open kept running the old code; it was left
alone.

## Left unfixed

Listed with their effects in `support-matrix.md` §5a. The largest are that learned
baselines are not persisted across restarts, that a notification which fails to send
is never retried, and that the interface has no localisation or RTL check.
