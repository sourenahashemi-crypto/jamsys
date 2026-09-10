# Privilege Model

**Design goal: the number of things running as root should be as close to zero as
possible, and what does run as root should have no input channel at all.**

## The four principals

| Component | Runs as | Can be reached by | Writes |
|---|---|---|---|
| `jamsys` (UI) | you (desktop session) | you | UI config only |
| GNOME Shell extension | you (inside gnome-shell) | you | nothing |
| `jamsysd` | you (`systemd --user`) | UI over a `0700` socket dir; the extension over a session-bus name | `~/.local/share/jamsys/` |
| `jamsys-helper` | root, `oneshot` on a timer, **optional** | **nobody** | `/run/jamsys/privileged.json` |
| `jamsys-kbd` | root, via Polkit, **optional** | you, through pkexec | three ASUS LED attributes |
| `jamsys-power` | root, via Polkit, **optional** | you, through pkexec | one attribute: the battery charge threshold |
| `jamsys-xabove` | you (no privilege at all) | you, from the cluster window | nothing — one X11 `ClientMessage` |

**The UI never runs as root. The daemon never runs as root. The Shell extension holds no
privilege and contains no monitoring logic.** 97 % of all metrics need no privileges at
all — verified during discovery, not assumed.

`jamsys-xabove` is a third helper but not a privileged one. It exists only because
GJS cannot call `XSendEvent`, not because anything needs elevation: it runs as you,
opens no files, spawns no shell, and its whole vocabulary is two window-manager states,
a numeric window id, and `on`/`off`, all validated before the X display is opened
(`jamsys-ui/tests/xabove-test.py`, 34 tests). It is listed here so the table stays a
complete account of what JamSys executes, not because it widens the trust boundary.

`jamsys-power` is a separate binary from `jamsys-kbd` rather than another verb in
it, because each privileged helper should do one thing to one closed set of paths.
`jamsys-kbd` refuses to run at all on a machine with no lit keyboard, which is right
for a keyboard tool and wrong for a battery one. It takes no path argument and
cannot be given one: three candidate battery directories are compile-time constants,
the attribute name is a constant, and the only input is an integer validated to
20-100 before anything is opened. 20 is a floor, not a formality -- a "charge limit"
below that is a way to be caught with a flat battery.

Its Polkit action is deliberately **stricter** than the keyboard's. Keyboard colour
is cosmetic and changed constantly, so `allow_active=yes` is right there. A charge
limit is set rarely and changes how the machine treats its battery, so it uses
`auth_admin_keep`: authenticate once, remembered for a few minutes.

Note the asymmetry between the two root components, which is deliberate:
`jamsys-helper` **reads** and therefore has no input channel at all;
`jamsys-kbd` **writes** and therefore necessarily does, so all of the validation
effort goes there.

## What actually needs root, and nothing else does

Exactly two metrics on this machine:

| Metric | Why | Without helper |
|---|---|---|
| CPU package power (W) | `/sys/class/powercap/intel-rapl:0/energy_uj` is `0400 root` (RAPL side-channel mitigation, CVE-2020-8694) | Unsupported on AC. On battery, `BAT0/power_now` gives true whole-system draw, which is the more useful number anyway. |
| NVMe SMART / media errors | `/dev/nvme0` is `crw------- root root` | Unsupported. NVMe **temperature** still works via hwmon, unprivileged. |

Journal access needs **no root** — the user is in `adm`. Wi-Fi, battery, thermals, fans,
GPU, network, per-process GPU via DRM fdinfo: all unprivileged.

## Why the helper has no IPC

The obvious design — "privileged helper exposes a socket, daemon sends requests, helper
validates them" — creates a root-privileged parser reachable from an unprivileged process.
That parser is then the entire security surface of the application.

So the helper **has no request channel**:

```
timer (60 s) → helper runs → reads 2 fixed paths → writes JSON → exits
```

* It takes **no arguments, no stdin, no socket, no environment input**.
* The set of files it reads is a **compile-time constant array**. There is no path
  parameter, so there is no path traversal.
* It is **read-only**: it never writes to `/sys`, `/dev`, or anything but its own output.
* It writes atomically (`tmpfile` + `rename`) to `/run/jamsys/privileged.json`, mode
  `0644`, so the daemon just reads a file.
* It is a `Type=oneshot` unit driven by a `systemd` timer, so it is **not resident**.
  Root code executes for a few milliseconds a minute, then the process is gone.

Hardening applied in the unit (`packaging/systemd/jamsys-helper.service`):

```ini
ProtectSystem=strict        ProtectHome=yes         PrivateNetwork=yes
PrivateTmp=yes              NoNewPrivileges=yes     RestrictSUIDSGID=yes
ProtectKernelModules=yes    ProtectKernelLogs=yes   LockPersonality=yes
MemoryDenyWriteExecute=yes  RestrictNamespaces=yes  RestrictRealtime=yes
SystemCallFilter=@system-service   SystemCallArchitectures=native
CapabilityBoundingSet=CAP_SYS_ADMIN   ReadWritePaths=/run/jamsys
DeviceAllow=/dev/nvme0 r
```

`CAP_SYS_ADMIN` is required for the NVMe admin passthrough ioctl. If you would rather not
grant it, `Install: skip the helper` — the app is fully functional without it.

## No arbitrary command execution

* The UI **cannot** make the daemon run a command. The IPC protocol is a closed set of
  typed requests (`snapshot`, `history`, `alerts`, `ack`, `mute`, `set_threshold`,
  `coverage`, `inventory`, `processes`, `ping`). There is no `exec`, no path parameter that
  reaches a shell, and no config key that is interpreted as a command.
* The daemon spawns exactly **one** child in its lifetime: `journalctl` with a fixed
  argument vector, via `execvp` with no shell.
* Config values that could reach a filesystem call (e.g. ignored mount points) are
  matched, never executed, and never interpolated into a command line.

## Socket access control

```
$XDG_RUNTIME_DIR/jamsys/          drwx------  (0700, you)
$XDG_RUNTIME_DIR/jamsys/sock      srw-------  (0600, you)
```

`$XDG_RUNTIME_DIR` is already `0700` and per-user under `/run/user/$UID`. Combined with
the `0700` subdirectory this means **only your UID can connect** — enforced by the kernel,
not by an auth handshake. The daemon additionally reads `SO_PEERCRED` on accept and
rejects any peer whose UID differs from its own, which closes the case where the runtime
dir has been loosened.

Requests are capped (64 KiB per line), the client count is capped (8), and a malformed
line closes that connection only.

## Privacy

* **No packet capture, ever.** Network monitoring reads counters
  (`/sys/class/net/*/statistics/*`) and link state. No `AF_PACKET` socket is opened, no
  payload is read, no `pcap` dependency exists.
* Connection counting reads `/proc/net/tcp{,6}` — state and counts only, and only for
  aggregate counts.
* **No telemetry, no analytics, no update check, no account.** The daemon makes no
  outbound connection except the optional reachability probe, which is a bare TCP
  `connect()` to the default gateway, the configured DNS server, and one configurable
  host — with **zero bytes sent** — and can be disabled entirely
  (`[collectors] reachability = false`).
* Everything is stored under `~/.local/share/jamsys/`. Deleting that directory deletes
  all history.

## The write helper: `jamsys-kbd`

Keyboard lighting is the only thing JamSys can change, and it is the only privileged
component that accepts input. Every ASUS control path on this machine is `root:root 0644`,
and the two RGB attributes are `--w-------`, so there is no unprivileged route:

```
brightness           -rw-r--r-- root root      0..3
kbd_rgb_mode         --w------- root root      write-only: cmd mode red green blue speed
kbd_rgb_state        --w------- root root      write-only: cmd boot awake sleep keyboard
```

Verified: writing `brightness` as the desktop user returns `Permission denied`.

### What keeps it safe

* **No path is ever a parameter.** The three writable files are compile-time constants.
  There is no traversal because there is nothing to traverse. A unit test asserts that
  every operation renders to one of exactly those three paths and that none contains `..`.
* **No shell, ever.** pkexec `execve`s the binary with an argument vector. The helper
  spawns nothing, reads no environment variable, opens no socket and does not read stdin.
* **Arguments are parsed into typed, range-checked values before anything is opened**,
  and an out-of-range value is a hard exit rather than a clamp. Strict parsing rejects
  `+1`, ` 1`, `0x2`, `-1`, `1.0`, non-ASCII digits and overlong input.
* **Validation runs before the privilege check**, so bad input never reaches a write even
  when the caller is already root. Verified: `jamsys-kbd write /etc/passwd 1` exits 2.
* **Write-only surface.** It has no code path that reads a sysfs attribute, so it is not
  an information-disclosure primitive either.
* **208 lines** excluding comments, so it can be audited by reading it. Ten unit tests
  cover the injection attempts explicitly.

### Why `allow_active=yes`

The Polkit action authorises a physically-present, active local session without a
password — the same posture the desktop already takes for screen brightness. Prompting on
every drag of a colour picker would make the feature unusable, and users respond to that
by disabling something worse. Remote and inactive sessions must still authenticate as an
administrator.

### The alternative with no privileged code at all

`packaging/udev/99-jamsys-keyboard.rules` grants the `video` group write access to those
same three attributes. Install it and JamSys needs no privileged binary for keyboard
control whatsoever — strictly less attack surface, since there is no privileged code to
attack. The trade-off is granularity: any process running as you can then write them at
any time, whereas the helper route puts each write behind a Polkit action that can be
tightened, logged or revoked centrally.

The helper is the default because it is the more controllable of the two. Both are
shipped; the Hardware page detects which is present and says so.

### What this helper cannot do

It cannot set the platform profile, the battery charge limit, fan curves, or anything
else. Those interfaces exist on this machine (`platform_profile`,
`charge_control_end_threshold`, `throttle_thermal_policy`, the `asus-armoury` firmware
attributes) and are deliberately **not** wired up: monitoring has priority over tweaking,
and each one needs its own validation and its own testing on hardware before it should be
allowed anywhere near a root binary.

## D-Bus surface

The daemon owns `org.jamsys.Daemon` on the **session** bus, which is per-user and
unreachable from other accounts. It exposes three methods and one signal, all read-only
except `AckTopAlert`, which only marks an alert acknowledged. There is no method that
changes a monitoring setting, runs anything, or reads a caller-supplied path. Replies are
addressed back to the caller's unique name; the service never broadcasts state to
anything but its own signal.

## Uninstalling privilege

```bash
# Metric helper (CPU package power, NVMe SMART)
sudo systemctl disable --now jamsys-helper.timer
sudo rm -f /usr/libexec/jamsys-helper

# Keyboard write helper
sudo rm -f /usr/libexec/jamsys-kbd /usr/share/polkit-1/actions/org.jamsys.keyboard.policy
```

The daemon notices `/run/jamsys/privileged.json` going stale (> 5 min) and downgrades
those two metrics to `Unsupported` on the Coverage page. The Hardware page notices the
keyboard helper is gone and shows the lighting controls as read-only, with instructions
for both ways to re-enable them. Nothing else changes, and monitoring is unaffected —
which is the whole reason lighting lives in a separate binary.
