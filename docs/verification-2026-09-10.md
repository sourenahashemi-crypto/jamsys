# Repository verification — 2026-09-10

Scope: execute the JamSys handover, reproduce correctness defects before fixing them,
add regression coverage, and make one small usability improvement. No push, install,
sudo/pkexec, driver unload, suspend, hardware-control write, or fault-injection scenario
was performed. The existing service was not restarted. `HANDOVER.md` was the only
pre-existing worktree change (untracked); it is preserved and excluded from commits.
No `AGENTS.md` exists in this checkout or its ancestor directories. The support matrix,
architecture, UI architecture, and handover were read before edits; the full baseline
was run before changing application code.

## Reproduced and fixed

| Defect | Before-fix evidence | Fix and regression |
|---|---|---|
| Alert intervals used the wall clock | `notification_intervals_use_monotonic_time` and `resolution_finishes_after_monotonic_hold` both failed, exit 101. The recorded notify time was an epoch timestamp, not monotonic. | Notification cooldown/refill and resolution now use monotonic time. Tests check the clock domain, the 119/120-second resolution boundary, and continued wall-clock snooze semantics. |
| A vanished thermal sensor remained hot in the snapshot | `losing_all_channels_clears_the_previous_snapshot` failed, exit 101: retained `Some(99.0)` instead of `None`. | The all-channels-gone error path clears the snapshot before returning `Gone`. No stale temperature or sample remains. |
| A driver disappearing permanently disabled its collector | The new degradation integration test failed, exit 101: probe count stayed 2 instead of reaching 3 at the retry deadline. | Previously usable collectors retry after 15 minutes, at their next tier run. Tests cover absent retry, return, and explicit disable. Sources absent at startup still do not automatically re-probe. |
| Shell widget rebuild leaked its popup | Shell contract regression failed, exit 1: the old popup was not destroyed. | Common teardown owns popup destruction, covering rebuild and disable. |
| Daemon loss left stale UI data/subscriptions and late replies could restore it | Original standalone callback harness failed two checks, exit 1: no name watcher and old readings remained. Shell tests against the original source failed cleanup and reply-ordering checks. | Both surfaces clean up connections, ignore callbacks from previous generations, and prefer a newer push to an older initial reply. Standalone watches loss/reappearance and releases its watcher on close. Shell generations survive disable/re-enable. |
| Offline panel guidance disappeared after a healthy update or rebuild | Tests against the original Shell source failed command visibility and offline rebuild checks. | Offline status restores visibility of the command and survives rebuilding the panel. |

The expanded Shell suite was also run against a temporary copy of the original
`HEAD` implementation: **10 failing checks**, exit 1. The checkout was not reverted
to do this. Production fixes subsequently pass the same behavioural checks.

One exploratory test supplied `null` on a failed GJS method reply. Inspection of the
installed `libgjs.so.0` resource, `/org/gnome/gjs/modules/core/overrides/Gio.js`, showed
the actual contract is `replyFunc([], error, null)`. The test was corrected to that
contract and the unnecessary production change was removed. This is **not** counted
as a product defect. Unknown Shell settings/API parameters remain strict failures.

## Usability change

Both cluster surfaces now use one offline renderer showing:

```text
JamSys — waiting for monitoring
systemctl --user start jamsysd
```

This replaces stale/blank instruments and automatically disappears when data returns.
It does not execute the command or persist any preference. Eight Cairo PNGs cover
housing/cutout, scales 0.55/1.3, and light/dark grounds. All 24 text/geometry checks
pass. Small cutout on light and normal housing on dark were visually inspected.

## Commands and results

Commands below run from the repository root, with `source scripts/devenv.sh` before
every Rust/build invocation. Raw session logs and renders are in
`/tmp/jamsys-review/` (temporary local artifacts, not part of the package).

| Command | Baseline | Final |
|---|---|---|
| `cargo test --release --manifest-path jamsys-daemon/Cargo.toml` | 263 unit + 16 integration passed; exit 0 | 267 unit + 17 integration passed; exit 0; zero binary/doc tests |
| `(cd jamsys-helper && cargo test --release)` | 5 passed; exit 0 | 5 passed; exit 0 |
| `(cd jamsys-kbd && cargo test --release)` | 12 passed; exit 0 | 12 passed; exit 0 |
| `(cd jamsys-power && cargo test --release)` | 7 passed; exit 0 | 7 passed; exit 0 |
| `python3 jamsys-ui/tests/xabove-test.py` | 34 checks passed; exit 0 | 34 checks passed; exit 0 |
| `python3 jamsys-ui/tests/charge-limit-test.py` | 19 checks passed; exit 0 | 19 checks passed; exit 0 |
| `gjs -m gnome-extension/tests/format-test.js` | 23 checks passed; exit 0 | 23 checks passed; exit 0 |
| `gjs -m gnome-extension/tests/gauges-test.js` | 60 checks passed; exit 0 | 60 checks passed; exit 0 |
| `gjs -m gnome-extension/tests/shell-api-test.js` | 53 checks passed; exit 0 | 69 checks passed; exit 0 |
| `gjs -m gnome-extension/tests/standalone-lifecycle-test.js` | New suite | 10 checks passed; exit 0 |
| `gjs -m gnome-extension/tests/offline-render-test.js /tmp/jamsys-review` | New suite | 24 checks passed; exit 0 |
| `gjs -m gnome-extension/tests/live-dbus-test.js` | 26 checks passed; exit 0; 2 pushes | 26 checks passed; exit 0; 2 pushes |
| `bash packaging/build-deb.sh` | Not run before edits | Built `build/jamsys_1.0.0_amd64.deb`; exit 0 |
| `sh -n build/jamsys_1.0.0_amd64/DEBIAN/{postinst,prerm,postrm}` (each script separately) | Not run | All three syntax checks exit 0; scripts not executed |
| `glib-compile-schemas --strict --dry-run build/jamsys_1.0.0_amd64/usr/share/gnome-shell/extensions/jamsys@jamsys.org/schemas` | Not run | Exit 0 |
| `(cd build/jamsys_1.0.0_amd64 && md5sum -c DEBIAN/md5sums)` | Not run | 47 files verified; exit 0 |
| `dpkg-deb --contents build/jamsys_1.0.0_amd64.deb` | Not run | Archive readable; exit 0 |
| `git diff --check` | Clean | No whitespace errors; exit 0 |

Targeted red/green commands used the daemon manifest with these filters:
`notification_intervals_use_monotonic_time`, `resolution_finishes_after_monotonic_hold`,
`losing_all_channels`, and `--test degradation a_disappeared_collector`. The first
three failed individually before the fix; all pass afterward. The final complete
suite includes them. Four unit tests and one integration test were added; no tests
were removed. The existing resolution test's seeded timestamp now uses the same
monotonic clock as the manager.

The first sandboxed baseline daemon run returned exit 101 (246 passed, 17 failed)
because local IPC/netlink/D-Bus sockets were denied. The live test returned exit 1
for the same reason. With approved access outside that sandbox, the complete baseline
and final daemon/live suites passed. No test assertion was disabled. Two existing
unused-variable/assignment warnings in `collectors/keyboard.rs` remain.

The Python charge-limit suite explicitly skips its absent-helper branch on this
machine because the helper exists. No charge-limit write was issued. The live
D-Bus suite reads the **already-installed daemon**, not the new package. It establishes
protocol/read/push compatibility, not deployment of the fixes.

## Limits and manual verification

- The new package is built but not installed. Live desktop behaviour of this version
  is unverified. After a user-approved install, log out/in to load the Shell module;
  test popup rebuilds, hide/restore, service loss/recovery, and offline guidance on
  both surfaces. Check real pointer/keyboard input; synthetic clicks are not proof.
- Thermal loss and collector recovery are synthetic tests. A real driver reload was
  not attempted. Recovery can take 15 minutes plus the next scheduled tier interval.
- SMART/RAPL privileged paths, suspend/resume and drain, real Xid/OOM, disk-full,
  additional keyboard modes, and Wi-Fi/Bluetooth fault injection were not exercised.
  They require separate approved manual checks; do not provoke OOM or disk-full on
  the working machine merely to verify an alert.
- The dGPU gate code was not changed. No new measurement of attributable GPU wake,
  power draw, CPU overhead, or battery impact was made. Historical figures in the
  support matrix are identified as historical, not reverified here.
- This is a targeted correctness pass, not proof that every subsystem or error path
  is defect-free. The support matrix retains the other documented coverage gaps.

Local commits separate backend correctness, UI lifecycle/usability, and verification
documentation. Their hashes are reported in the completion message; no commit was
pushed. `HANDOVER.md` remains untracked by design.
