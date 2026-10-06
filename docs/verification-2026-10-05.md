# Review and verification — 2026-10-05

Reviewed repository baseline: `50b1f14`. Work took place in an isolated Ubuntu
24.04 container, not `/home/ronin/app/monitoring` or the user's desktop. No daemon,
UI or privileged helper was installed or run as a live monitoring application.
The date is the user's Los Angeles date (the session crossed October 6 UTC).

## Reproduced defects and fixes

| Defect | Evidence and change |
|---|---|
| Closing the client could deadlock while its reader was idle | A real socket-pair test hangs at buffered-file close in the baseline. Shutdown now interrupts the socket first, retires the connection by identity, and releases pending requests. |
| Lost requests waited ten seconds; failed handshakes leaked pending entries; retries could reopen a closed client | Of the first seven new lifecycle cases, the original client failed four and errored in one. All pass with the fix. Shutdown is terminal and retry/completion callbacks cannot restart the window. |
| Non-object JSON could kill the reader | Sending `null`, arrays or scalar JSON to the baseline caused a reader exception and a request timeout. Malformed frame shapes now leave the stream usable. |
| UI subscription blocked the main loop; repeated retries could accumulate workers | The real window scheduling methods are exercised with GLib completion delivery and recorded thread IDs. Connect and subscribe run on one worker; retries and overview-event requests are single-flight. |
| Failed report queries could claim healthy monitoring | `summarise({}, None, None, None, {})` returned `healthy` before the fix. Both interfaces now preserve missing-section errors and use an unknown verdict; partial reports retain known critical alerts. |
| Terminal reports discarded text beyond the terminal width | The old `line[:width]` removed diagnostic details. Long-line regression verifies the end of a diagnostic survives wrapping. |
| A terminating newline could bypass the 64 KiB request limit | Complete lines now receive the same size check as unterminated tails. Paired-socket tests also preserve valid batches of many short requests. |
| Clean-shell package builds could not find helper binaries | A layout test against the original script fails looking for `jamsys-helper` under the daemon's target directory. All crates now share an explicitly exported target directory. |
| Successful packaging could exit 141 | The layout regression observed SIGPIPE from `dpkg-deb --info \| head` with `pipefail`. The display step now drains output using `sed`. |
| Build requirements understated dependency versions | Cargo 1.75 rejects the locked 2024-edition dependency and lock format. The declared minimum is now Rust 1.85; package requirements state GTK 4.14 / libadwaita 1.4, matching APIs already used by the UI. |

## Interface improvements

- Persistent process name/PID filtering, CPU/memory/name sorting, and PID deduplication.
  Scope is explicitly the daemon's top samples, not all running processes.
- Offline/reconnecting banner on every page, a retry action, and last-update time.
- Ctrl+R / F5 refresh and a disabled refresh button while a request is pending.
- Report completion respects the existing interaction/scroll guard and throttles
  from completion time, including when service replies are slow.

## Executed validation

Dependencies were staged for the test environment; the desktop/session was not
changed. Compilation used Rust/Cargo 1.88.0, Python 3.12, GTK 4.14 and libadwaita 1.5.

| Check | Result |
|---|---|
| `scripts/run-tests.sh` | **15 suites passed, 2 failed, 4 skipped**. This is not a fully green hardware run. |
| Daemon library | **253 passed, 25 failed**. Eight tests assume the original laptop's devices/process inventory; fifteen need listening Unix sockets; two need netlink sockets. The container denies those sockets with `EPERM`. |
| Daemon degradation integration suite, executed separately | **19 passed**. The main Cargo invocation stops after failing library tests, so this separate execution matters. |
| Metric / keyboard / power helper unit tests | **5 / 12 / 7 passed**; no helper was used to change hardware. |
| Existing Python xabove / charge-limit / report checks | **34 / 22 / 46 passed**. |
| New client lifecycle tests | **11 passed**, using real kernel socket pairs, including slow replies, concurrent requests, shutdown, malformed frames and reconnect ownership. The address-connection step is substituted; actual socket listening is unavailable. |
| New process-selection / window-scheduling / package-layout tests | **5 / 4 / 1 passed**. Window scheduling imports real GTK classes but uses an unconstructed host and GLib callbacks, not rendered widgets. Package-layout tests use placeholder build outputs; the real release build was also executed separately. |
| Existing GJS suites | Format, gauge geometry, Shell API contracts, offline Cairo rendering and standalone lifecycle all passed. |
| Python compile/import, shell syntax, whitespace | Passed. All window classes import with the real GTK/Adwaita typelibs. |
| `packaging/build-deb.sh` | Successfully compiled all four release executables and built the amd64 `.deb`. Not installed. |

The second failed suite is the existing Python listening-socket client test;
it fails creating an `AF_UNIX` socket with `EPERM`, before reaching the client.
Three GTK display suites are skipped because no display exists. Xvfb cannot open
its listening sockets in this environment. The fourth skip is the live D-Bus test,
which requires a running daemon and `--all`. These constraints were left visible;
no assertions or test gates were weakened to make the runner green.

## Remaining target-machine validation

1. Run `scripts/run-tests.sh --all` in the target user's desktop session.
2. Check process filtering/sorting, scroll and keyboard focus through refreshes;
   inspect the connection banner in both themes and at a narrow window width.
3. Close the UI while the daemon is idle; reopen, restart the user service and
   confirm that cached values are marked offline until a fresh snapshot arrives.
4. Validate the installed package and normal monitoring on the target machine.
   No new claims are made about NVMe SMART, RAPL, GPU wake behaviour, battery or
   keyboard controls, or live GNOME Shell rendering.
