# Troubleshooting

Ordered by how often each thing actually goes wrong.

---

## The interface says "Cannot reach the monitoring service"

The UI is a client; the daemon is separate and must be running.

```bash
systemctl --user status jamsysd
systemctl --user start jamsysd
journalctl --user -u jamsysd -n 50 --no-pager
```

The daemon writes its own problems to the journal under its unit name. Host problems
(your CPU is hot) go to the alert stream; application problems (a collector failed) go to
the log. They are deliberately separate streams.

Check the socket exists and is yours:

```bash
ls -la "$XDG_RUNTIME_DIR/jamsys/"
# drwx------  … .        <- 0700 directory
# srw-------  … sock     <- 0600 socket
```

If `$XDG_RUNTIME_DIR` is unset — common inside `su`, a container, or a bare `ssh`
session — the daemon falls back to `/tmp/jamsys-$UID`, and a UI started with a
different environment will look in the other place. Start both from the same session.

### "socket path is N bytes, over the 100-byte kernel limit"

`sockaddr_un.sun_path` is a fixed 108-byte field. Point `XDG_RUNTIME_DIR` somewhere
shorter — normally `/run/user/$UID`.

---

## I reinstalled, but nothing changed

Check whether the *running process* is the binary you just installed. Replacing a file
does not replace a running process, and the two look identical from the outside:

```bash
PID=$(systemctl --user show -p MainPID --value jamsysd)
readlink /proc/$PID/exe
```

`/home/ronin/.local/bin/jamsysd (deleted)` means the process is still executing the old
inode. Restart it:

```bash
systemctl --user restart jamsysd
```

`scripts/install-user.sh` now restarts unconditionally and prints which binary the
daemon ended up on, because `systemctl enable --now` starts a stopped unit but does
nothing at all to a running one.

## The window does not open, "Message recipient disconnected"

```
Failed to register: GDBus.Error:org.freedesktop.DBus.Error.NoReply:
Message recipient disconnected from message bus without replying
```

A previous instance was still releasing the application id when the new one tried to
claim it — launching twice quickly, or relaunching right after closing. The application
now retries registration a few times before giving up, so this should not be reachable;
if it still appears, wait a second and launch again.

## The daemon will not start

```bash
jamsysd --check          # validate the config
jamsysd --log-level=debug  # run in the foreground and watch
```

| Message | Cause and fix |
|---|---|
| `using defaults, config could not be parsed` | A syntax error in `~/.config/jamsys/config.toml`. The daemon deliberately starts on defaults rather than refusing; fix or delete the file. |
| `database unusable … starting fresh` | The SQLite file was corrupted (usually an unclean shutdown on a failing disk). It is renamed to `history.db.corrupt.<timestamp>` and a new one is created. History is lost; monitoring is not. |
| `IPC bind: Address already in use` | Another daemon is running: `pgrep -a jamsysd`. |
| `Permission denied` on the socket directory | `$XDG_RUNTIME_DIR` is owned by another user. |

---

## A subsystem shows "Unavailable"

Open **Coverage**. Every collector states its support level and the reason. This page is
the authority — if something is unavailable, JamSys says so rather than pretending.

| Shows as | Meaning | Fix, if any |
|---|---|---|
| **Unavailable** | The hardware or interface is not present. | Usually nothing to fix; a desktop has no battery. |
| **Partial** | Some metrics work. The reason names which do not. | Often the privileged helper. |
| **Quarantined** | It worked, then failed three times in a row. | See the reason; it is retried every 15 minutes. |
| **Disabled** | Turned off in the config or on the Coverage page. | Flip the switch. |

### NVMe SMART and CPU package power are unavailable

Expected without the helper. `/dev/nvme0` is `0600 root` and
`/sys/class/powercap/intel-rapl:0/energy_uj` is `0400 root` (a deliberate mitigation for
CVE-2020-8694). NVMe *temperature* still works unprivileged through hwmon.

```bash
sudo systemctl enable --now jamsys-helper.timer
systemctl status jamsys-helper.timer
cat /run/jamsys/privileged.json
```

If the file is missing, check `journalctl -u jamsys-helper -n 20`.

### NVIDIA shows as unavailable but the card exists

```bash
ls -l /usr/lib/x86_64-linux-gnu/libnvidia-ml.so.1   # NVML library
cat /proc/driver/nvidia/version                     # driver loaded?
cat /sys/class/drm/card*/device/power/runtime_status
```

JamSys `dlopen`s NVML at runtime; it never links against it, so it builds and runs
identically with no NVIDIA hardware. If the library is absent, install the driver's
utility package.

### The journal collector is unavailable

Reading the system journal needs group membership:

```bash
groups | grep -E 'adm|systemd-journal'
sudo usermod -aG adm "$USER"   # then log out and back in
```

Without it, kernel errors, Xid messages and OOM records are not seen. Everything else
works.

---

## Alerts

### Too many, or ones I do not care about

Use the ⋮ menu on any alert: **Mute**, **Snooze**, **Ignore this rule**, **Ignore this
device or service**, **Change threshold**. All are reversible in **Settings**.

For a permanently failed unit you have decided not to care about — a broken snap, say —
"Ignore this service" is the right answer. It writes a suppression; the unit is still
tracked and still shown, just never alerted on.

To silence a recurring harmless kernel message, add a glob to `journal_ignore` in the
config. `*` is the only wildcard.

```toml
journal_ignore = ["*ACPI BIOS Error*", "*my noisy driver*"]
```

### None at all, and I expected some

* Learned rules stay silent until they have 120 samples in the current context —
  about 20 minutes, and separately for AC/battery and idle/active. Coverage shows this.
* Deterministic rules need their dwell time: 60 seconds at ≥ 95 °C, not one spike.
* Check for an accidental suppression in **Settings**.
* Check the minimum notification severity in the config (`min_notify_severity`).

### Notifications do not appear, but alerts show in the window

The daemon posts to `org.freedesktop.Notifications` on the session bus.

```bash
notify-send "test"                                # does anything work?
echo "$DBUS_SESSION_BUS_ADDRESS"                  # set in the daemon's environment?
systemctl --user show-environment | grep DBUS
```

A user unit started before the graphical session may lack the variable. The shipped unit
orders itself after `graphical-session.target` for exactly this reason. If you wrote your
own, add:

```bash
systemctl --user import-environment DBUS_SESSION_BUS_ADDRESS
```

To test the daemon's own D-Bus client in isolation — it is hand-written, so this proves
the marshalling as well as the connection:

```bash
cd jamsys-daemon && cargo run --release --example notify_selftest
# expected: "NOTIFICATION DELIVERED, id=<n>" and a bubble on screen
```

---

## The discrete GPU keeps waking up

This is a real condition JamSys is designed to find, and it is worth checking that
JamSys is not the cause — it is not, but here is how to prove it:

```bash
# Stop the daemon and watch. It should settle to "suspended" and stay there.
systemctl --user stop jamsysd
for i in $(seq 20); do cat /sys/class/drm/card2/device/power/runtime_status; sleep 3; done

# Start it again and watch the counter. It should not move.
A=$(cat /sys/class/drm/card2/device/power/runtime_active_time)
systemctl --user start jamsysd; sleep 90
B=$(cat /sys/class/drm/card2/device/power/runtime_active_time)
echo "dGPU awake for $((B-A)) ms during 90 000 ms"
```

Measured on the development machine: **0 ms**.

Common real causes: a browser or Electron application with hardware acceleration, an
application launched with `__NV_PRIME_RENDER_OFFLOAD`, an external display on a port
wired to the discrete GPU, or another monitoring tool polling `nvidia-smi`. The GPU page
lists processes holding the device, and the Events page shows when it woke.

---

## Storage looks wrong

### My snap mounts are missing

Deliberate. Snap mounts are read-only squashfs pinned at 100 % use. On a stock Ubuntu
desktop there can be forty of them, and a naive "disk nearly full" rule fires forty false
criticals. The Storage page states how many are hidden. Adjust `ignore_mounts` and
`ignore_fstypes` in the config if you disagree.

### A disk alert fires for a filesystem I do not care about

Use "Ignore this device or service" on the alert, which suppresses that mount point only.

---

## The database is larger than expected

```bash
du -h ~/.local/share/jamsys/history.db
jamsys --json stats | grep -E 'db_bytes|sample_rows'
```

Raw samples are the bulk and live 24 hours by default. Shorten it:

```toml
[retention]
raw_hours = 6
minute_days = 7
five_min_days = 30
fifteen_min_days = 90
```

Retention runs at the glacial tier (every 15 minutes) and deletes in chunks, so the
change takes effect gradually rather than in one long stall.

---

## The daemon is using more CPU than expected

```bash
jamsys --json stats
```

`collector_timings` shows per-collector cost. Note these are **wall-clock** durations
including blocking I/O, not CPU time — `thermal` looks expensive mostly because ACPI
sensor reads block in firmware.

To reduce it, lengthen the tiers:

```toml
[sampling]
fast_ms = 5000
medium_ms = 30000
slow_ms = 120000
idle_multiplier = 4.0
```

Or disable what you do not need — `process` is the single most expensive collector
because it walks `/proc`:

```toml
[collectors]
process = false
```

---

## Suspend and resume

JamSys derives sleep from `CLOCK_BOOTTIME − CLOCK_MONOTONIC`, so it needs no DBus
subscription and cannot miss a suspend that happened while it was not scheduled.

If overnight battery drain is flagged:

```bash
cat /sys/power/mem_sleep        # [s2idle] deep  — the bracketed one is in use
cat /proc/acpi/wakeup           # devices allowed to wake the machine
```

`s2idle` drains far more than `deep`. Whether `deep` is available is a firmware matter,
not something JamSys can change.

---

## I want to see the cluster without logging out

```bash
jamsys-cluster
```

The same drawing in a plain GTK4 window. Wayland will not let it place itself, so drag
it to the corner you want. This is also the answer on any desktop that is not GNOME.

### It is too small to read

Scroll on it, or press `+` / `-`; `0` returns to the default. Right-click for four size
presets. The size is remembered.

### Keeping it above other windows

Right-click and tick **Always on top** (and **On all workspaces** if you want it on
every desktop). Both are remembered.

If those two entries are greyed out, the window is running as a native Wayland surface,
where an application has no way to raise itself. Always-on-top is an EWMH request that
Mutter honours only for X11 clients, so `jamsys-cluster` runs on XWayland by default.
You get a native Wayland surface if you passed `--wayland`, or if you launched
`standalone.js` directly instead of through the launcher. Use:

```bash
jamsys-cluster
```

If they are *not* greyed out but nothing happens, check that the helper is installed and
that the window really is on X:

```bash
command -v jamsys-xabove
xprop -id "$(xwininfo -root -children | grep -m1 '"JamSys"' | awk '{print $1}')" _NET_WM_STATE
```

With the toggle on, `_NET_WM_STATE` should list `_NET_WM_STATE_ABOVE`. That property is
owned by the window manager, so if it appears there, Mutter accepted the request.

## The corner readout does not appear

### After installing, GNOME does not list the extension

Expected on Wayland. GNOME Shell will not load a **newly installed** extension without a
session restart, and Wayland has no `Alt+F2` `r`. Log out and back in, then:

```bash
gnome-extensions list | grep jamsys
gnome-extensions enable jamsys@jamsys.org
```

This is a GNOME constraint, not something JamSys can work around.

### It is enabled but shows "JamSys — offline"

The extension is a client; the daemon must be running.

```bash
systemctl --user status jamsysd
busctl --user list | grep jamsys     # should show org.jamsys.Daemon
gdbus call --session --dest org.jamsys.Daemon \
    --object-path /org/jamsys/Daemon --method org.jamsys.Daemon.GetState
```

If `busctl` shows nothing, the daemon started without a session bus — check
`journalctl --user -u jamsysd | grep -i bus`.

Note the two names are deliberately different: the **daemon** owns
`org.jamsys.Daemon`, and the **window** registers `org.jamsys.Monitor` as its
GApplication id, which must match its `.desktop` filename. When both claimed the same
name, GApplication found the daemon already there, assumed it was another copy of the
window, and refused to start with
`org.freedesktop.DBus.Error.UnknownMethod: org.gtk.Actions.DescribeAll`.

### It says State: ERROR

```bash
gdbus call --session --dest org.gnome.Shell.Extensions \
  --object-path /org/gnome/Shell/Extensions \
  --method org.gnome.Shell.Extensions.GetExtensionErrors "jamsys@jamsys.org"
```

`Unrecognized parameter "..."` means a Shell API was called with a key that GNOME's
`Params.parse` rejects. `gnome-extension/tests/shell-api-test.js` exists to catch
exactly that before it reaches the Shell; run it.

**A fix on disk does not take effect until the Shell restarts.** GJS caches the loaded
module, `disable`/`enable` reuses it, and `ReloadExtension` is a stub in GNOME 50. On
Wayland that means logging out — there is no way around it.

### It is enabled but nothing is drawn

```bash
journalctl --user -u gnome-shell --since "5 minutes ago" | grep -i jamsys
```

You can also test the rendering and the live data path without the Shell at all:

```bash
gjs -m gnome-extension/tests/format-test.js      # rendering
gjs -m gnome-extension/tests/live-dbus-test.js   # against the running daemon
```

### It is in the wrong corner

`gnome-extensions prefs jamsys@jamsys.org`. Note that bottom positions float above
the desktop rather than living in the panel — GNOME Shell has no bottom panel — so they
can overlap a dock and are hidden by fullscreen windows. Top positions are solid.

### It updates too rarely, or never

That is the design: the daemon emits an update only when a displayed value changes
enough for a human to notice. On a quiet machine that can be several seconds apart.
Check it is happening at all:

```bash
jamsys --json stats | grep dbus_signals
```

---

## I cannot close the gadget

```bash
jamsys-cluster --quit
```

That works without keyboard focus and without a working pointer. Otherwise: the
close button is at the top right and brightens when you point at it, and Escape or
Ctrl+Q work when the gadget has focus.

If it stopped responding to clicks entirely, **click-through** is on. On GNOME a
shaped input region also stops the window being focused or clicked at all, which is
why it is off by default. `--quit` then relaunching clears it, or:

```bash
jamsys-cluster --no-click-through
```

## An alert will not clear even though I fixed the problem

Alerts resolve on hysteresis, not instantly: the condition has to stop being true for
about two minutes before the alert closes, so a flapping metric does not fill the
timeline. Wait a couple of minutes before concluding it is stuck.

The specific case of **a failed service you have uninstalled** is different, and used
to be a genuine bug. systemd keeps a unit's failed result until someone clears it, so
removing the package leaves the failure behind forever and nothing you do to the
absent service helps. JamSys now recognises this -- a unit whose file is gone
(`LoadState=not-found`) is reported as leftover bookkeeping at Notice rather than as
a failure at Warning, and it tells you the one command that clears it:

```bash
sudo systemctl reset-failed <unit>
```

## The gadget jumped to full screen

Fixed. The whole face is a drag handle, which also behaves like a title bar, and
GNOME's default double-click-on-titlebar action is "maximize" -- so a double-click
pinned it full screen, and a maximized window ignores every resize request after
that. It now refuses to maximize at all. If you are running an older build, drag it
back with the window menu (Alt+Space -> Unmaximize) or restart it:

```bash
jamsys-cluster --reset
```

## Resizing does nothing, or "0" does not reset the size

Fixed, and worth knowing why: the shortcuts used to match the keyboard *symbol*, so
with a non-Latin layout active the "0" key produced `Farsi_0` rather than `0` and the
reset silently stopped working, while `-` and `=` kept working because those symbols
are the same in both layouts. They now match the physical key, so they work whatever
layout is selected.

## Bluetooth keeps disconnecting

JamSys now names the device and the time of every drop, and escalates to a warning
when the same device drops three times in fifteen minutes. Open **Devices**, or read
it from the shell:

```bash
jamsys --json snapshot | python3 -c 'import json,sys,datetime; b=json.load(sys.stdin)["snapshot"]["bluetooth"]; [print(datetime.datetime.fromtimestamp(e["at_ms"]/1000).strftime("%H:%M:%S"), e["kind"], e["name"], e["likely_cause"]) for e in b["events"]]'
```

### Why it does not always say why

The kernel does not expose an HCI disconnect reason to an unprivileged process, so
JamSys reports what it could observe and says "reason not observable" rather than
guessing. Note that `journalctl` will not help either: BlueZ logs nothing at all for
a clean disconnect. It does log failures to *re*connect, and those are translated
into plain language.

### The two things worth fixing first

Both are reported as risk notes on the alert when they apply.

**USB autosuspend on the Bluetooth radio** is the most common cause of dropouts on
Realtek combo adapters. Check it:

```bash
for d in /sys/bus/usb/devices/*/; do grep -qi bluetooth "$d/product" 2>/dev/null && echo "$d $(cat $d/power/control) suspended_for=$(cat $d/power/runtime_suspended_time)ms"; done
```

If it says `auto`, disable it for that device with a udev rule:

```bash
echo 'ACTION=="add", SUBSYSTEM=="usb", ATTR{product}=="*Bluetooth*", TEST=="power/control", ATTR{power/control}="on"' | sudo tee /etc/udev/rules.d/50-bluetooth-no-autosuspend.rules
```

**Wi-Fi power save**, on a combo Wi-Fi/Bluetooth chip, adds coexistence pressure:

```bash
iw dev "$(ls /sys/class/net | grep -m1 wl)" get power_save
```

Turning it off costs a little battery and often ends the dropouts:

```bash
sudo iw dev "$(ls /sys/class/net | grep -m1 wl)" set power_save off
```

### "Host is down" / page timeout when reconnecting

The headset is not answering. It has usually powered itself off or gone to standby;
switch it off and on again rather than retrying from the computer.

## Keyboard lighting

### The controls are greyed out

No write path is installed. On a from-source install that is the usual state, because
`install-user.sh` writes only into `~/.local` and the helper must be root-owned in
`/usr/libexec`. Run once:

```bash
sudo ./scripts/install-privileged.sh
systemctl --user restart jamsysd
```

or, to authenticate through the desktop prompt instead of a terminal:

```bash
pkexec /bin/bash ./scripts/install-privileged.sh
```

That installs both `jamsys-kbd` (lighting) and `jamsys-power` (battery charge limit)
with their Polkit actions. It finds the built binaries whichever way it was elevated:
`sudo` exports `SUDO_USER`, `pkexec` exports `PKEXEC_UID`, and both scrub
`CARGO_TARGET_DIR` — an earlier version relied on that variable and therefore always
failed on a machine using the staged toolchain.

The Hardware page says which of the two write paths is available. Check what the
daemon sees:

```bash
jamsys --json snapshot | python3 -c \
  "import json,sys;print(json.load(sys.stdin)['snapshot']['keyboard'])"
```

`control` will be `helper`, `direct` or `none`.

### "Authorisation was declined"

The Polkit prompt was dismissed, or the session is not active-and-local. Check the
action is installed:

```bash
pkaction --action-id org.jamsys.keyboard.set --verbose
```

### A colour is set but nothing changes

Confirm the helper works directly, which prints the exact error the kernel gave:

```bash
pkexec /usr/libexec/jamsys-kbd rgb 0 255 0 0 0     # static red
```

Some ASUS models advertise `kbd_rgb_mode` but only implement a subset of modes in
firmware. Try mode 0 (static) first; if static works and breathing does not, the
firmware is the limit, not JamSys.

### The colour shown is not the colour set

`kbd_rgb_mode` is **write-only in the kernel** — the current colour cannot be read back
from the hardware at all. The page shows what JamSys last set and labels it as such.
After a reboot, or if something else changed the lighting, it will not know.

---

## Reporting a problem

Collect this:

```bash
jamsysd --version
jamsysd --discover
jamsys --json coverage
jamsys --json stats
journalctl --user -u jamsysd -n 200 --no-pager
gnome-extensions info jamsys@jamsys.org 2>/dev/null
```

`--discover` and `coverage` contain hardware models and driver versions but no personal
data, no addresses beyond your local interface configuration, and no file contents.
