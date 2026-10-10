# scrollock-rgb

> Coded by Meta's Muse Spark 1.3 (AI coding agent), directed by the repo owner.

CLI-first RGB control for Scroll-Lock-driven keyboards on Linux.
Targets CachyOS / Arch + KDE Plasma Wayland first.

Tested on: `1a2c:6003 SEMICO USB Keyboard` (`/sys/class/leds/input5::scrolllock`).

## Why Caps/Num kill RGB (real cause)

`Handlers=kbd leds` owns the LEDs. `Scroll_Lock` had a keysym but no
`modifier_map`, so kernel Scroll state stayed OFF. Every Caps/Num press
sends a combined LED report built from lock states (Scroll=OFF) and kills
RGB instantly. `trigger=none` + `brightness=1` + polling `guard` can only
repair *after* the kill (visible blink/off) — it cannot prevent it.

Permanent fix (verified Mint solution, ported to Arch): make ScrollLock
a real Mod3 modifier in XKB, reboot, press ScrollLock once. Then kernel
state is ON and Caps/Num reports include Scroll=ON → RGB survives.

## Permanent fix (do once)

```sh
scrollock-rgb doctor    # shows [FAIL] XKB before fix
scrollock-rgb fix-xkb   # pkexec patches /usr/share/X11/xkb/symbols/pc + backup
# reboot (or logout/login — KWin Wayland reads XKB at session start)
# press the physical ScrollLock key once → RGB on, survives Caps/Num
scrollock-rgb doctor    # shows [OK] XKB
```

Backup: `/usr/share/X11/xkb/symbols/pc.bak-scrollock-rgb`.
Re-run `fix-xkb` after `xkeyboard-config` package updates (they overwrite `/usr/share`).

## Build

```sh
cargo build --release
./target/release/scrollock-rgb status
```

## Usage (no flags needed for one keyboard)

```sh
scrollock-rgb            # same as status
scrollock-rgb status     # show RGB state  (aliases: st, list, show)
scrollock-rgb on         # RGB on          (aliases: enable, start)
scrollock-rgb off        # RGB off         (aliases: disable, stop)
scrollock-rgb toggle     # flip it         (aliases: tgl, switch, flip)
scrollock-rgb fix        # one-time setup so the ScrollLock key toggles RGB
scrollock-rgb doctor     # check what is wrong (aliases: check, diag)
scrollock-rgb guard      # keep it alive across Caps/Num (aliases: watch, daemon)
scrollock-rgb probe      # hardware details for bug reports
```

With several keyboards, everything applies to all of them; add
`--device input5` (or part of the name, or `1a2c:6003`) to pick one.

- ScrollLock key toggles RGB (after `fix` + reboot). The guard follows your
  toggle: it never fights the key, so no strobing.
- Caps/Num keys keep working (locking still locks); only their indicator
  lights stay off, because on this firmware a lit Caps/Num LED cuts RGB
  power. `guard --keep-lock-leds` leaves them alone if you prefer the lights.
- Needs permission for keyboard LEDs → asks once via the system password
  prompt (pkexec); the boot service runs as root so it never asks.

## Permanent rootless access (optional)

```sh
sudo cp udev/99-scrollock-rgb.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=leds
```

## Autostart on boot (installed + verified)

Two system-level mechanisms, both root at boot, no password at runtime:

- `udev/99-scrollock-rgb.rules` → `/etc/udev/rules.d/` — `ACTION=="add"`
  `RUN` forces `trigger=none` + `brightness=1` on boot/replug (login-screen RGB).
- `systemd/scrollock-rgb.service` → `/etc/systemd/system/`,
  `ExecStart=/usr/local/bin/scrollock-rgb guard --interval-ms 50`,
  `Restart=always`, `WantedBy=multi-user.target` — persistent guard.

What the guard does every 50 ms: keeps the ScrollLock trigger detached,
follows your ScrollLock toggle (on stays on, off stays off — no fight, no
strobing), repairs a Caps/Num kill within one poll, and keeps Caps/Num
lights off so the firmware can never latch RGB off. A Caps/Num press can
still flip the hardware for <50 ms before repair; only a kernel-level key
filter could remove even that.
Verify: `systemctl is-enabled scrollock-rgb.service` → `enabled`,
`systemctl status scrollock-rgb.service` → `active (running)`.

## Memory safety

- `#![forbid(unsafe_code)]`: no `unsafe` in this crate (`src/main.rs:14`).
- Only safe `std` + `clap`; sysfs values allow-listed (`0`/`1`/`none`),
  paths canonicalized to `/sys/devices/` + `brightness`/`trigger`,
  XKB path allow-listed to `*/symbols/pc`, size-bounded (<1MB).
- No `unwrap`/`expect`/`panic!` on untrusted input; `clippy -D warnings` clean.

## Authorship and license

Coded by Meta's Muse Spark 1.3 (AI coding agent, working through OpenCode),
directed, tested, and published by the repository owner. Licensed MIT —
see `LICENSE`.
