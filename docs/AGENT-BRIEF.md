# Brief: find the bugs in JamSys, then make it better to use

You are taking over a working but rough desktop application. Your job is two
things, in this order:

1. **Find real bugs.** Not style opinions — defects that make the app wrong,
   silent, stuck, or misleading.
2. **Improve how it feels to use.** Missing options, awkward interactions,
   things that are technically implemented but that nobody can find or operate.

Read `docs/support-matrix.md` first. It is an honest account of what genuinely
works, what is partial, and what has never been executed on hardware. Keep it
honest — see "Rules you must not break" below.

---

## What the app is

JamSys is a local system-health monitor for one Linux laptop. It answers one
question: *is this machine behaving normally right now, and if not, exactly what
changed?* No telemetry, no cloud, no remote anything.

**Target machine** (the only one it has ever run on):
ASUS TUF Gaming F16 FX608JMR · Ubuntu 26.04 · kernel 7.0 · GNOME Shell 50.1 ·
**Wayland** · i7-14650HX · Intel iGPU + NVIDIA RTX 5060 (hybrid) · 32 GB ·
WD NVMe · Realtek RTL8852CE combo Wi-Fi/Bluetooth · PipeWire.

**Layout** (~23 000 lines):

| Path | What it is |
|---|---|
| `jamsys-daemon/` | Rust. Single thread, one `epoll` loop, one `timerfd`. Collectors, anomaly engine, SQLite store, D-Bus service, IPC socket. ~15 700 lines |
| `jamsys-ui/` | Python + GTK4/Adwaita. The main window, 16 pages. Also `bin/jamsys` (CLI) and `bin/jamsys-cluster` (launcher) |
| `gnome-extension/` | GJS. **Two surfaces sharing one renderer** (`gauges.js`): a GNOME Shell corner widget (`extension.js`) and a standalone GTK4 window (`standalone.js`) |
| `jamsys-kbd/` | Root helper: ASUS keyboard backlight + RGB. Polkit `allow_active` |
| `jamsys-power/` | Root helper: battery charge-stop threshold. Polkit `auth_admin_keep` |
| `jamsys-helper/` | Root, oneshot on a timer, optional: RAPL + NVMe SMART |
| `docs/` | 13 documents. They are part of the deliverable, not decoration |

---

## Rules you must not break

These are the user's, not negotiable:

- **The UI never runs as root. The daemon never runs as root.** Privileged work
  goes to a tiny helper through Polkit, and only ever with a closed vocabulary of
  validated integers. A helper must never accept a path, a filename, a shell
  string, or anything it does not fully understand.
- **No telemetry, no network calls, no cloud, no remote execution.**
- **Network monitoring must never inspect packet contents.**
- **Never claim a subsystem works unless it is wired to a real data source and
  tested.** If you cannot verify something on hardware, say so in
  `docs/support-matrix.md` and mark it unproven. Downgrading a claim you cannot
  support is *correct behaviour*, not a failure.
- Keyboard-lighting or battery failures must never affect monitoring.

---

## Environment traps that have already cost days

Read these before you touch anything. Each one was diagnosed the hard way.

**1. A GNOME Shell extension will not reload without a full logout.**
GNOME caches the loaded module. `disable`/`enable` reuses it, `ReloadExtension`
is a stub in GNOME 50, and a fresh-UUID copy is not discovered. Editing
`extension.js` and re-installing changes *nothing* on screen until the user logs
out. This wasted three review rounds. Before concluding "my fix didn't work",
compare the file mtime against the shell's start time:

```bash
E=~/.local/share/gnome-shell/extensions/jamsys@jamsys.org
stat -c %y "$E/extension.js"; ps -o lstart= -p $(pgrep -x gnome-shell | head -1)
```

Iterate on `standalone.js` (the window) instead — it restarts instantly — and
port to the extension once the behaviour is right.

**2. Synthetic input is unreliable here.** `XTest` pointer events get
interpreted as window drags by `Gtk.WindowHandle`, and synthetic clicks never
form a double-click (timestamps). Keyboard injection works only while the window
genuinely has focus, and Mutter overrides `XSetInputFocus` outright. **Do not
conclude a handler is broken from a synthetic-input test.** Instrument the
handler with `printerr` and read what actually arrived.

**3. A sysfs attribute is not a stream.** It parses *each* `write()`
independently. Writing a value and then a separate `"\n"` makes the kernel parse
a lone newline and return `EINVAL` — after the first write already took effect.
One buffer, one `write()`. See `jamsys-kbd/src/main.rs::attr_line`.

**4. Two clocks.** The rule engine runs on `CLOCK_MONOTONIC`; stored events carry
wall-clock for display. Mixing them produced an age of about −1.8e9 s, which read
as "inside the window" for ever, so an alert never resolved — and an alert that
never resolves never re-notifies. Anything holding a time must say which clock.

**5. The daemon probes capabilities only at startup.** Install a helper under a
running daemon and it keeps reporting `control: none`. Restart it.

**6. `sudo` scrubs `CARGO_TARGET_DIR`; `pkexec` exports `PKEXEC_UID`, not
`SUDO_USER`.** There is no `gcc` and no passwordless sudo on this machine; the
toolchain is staged at `~/.cache/jamsys-toolchain` and artefacts land in
`~/.cache/jamsys-target`. Always `source scripts/devenv.sh` first.

**7. Permissive test stubs hide real bugs.** The Shell stubs are deliberately
strict — an unknown settings key *throws*. This has caught: a GNOME 50 parameter
rejection (`addChrome({affectsInputRegion})`), a settings key that did not exist,
and a menu stub that discarded its items. **If you add an API call, model it in
the stub properly.** Never loosen a stub to make a test pass.

**8. Input-region shaping breaks the widget on Mutter.** Shaping a window's input
region stops it being focused *and* stops it receiving clicks — verified against
the region Mutter itself reports via `XShapeGetRectangles`. Click-through is
therefore opt-in and off by default. Do not turn it back on by default.

**9. Never wake the discrete GPU to measure it.** Read
`/sys/class/drm/card*/device/power/runtime_status` first and only call NVML when
the card is already awake. This is a hard requirement — measured 0 ms of
attributable wake, and it must stay that way.

---

## How to build, test, install

```bash
cd /home/ronin/app/monitoring
source scripts/devenv.sh                 # staged toolchain — always first

for c in jamsys-daemon jamsys-helper jamsys-kbd jamsys-power; do (cd $c && cargo test --release); done
python3 jamsys-ui/tests/xabove-test.py
python3 jamsys-ui/tests/charge-limit-test.py
for t in format-test gauges-test shell-api-test live-dbus-test; do gjs -m gnome-extension/tests/$t.js; done

bash scripts/install-user.sh             # user install, no root
bash packaging/build-deb.sh              # .deb
```

Current baseline, all passing: **263** daemon, 16 degradation, 5 helper, 12 kbd,
7 power, 34 xabove, 19 charge-limit, plus four JS suites. If your change drops a
number, explain why.

Useful while working:

```bash
jamsys --json snapshot | python3 -m json.tool | less   # everything the daemon knows
jamsys --json stats                                    # self-monitoring, collector timings
jamsys-cluster                                         # the window; restarts instantly
gjs -m gnome-extension/tests/render-cluster.js /tmp --live   # render the gadget to PNG
journalctl --user -u jamsysd -f
```

`render-cluster.js` is important: it draws the gadget with the *same* code the
Shell uses, so you can review layout changes as images without a logout. Always
check a **light** wallpaper as well as dark — the cut-out design fails there
first.

---

## Where to look for bugs

Ordered by where defects have actually been found:

1. **Lifecycle and state.** Alerts that never resolve, notifications that outlive
   their cause, widgets that keep stale state, settings written by a one-off CLI
   flag that then persist for ever (`--housing` did this).
2. **Anything with a clock or a window.** Hysteresis, dwell, flap detection,
   rollups, retention.
3. **Collector degradation.** Every collector must survive its data source
   vanishing mid-read. There is a `tests/degradation.rs`; extend it.
4. **The two surfaces drifting apart.** `extension.js` and `standalone.js` share
   `gauges.js` but not their input handling. Features have already been added to
   one and silently missed on the other. Audit both.
5. **Error paths that report the wrong thing.** The keyboard helper reported
   failure for a write that had succeeded. Look for more of that shape.
6. **Anything the support matrix calls unverified.** Section 3 lists NVMe SMART,
   RAPL, real suspend/resume, a real Xid, a real OOM. If you can verify one
   safely, do it and update the matrix. Do not fake it.

## Where to improve usability

The user's own words, paraphrased from review rounds:

- Controls should be **discoverable**. Scroll-to-resize existed for a long time
  and was invisible; a menu with Bigger/Smaller fixed the complaint, not the
  scroll handler.
- Nothing should happen **by accident**. A stray click must not launch a window.
- Anything you can hide must have a **visible way back**. "Hide the cluster"
  switches to a panel line whose menu can restore it, deliberately.
- Errors must say **what to do**. "No control path is installed" is useless;
  print the exact command with an absolute path.
- Reversibility matters: if you can set a battery limit, you must be able to
  lift it just as easily.

Good directions to consider (your judgement — do not do all of them):
per-monitor placement for the widget, keyboard-lighting profiles/presets, alert
snooze and per-rule muting from the widget itself, an at-a-glance history view,
export of a diagnostic bundle, first-run setup that offers the privileged
install, better empty/unknown states, accessibility and keyboard navigation of
the main window, localisation (the user is a Persian speaker — RTL is worth
checking).

---

## How to work

- **Verify before you claim.** Measure it, show the output. `ps` reports lifetime
  average CPU, not instantaneous — read `/proc/<pid>/stat` deltas if it matters.
- **Reproduce a bug before fixing it**, and add the regression test that would
  have caught it. Every trap in the list above now has one.
- Match the surrounding style: comments explain *why*, especially why an obvious
  approach was rejected. Do not add comments that restate the code.
- Commit in coherent pieces with messages that explain the reasoning, not the
  diff. Push to `origin main` (`git@github.com:sourenahashemi-crypto/jamsys.git`).
- Update `docs/` in the same commit as the behaviour. The support matrix is the
  contract with the user.
- If you find something you cannot fix — no root, no hardware, no reproduction —
  say so plainly and write it into the matrix rather than leaving it implied.

Start by reading `docs/support-matrix.md`, `docs/architecture.md` and
`docs/UI-architecture.md`, then run the suites to confirm the baseline before
changing anything.
