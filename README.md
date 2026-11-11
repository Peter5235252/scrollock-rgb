# scrollock-rgb

> Coded by models in Cline and OpenCode, directed by the repo owner.

CLI-first RGB control for keyboards whose backlight follows the ScrollLock LED.
Works with KDE Plasma, GNOME, Hyprland, Sway, niri, other wlroots compositors,
and plain X11 — on Arch, Debian/Ubuntu, Fedora, openSUSE, Alpine, Void, Gentoo,
NixOS and anything else with Linux input/LED sysfs.

Tested hardware: `1a2c:6003 SEMICO USB Keyboard`.
The utility discovers the real `inputN::scrolllock` automatically, so the
number changing between boots/replugs does not matter.

## Why Caps/Num kill RGB (and why Hyprland flickers)

The RGB strip is wired to the ScrollLock **LED**. Every Caps/Num press makes
the desktop send a combined LED report based on its XKB lock state. If
ScrollLock is not a real modifier, that report contains “ScrollLock = off” and
the keyboard latches the backlight off.

Hyprland is stricter: it owns the keyboard LEDs and re-syncs them to its XKB
state on every key press. Without the right XKB option it clears the
ScrollLock LED after every key press; any daemon that writes the LED back just
creates a fight, which is visible as flicker.

The fix is the standard xkeyboard-config option:

```
scrolllock:mod3
```

It makes ScrollLock a real Mod3 modifier, so Caps/Num LED reports keep
“ScrollLock = on”. `scrollock-rgb fix` applies that option to **your desktop's
own config** — it no longer patches `/usr/share/X11/xkb/symbols/pc`.

## One-time fix (no root needed)

```sh
scrollock-rgb doctor    # shows what is wrong
scrollock-rgb fix       # detects KDE/GNOME/Hyprland/Sway/niri/X11 and configures it
# log out and back in (Hyprland: `hyprctl reload` is often enough)
# press the physical ScrollLock key once → RGB on, Caps/Num can no longer kill it
scrollock-rgb doctor    # shows [OK]
```

What `fix` writes:

| Session | Configuration |
| --- | --- |
| Hyprland | `kb_options = "…,scrolllock:mod3"` in `hyprland.conf` or `hyprland.lua`; applied live with `hyprctl` |
| Sway | `input "type:keyboard" xkb_options …,scrolllock:mod3`; applied live with `swaymsg` |
| KDE Plasma | `[Layout] Options=…,scrolllock:mod3` in `~/.config/kxkbrc` |
| GNOME | `org.gnome.desktop.input-sources xkb-options` via `gsettings` |
| niri | `input { keyboard { xkb { options "…,scrolllock:mod3" } } }` in `config.kdl` |
| X11 | `setxkbmap -option …` now, plus a tagged block in `~/.xprofile`/`~/.xsessionrc` |
| Other Wayland | `XKB_DEFAULT_OPTIONS` in `~/.config/environment.d/90-scrollock-rgb.conf` |

Every changed file gets a `*.bak-scrollock-rgb` backup. Existing options are
preserved; the command is idempotent. `fix --dry-run` shows the planned change
without writing anything.

Hyprland users can also add it manually (append to existing options, do not
replace them):

```ini
input {
    kb_options = scrolllock:mod3
}
```

Newer Hyprland uses Lua:

```lua
hl.config({ input = { kb_options = "scrolllock:mod3" } })
```

The option comes from `xkeyboard-config` and is available on every current
distro. If it is missing, `fix` prints the exact package command for your
distro (`pacman`, `apt`, `dnf`, `zypper`, `apk`, `xbps-install`, `emerge`, …).

## Usage

```sh
scrollock-rgb            # same as status
scrollock-rgb status     # show RGB state  (aliases: st, list, show)
scrollock-rgb on         # RGB on          (aliases: enable, start)
scrollock-rgb off        # RGB off         (aliases: disable, stop)
scrollock-rgb toggle     # flip it         (aliases: tgl, switch, flip)
scrollock-rgb fix        # one-time XKB setup (aliases: fix-xkb, repair, setup)
scrollock-rgb doctor     # check what is wrong (aliases: check, diag)
scrollock-rgb guard      # repair Caps/Num kills (aliases: watch, daemon)
scrollock-rgb probe      # hardware + session details for bug reports
```

With several keyboards everything applies to all of them. Add
`--device input4` (or part of the name, or `1a2c:6003`) to pick one.

- After `fix`, the desktop handles ScrollLock toggling itself; the guard only
  follows the LED and repairs a Caps/Num kill. It deliberately does not enforce
  a remembered toggle, so it cannot fight the compositor and cause flicker.
- The old pre-fix behaviour (the guard toggles the LED on every ScrollLock key
  press) is still available as `guard --emulate-toggle` for setups where the
  compositor does not handle ScrollLock at all. It may flicker; prefer `fix`.
- After `fix`, Caps/Num indicator LEDs stay on normally. The old workaround
  that kept them off is still available as `guard --suppress-lock-leds` for
  firmware that really cuts RGB when a lock LED is lit.
- Needs write permission for keyboard LEDs → it opens the desktop's
  authentication dialog (polkit) and falls back to `sudo`, then `doas`.
  The system service runs as root, so it never asks.

## Build

```sh
cargo build --release
./target/release/scrollock-rgb status
./target/release/scrollock-rgb doctor
```

Rust 1.70+; no `unsafe`; only `std` + `clap`.

## Autostart (optional but recommended)

The guard repairs Caps/Num kills and follows the desktop's ScrollLock LED; the
udev rule turns RGB on at boot/replug. After `fix`, the compositor is
responsible for the ScrollLock toggle itself.

Install the binary and files (pick the block for your init system):

```sh
# binary (absolute source path works from any directory)
sudo install -m 755 "$PWD/target/release/scrollock-rgb" /usr/local/bin/scrollock-rgb

# udev rule (all distros)
sudo install -m 644 "$PWD/udev/99-scrollock-rgb.rules" /etc/udev/rules.d/
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=leds
```

systemd (Arch, Debian, Fedora, openSUSE, …):

```sh
sudo install -m 644 "$PWD/systemd/scrollock-rgb.service" /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now scrollock-rgb.service
```

OpenRC (Alpine, Gentoo, Artix-OpenRC, …):

```sh
sudo install -m 755 "$PWD/openrc/scrollock-rgb" /etc/init.d/scrollock-rgb
sudo rc-update add scrollock-rgb default
sudo rc-service scrollock-rgb start
```

runit (Void, Artix-runit, …):

```sh
sudo mkdir -p /etc/sv/scrollock-rgb
sudo install -m 755 "$PWD/runit/scrollock-rgb/run" /etc/sv/scrollock-rgb/run
sudo ln -s /etc/sv/scrollock-rgb /var/service/scrollock-rgb
```

dinit (Chimera, Artix-dinit, …):

```sh
sudo install -m 644 "$PWD/dinit/scrollock-rgb" /etc/dinit.d/scrollock-rgb
sudo dinitctl enable scrollock-rgb
```

Verify:

```sh
systemctl status scrollock-rgb   # or: rc-service scrollock-rgb status
scrollock-rgb doctor
```

The udev rule and guard are independent. The rule sets `trigger=none` and RGB
on when the LED device appears; the guard follows the compositor's LED changes
and repairs firmware kills caused by Caps/Num.

## Legacy system-wide patch

Very old versions of this tool appended `modifier_map Mod3 { Scroll_Lock };`
to `/usr/share/X11/xkb/symbols/pc`. That is no longer needed and can be undone
without clobbering package updates:

```sh
scrollock-rgb fix --restore-legacy --dry-run
scrollock-rgb fix --restore-legacy
```

It only restores when the backup exactly matches the current file minus the
inserted line; otherwise it refuses and tells you to reinstall the package.

## Troubleshooting

- `doctor` says the option is configured but not active → log out/in. KDE reads
  `kxkbrc` at login; Hyprland may apply `hyprctl reload`.
- Hyprland still flickers → check `hyprctl getoption input:kb_options` contains
  `scrolllock:mod3`. If it does not, run `fix` again and reload/relogin.
- Flicker persists with the option active → make sure the service is not using
  `guard --emulate-toggle`; normal follow mode never fights the compositor.
- No keyboard found → `scrollock-rgb probe`; make sure the kernel exposes
  `inputN::scrolllock` under `/sys/class/leds`.
- Permission denied despite the authentication prompt → install polkit; the
  error prints the correct package command for your distro.

## Memory safety

- `#![forbid(unsafe_code)]`: no `unsafe` in this crate.
- Only safe `std` + `clap`; sysfs values allow-listed (`0`/`1`/`none`), paths
  canonicalized to `/sys/devices/` + `brightness`/`trigger`.
- Config files are only edited below your home/config directory; symlinks that
  escape those roots are refused. Every edit is backed up and written
  atomically.
- No `unwrap`/`expect`/`panic!` on untrusted input; `clippy -D warnings` clean.

## Authorship and license

Coded by models in Cline and OpenCode, directed and tested by the repository
owner. Licensed MIT — see `LICENSE`.
