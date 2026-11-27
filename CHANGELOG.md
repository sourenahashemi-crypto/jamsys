# Changelog

## 1.1.1 — 2026-11-27

### Fixed

- The corner widget in GNOME Shell could be resized but not moved: the drag read
  the pan offset as `.x`/`.y`, which a `Graphene.Vec2` does not have, so every drag
  moved it by zero. The test stub returned the same wrong shape; it and a check
  against the real Clutter API now match the real return value.
- An iGPU with no reading showed a dash with a percent sign on a lit dial.

## 1.1.0 — 2026-11-27 (superseded by 1.1.1; its release was withdrawn)

### Security monitoring (new)

- **Security page** with a checkup: firewall, network exposure, automatic security
  updates, updates waiting, restart needed, automatic login, extra root accounts,
  SSH server settings, SSH key permissions, disk encryption and Secure Boot. Each
  line is either fine or not, with the one command that fixes it.
- **Firewall switch**: turn `ufw` on or off from the window, through pkexec.
- **Alerts** for a new program listening on the network, repeated failed logins,
  the firewall being off while something is reachable, the router's hardware address
  changing mid-connection (ARP spoofing), and a new USB device that presents itself
  as a keyboard.
- Detection only, all without root. JamSys is not a firewall and does not block.

### Fixed

- The GNOME Shell extension could not load on GNOME 50 (`Clutter.DragAction` and
  `get_click_count()` were removed); ported to `PanGesture` / `ClickGesture`, and a
  dragged position can now be saved.
- Per-process I/O read zero for every process: the user unit's sandbox put the daemon
  in a user namespace without protecting anything.
- IPC replies larger than the socket buffer were truncated and never completed.
- False "Wi-Fi is dropping packets" warnings from counting benign drops as errors.
- Learned baselines are now saved and restored across restarts.
- Notifications that were rate-limited or could not be sent are retried; the
  re-notification cooldown works.
- Three ways to pin a CPU core at 100% (journal follower exit, lost session bus,
  an unread collector socket).
- Reachability probes no longer block the daemon for one timeout per target.
- Stale dwell timers, a suspend-drain check that could never fire, non-ASCII mount
  points never monitored, unvalidated configuration, two daemons sharing one socket,
  a disabled collector's readings lingering, undeduplicated journal errors, a failed
  battery read becoming 0%, and more — see `docs/support-matrix.md` §5a.
- The main window no longer closes menus, collapses rows or resets controls on every
  refresh; the threshold editor offers the thresholds a rule really has.
- The cluster draws non-Latin alert text, opens its menu from the keyboard, and
  describes itself to screen readers.

### Changed

- The security collector reads listening sockets via `sock_diag` (0.7 ms per sample,
  down from 5.5 ms through `/proc/net/tcp`).

## 1.0.0 — 2026-09-09

First release: Rust daemon, GTK4 window, GNOME Shell instrument cluster, standalone
cluster window, keyboard lighting and battery charge-limit helpers.
