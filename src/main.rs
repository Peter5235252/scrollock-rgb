//! scrollock-rgb — friendly RGB control for ScrollLock-driven keyboards.
//!
//! How it works (1a2c:6003 class): the RGB strip is wired to the ScrollLock
//! LED. Pressing ScrollLock toggles RGB. Pressing Caps/Num makes the desktop
//! send a combined LED report built from the XKB lock state; if ScrollLock is
//! not a real modifier, that report turns the ScrollLock LED off and the
//! keyboard latches RGB off. `fix` adds the standard `scrolllock:mod3` XKB
//! option to the *current desktop's own* keyboard config (Hyprland, Sway, KDE,
//! GNOME, niri, generic Wayland, or X11), so the ScrollLock state survives
//! Caps/Num presses on every compositor. After that the desktop owns the
//! toggle; the `guard` daemon follows it and only repairs Caps/Num kills. It
//! deliberately does not enforce a remembered toggle, because doing so would
//! fight the compositor (the old `--emulate-toggle` mode is opt-in only).
//!
//! Hyprland owns keyboard LEDs and re-syncs them to its XKB state on every key
//! press. If its keymap lacks `scrolllock:mod3`, it clears the ScrollLock LED
//! and any daemon that writes the LED back has to fight it every poll — that
//! is the RGB flicker. Adding the option makes Hyprland's LED reports stay on,
//! and the guard simply accepts them, so there is nothing to fight over.
//!
//! Memory-safety contract (no `unsafe` anywhere in this crate):
//! - Only safe `std` APIs (`fs`, `env`, `process`, `thread`, `time`,
//!   `collections`) + `clap` (safe).
//! - Sysfs values are allow-listed (`0`/`1` for brightness, `none` for
//!   trigger); sysfs paths are canonicalized and constrained to
//!   `/sys/devices/` + `brightness`/`trigger`.
//! - `--device` never becomes a path; paths come only from enumerating
//!   `/sys/class/input/inputN`. State-file names are built from validated
//!   `inputN` names only, under the fixed dir `/var/lib/scrollock-rgb`.
//! - Desktop config files are only edited below the user's home/config dir;
//!   symlinks that escape those roots are refused. Every edit is backed up.
//! - No panicking indexing: `strip_prefix`, checked parsing, `unwrap_or`
//!   only. All reads are tiny bounded sysfs/state/config files.

#![forbid(unsafe_code)]

mod desktop;
mod elevate;
mod util;
mod xkbconf;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};

use desktop::DesktopKind;
use elevate::{elevate_self, needs_elevation_error};
use util::{config_home, home_dir, is_root, run_output};
use xkbconf::{xkb_options_contain, XKB_OPTION};

const LEDS_CLASS: &str = "/sys/class/leds";
const INPUT_CLASS: &str = "/sys/class/input";
const STATE_DIR: &str = "/var/lib/scrollock-rgb";
const KEY_SCROLLLOCK: u16 = 70;
const KEY_CAPSLOCK: u16 = 58;
const KEY_NUMLOCK: u16 = 69;
const EV_KEY: u16 = 1;

#[derive(Parser, Debug)]
#[command(
    name = "scrollock-rgb",
    version,
    about = "RGB control for ScrollLock-driven keyboards (ScrollLock toggles RGB)",
    long_about = "RGB control for keyboards whose backlight follows the ScrollLock LED.\n\nEXAMPLES:\n  scrollock-rgb               show RGB state\n  scrollock-rgb on            turn RGB on\n  scrollock-rgb off           turn RGB off\n  scrollock-rgb toggle        flip RGB on/off\n  scrollock-rgb fix           one-time setup so ScrollLock toggles RGB\n  scrollock-rgb doctor        check what is wrong"
)]
struct Cli {
    /// Do not try pkexec/sudo elevation (internal flag, prevents recursion)
    #[arg(long, hide = true, global = true)]
    no_elevate: bool,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Show RGB state for your keyboards
    #[command(visible_aliases = ["st", "list", "show"])]
    Status {
        /// Only this keyboard: input name (input5), name part (SEMICO), or VID:PID
        #[arg(long)]
        device: Option<String>,
    },
    /// Turn RGB on
    #[command(visible_aliases = ["enable", "start"])]
    On {
        #[arg(long)]
        device: Option<String>,
        #[arg(long, hide = true)]
        all: bool,
        #[arg(long, hide = true)]
        isolate_locks: bool,
    },
    /// Turn RGB off
    #[command(visible_aliases = ["disable", "stop"])]
    Off {
        #[arg(long)]
        device: Option<String>,
        #[arg(long, hide = true)]
        all: bool,
    },
    /// Flip RGB on/off
    #[command(visible_aliases = ["tgl", "switch", "flip"])]
    Toggle {
        #[arg(long)]
        device: Option<String>,
        #[arg(long, hide = true)]
        all: bool,
    },
    /// Repair Caps/Num kills without fighting the desktop's ScrollLock state
    #[command(visible_aliases = ["watch", "daemon", "protect"])]
    Guard {
        #[arg(long)]
        device: Option<String>,
        #[arg(long, hide = true)]
        all: bool,
        /// Keep Caps/Num indicator LEDs off (legacy workaround for broken
        /// firmware; normally unnecessary after `scrollock-rgb fix`)
        #[arg(long)]
        suppress_lock_leds: bool,
        /// Deprecated alias: lock LEDs are kept on by default now
        #[arg(long, hide = true)]
        keep_lock_leds: bool,
        /// Toggle the LED yourself on ScrollLock key presses. Only needed for
        /// old/compositor-less setups where ScrollLock is not handled at all;
        /// this is what caused flicker on Hyprland before `fix`.
        #[arg(long)]
        emulate_toggle: bool,
        /// Check interval in ms (smaller = faster repair, more wakeups)
        #[arg(long, default_value_t = 50)]
        interval_ms: u64,
        /// Run only N checks then exit (0 = forever). Used for tests.
        #[arg(long, default_value_t = 0, hide = true)]
        iterations: u64,
        // Accepted for compatibility with the previously installed unit.
        #[arg(long, hide = true)]
        isolate_locks: bool,
    },
    /// Check what is wrong (XKB setup, LED state)
    #[command(visible_aliases = ["check", "diag"])]
    Doctor,
    /// One-time setup so ScrollLock toggles RGB (no root needed)
    #[command(
        name = "fix",
        visible_aliases = ["fix-xkb", "repair", "setup"]
    )]
    FixXkb {
        /// Show what would change without writing anything
        #[arg(long)]
        dry_run: bool,
        /// Undo the old system-wide symbols/pc patch (safe check)
        #[arg(long)]
        restore_legacy: bool,
    },
    /// Low-level hardware details (for bug reports)
    #[command(visible_aliases = ["info", "details"])]
    Probe,
}

// ---- sysfs model ----

#[derive(Debug, Clone)]
struct LedState {
    base: PathBuf, // e.g. /sys/class/leds/input5::scrolllock
    brightness_path: PathBuf,
    trigger_path: Option<PathBuf>,
    brightness: u8,
    max_brightness: u8,
    trigger_active: String, // e.g. "none" parsed from "[none]"
    trigger_raw: String,
}

#[derive(Debug, Clone)]
struct Keyboard {
    input: String, // e.g. "input5"
    device_name: String,
    vid_pid: Option<String>,
    events: Vec<String>,
    scroll: LedState,
    caps: Option<LedState>,
    num: Option<LedState>,
}

fn read_trim(path: &Path) -> Option<String> {
    // Sysfs files are tiny; read_to_string fails on non-UTF8 -> None.
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// Extract active trigger from e.g. "[none] usb-gadget ... kbd-scrolllock".
fn parse_trigger_active(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b']' && j - i < 64 {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b']' {
                let s = raw.get(i + 1..j).unwrap_or("").trim().to_string();
                if !s.is_empty() && s.len() <= 32 {
                    return s;
                }
            }
        }
        i += 1;
    }
    String::new()
}

fn valid_input_name(name: &str) -> bool {
    name.strip_prefix("input")
        .map(|n| !n.is_empty() && n.len() <= 8 && n.bytes().all(|b| b.is_ascii_digit()))
        .unwrap_or(false)
}

fn valid_led_name(name: &str) -> bool {
    let Some((head, tail)) = name.split_once("::") else {
        return false;
    };
    if !(tail == "scrolllock" || tail == "capslock" || tail == "numlock") {
        return false;
    }
    valid_input_name(head)
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b':' || b == b'_' || b == b'-')
}

fn find_vid_pid(input_sys: &Path) -> Option<String> {
    let mut cur = input_sys.join("device");
    if let Ok(canon) = fs::canonicalize(&cur) {
        cur = canon;
    }
    for _ in 0..8 {
        let v = cur.join("idVendor");
        let p = cur.join("idProduct");
        if v.is_file() && p.is_file() {
            let vid = read_trim(&v).unwrap_or_default();
            let pid = read_trim(&p).unwrap_or_default();
            let ok = vid.len() == 4
                && pid.len() == 4
                && vid.bytes().all(|b| b.is_ascii_hexdigit())
                && pid.bytes().all(|b| b.is_ascii_hexdigit());
            if ok {
                return Some(format!("{vid}:{pid}"));
            }
        }
        match cur.parent() {
            Some(par) if par != cur => cur = par.to_path_buf(),
            _ => break,
        }
    }
    None
}

fn read_led(input: &str, kind: &str) -> Option<LedState> {
    let led_name = format!("{input}::{kind}");
    if !valid_led_name(&led_name) {
        return None;
    }
    let base = Path::new(LEDS_CLASS).join(&led_name);
    if !base.is_dir() {
        return None;
    }
    let brightness_path = base.join("brightness");
    if !brightness_path.is_file() {
        return None;
    }
    let trigger_path = base.join("trigger");
    let has_trigger = trigger_path.is_file();
    let brightness = read_trim(&brightness_path)
        .and_then(|s| s.parse::<u8>().ok())
        .unwrap_or(0);
    let max_brightness = read_trim(&base.join("max_brightness"))
        .and_then(|s| s.parse::<u8>().ok())
        .unwrap_or(1)
        .min(16);
    let trigger_raw = if has_trigger {
        read_trim(&trigger_path).unwrap_or_default()
    } else {
        String::new()
    };
    let trigger_active = parse_trigger_active(&trigger_raw);
    Some(LedState {
        base,
        brightness_path,
        trigger_path: has_trigger.then_some(trigger_path),
        brightness,
        max_brightness,
        trigger_active,
        trigger_raw,
    })
}

fn discover() -> io::Result<Vec<Keyboard>> {
    let mut out = Vec::new();
    let dir = fs::read_dir(INPUT_CLASS)?;
    for entry in dir.flatten() {
        let name_os = entry.file_name();
        let Some(name) = name_os.to_str() else {
            continue;
        };
        if !valid_input_name(name) || name.contains("::") {
            continue;
        }
        let Some(scroll) = read_led(name, "scrolllock") else {
            continue;
        };
        let input_sys = Path::new(INPUT_CLASS).join(name);
        let device_name = read_trim(&input_sys.join("name")).unwrap_or_else(|| "(unknown)".into());
        let device_name = device_name.chars().take(128).collect::<String>();
        let vid_pid = find_vid_pid(&input_sys);

        let mut events = Vec::new();
        if let Ok(rd) = fs::read_dir(&input_sys) {
            for e in rd.flatten() {
                let en_os = e.file_name();
                let Some(en) = en_os.to_str() else { continue };
                let Some(rest) = en.strip_prefix("event") else {
                    continue;
                };
                if rest.is_empty() || rest.len() > 6 || !rest.bytes().all(|b| b.is_ascii_digit()) {
                    continue;
                }
                events.push(format!("/dev/input/{en}"));
            }
        }
        events.sort();
        events.truncate(8);

        out.push(Keyboard {
            input: name.to_string(),
            device_name,
            vid_pid,
            events,
            caps: read_led(name, "capslock"),
            num: read_led(name, "numlock"),
            scroll,
        });
    }
    out.sort_by(|a, b| a.input.cmp(&b.input));
    Ok(out)
}

fn matches(kb: &Keyboard, f: &str) -> bool {
    let f = f.to_lowercase();
    kb.input.to_lowercase().contains(&f)
        || kb.device_name.to_lowercase().contains(&f)
        || kb
            .vid_pid
            .as_deref()
            .unwrap_or("")
            .to_lowercase()
            .contains(&f)
}

/// Without `--device`, apply to every matching keyboard (friendliest default).
/// `_all` is accepted for compatibility with older installs and ignored.
fn filter<'a>(
    kbs: &'a [Keyboard],
    device: &Option<String>,
    _all: bool,
) -> Result<Vec<&'a Keyboard>, String> {
    if let Some(f) = device {
        let hit: Vec<&Keyboard> = kbs.iter().filter(|k| matches(k, f)).collect();
        if hit.is_empty() {
            return Err(format!("no keyboard matches '{f}' (try without --device)"));
        }
        return Ok(hit);
    }
    Ok(kbs.iter().collect())
}

// ---- intended-state memory (for the toggle-following guard) ----

/// State-file path for one keyboard. The file name is built from the
/// validated `inputN` name only, so no filter string can escape STATE_DIR.
fn intended_path(input: &str) -> Option<PathBuf> {
    if !valid_input_name(input) {
        return None;
    }
    Some(PathBuf::from(STATE_DIR).join(format!("intended-{input}")))
}

/// Last RGB intent (true = on). Missing/unreadable file means "no memory yet".
fn load_intended(input: &str) -> Option<bool> {
    load_intent_record(input).map(|(on, _)| on)
}

/// State-file record plus a write timestamp. The guard uses the timestamp to
/// tell a CLI `on`/`off` command from its own adoption of a compositor change.
fn load_intent_record(input: &str) -> Option<(bool, u128)> {
    let p = intended_path(input)?;
    let s = fs::read_to_string(&p).ok()?;
    let mut fields = s.split_whitespace();
    let on = match fields.next()? {
        "1" => true,
        "0" => false,
        _ => return None,
    };
    let stamp = fields.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    Some((on, stamp))
}

/// Remember RGB intent. Best effort: the boot daemon runs as root and can
/// write; an unprivileged CLI may not be able to — that is fine, the guard
/// re-derives intent from what it sees.
fn save_intended(input: &str, on: bool) {
    let Some(p) = intended_path(input) else {
        return;
    };
    if fs::create_dir_all(STATE_DIR).is_err() {
        return;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let _ = fs::write(p, format!("{}\n{}\n", if on { 1 } else { 0 }, stamp));
}

// ---- validated privileged writes ----

/// Validate that `path` is a real sysfs LED file before writing.
fn validate_sysfs_file(path: &Path, expect: &str) -> Result<PathBuf, String> {
    if path.as_os_str().len() > 256 {
        return Err("sysfs path too long".into());
    }
    let canon =
        fs::canonicalize(path).map_err(|e| format!("canonicalize {}: {e}", path.display()))?;
    let s = canon.to_string_lossy();
    if !s.starts_with("/sys/devices/") {
        return Err(format!(
            "refusing to write outside /sys/devices: {}",
            path.display()
        ));
    }
    match canon.file_name().and_then(|n| n.to_str()) {
        Some(n) if n == expect => Ok(canon),
        _ => Err(format!(
            "refusing to write unexpected file: {}",
            path.display()
        )),
    }
}

fn write_sysfs(path: &Path, expect: &str, value: &str) -> Result<(), String> {
    let ok = match expect {
        "brightness" => value == "0\n" || value == "1\n",
        "trigger" => value == "none\n",
        _ => false,
    };
    if !ok {
        return Err("refusing to write unexpected sysfs value".into());
    }
    let canon = validate_sysfs_file(path, expect)?;
    fs::write(&canon, value).map_err(|e| format!("write {}: {e}", path.display()))
}

fn write_trigger_none(led: &LedState) -> Result<(), String> {
    let Some(trigger_path) = &led.trigger_path else {
        return Ok(());
    };
    if led.trigger_active == "none" {
        return Ok(());
    }
    write_sysfs(trigger_path, "trigger", "none\n")
}

fn write_brightness(led: &LedState, value: u8) -> Result<(), String> {
    let v = if value > 0 { 1 } else { 0 };
    if led.brightness == v {
        return Ok(());
    }
    write_sysfs(
        &led.brightness_path,
        "brightness",
        if v > 0 { "1\n" } else { "0\n" },
    )
}

/// Refresh cached LED state from sysfs (bounded, non-panicking).
fn reread(led: &LedState) -> LedState {
    let mut l = led.clone();
    l.brightness = read_trim(&l.brightness_path)
        .and_then(|s| s.parse::<u8>().ok())
        .unwrap_or(l.brightness);
    if let Some(trigger_path) = &l.trigger_path {
        let raw = read_trim(trigger_path).unwrap_or_default();
        l.trigger_active = parse_trigger_active(&raw);
        l.trigger_raw = raw;
    }
    l
}

/// One-shot set of the ScrollLock LED only. Order: trigger `none` first
/// (detach kernel auto-follow), then brightness, then verify.
fn apply_scroll(kb: &Keyboard, want_on: bool) -> Result<bool, String> {
    let scroll = reread(&kb.scroll);
    let mut changed = false;
    if scroll.trigger_path.is_some() && scroll.trigger_active != "none" {
        write_trigger_none(&scroll)?;
        changed = true;
    }
    let want: u8 = if want_on { 1 } else { 0 };
    let scroll2 = reread(&kb.scroll);
    let cur: u8 = if scroll2.brightness > 0 { 1 } else { 0 };
    if cur != want {
        write_brightness(&scroll2, want)?;
        changed = true;
    }
    let verify = reread(&kb.scroll);
    let got: u8 = if verify.brightness > 0 { 1 } else { 0 };
    if got != want {
        return Err(format!(
            "{}: could not set RGB (wanted {}, got {}, trigger={})",
            kb.input, want, verify.brightness, verify.trigger_active
        ));
    }
    Ok(changed)
}

fn sysfs_writable(kb: &Keyboard) -> bool {
    path_writable(&kb.scroll.brightness_path)
        && kb
            .scroll
            .trigger_path
            .as_deref()
            .map(path_writable)
            .unwrap_or(true)
}

fn path_writable(path: &Path) -> bool {
    match fs::OpenOptions::new().write(true).open(path) {
        Ok(_) => true,
        Err(e) => e.kind() != io::ErrorKind::PermissionDenied,
    }
}

// ---- user commands ----

fn cmd_doctor() -> i32 {
    println!("== scrollock-rgb check ==");
    if is_root() {
        println!("note: running as root; session/config checks may be inaccurate.");
    }

    let session = desktop::detect_session();
    let home = home_dir();
    let config = config_home();
    println!("session: {}", session.label());

    let mut rc = 0;

    match desktop::xkb_option_supported(home.as_deref()) {
        Ok(base) => println!(
            "[OK] xkeyboard-config supports scrolllock:mod3 ({})",
            base.display()
        ),
        Err(e) => {
            println!("[FAIL] {e}");
            println!("       Install/update it: {}", util::xkb_install_hint());
            rc = 1;
        }
    }

    let runtime = desktop::runtime_xkb(&session);
    let configured = desktop::configured_xkb(&session, home.as_deref(), config.as_deref());
    let runtime_has = runtime
        .as_ref()
        .map(|r| xkb_options_contain(&r.options, XKB_OPTION))
        .unwrap_or(false);
    let configured_has = configured
        .as_ref()
        .map(|c| xkb_options_contain(&c.options, XKB_OPTION))
        .unwrap_or(false);
    let legacy = desktop::legacy_patch_present(home.as_deref());

    if runtime_has {
        let source = runtime
            .as_ref()
            .map(|r| r.source.as_str())
            .unwrap_or("session");
        println!("[OK] XKB: ScrollLock is mapped to Mod3 ({source})");
    } else if configured_has {
        let source = configured
            .as_ref()
            .map(|c| c.source.as_str())
            .unwrap_or("config");
        if runtime.is_some() {
            println!("[WARN] XKB: scrolllock:mod3 is configured ({source}) but not active yet.");
            println!("       Log out/in so the compositor rebuilds its keymap.");
            if session.kind == DesktopKind::Hyprland {
                println!("       Hyprland: `hyprctl reload` may be enough.");
            }
        } else {
            println!("[OK] XKB: scrolllock:mod3 is configured ({source}); live state unavailable");
        }
    } else if legacy {
        println!("[OK] XKB: old system-wide symbols/pc patch is active (obsolete)");
        println!("       Run `scrollock-rgb fix` to migrate to the clean per-desktop option.");
    } else {
        println!("[FAIL] XKB: ScrollLock is not mapped to Mod3 for this session.");
        println!("       Run `scrollock-rgb fix` (no root needed).");
        if session.kind == DesktopKind::Hyprland {
            println!("       Hyprland re-syncs LEDs to its XKB state on every key press;");
            println!("       without this option the RGB backlight will flicker while typing.");
        }
        rc = 1;
    }

    if legacy {
        println!("[WARN] old system-wide symbols/pc patch detected (obsolete).");
        println!("       Remove it safely with: scrollock-rgb fix --restore-legacy");
    }

    if let Some((_, stdout, _)) = run_output("systemctl", &["is-active", "scrollock-rgb.service"]) {
        let state = stdout.trim();
        if !state.is_empty() {
            if state == "active" {
                println!("[OK] guard service is active");
            } else {
                println!("[WARN] guard service: {state}");
            }
        }
    }

    match discover() {
        Ok(kbs) if kbs.is_empty() => {
            println!("[FAIL] no RGB keyboard found");
            rc = 1;
        }
        Ok(kbs) => {
            for k in &kbs {
                let st = if k.scroll.brightness > 0 {
                    "on "
                } else {
                    "off"
                };
                println!(
                    "[{}] {} ({}) Caps/Num lights={}/{}",
                    st,
                    k.input,
                    k.device_name,
                    k.caps.as_ref().map(|c| c.brightness).unwrap_or(0),
                    k.num.as_ref().map(|n| n.brightness).unwrap_or(0),
                );
            }
        }
        Err(e) => {
            println!("[FAIL] search: {e}");
            rc = 1;
        }
    }
    rc
}

fn cmd_status(filter_arg: Option<String>) -> i32 {
    match discover() {
        Ok(kbs) if kbs.is_empty() => {
            println!("no RGB keyboard found.");
            println!("hint: run `scrollock-rgb doctor`.");
            1
        }
        Ok(kbs) => {
            for k in &kbs {
                if let Some(f) = &filter_arg {
                    if !matches(k, f) {
                        continue;
                    }
                }
                let st = if k.scroll.brightness > 0 {
                    "on "
                } else {
                    "off"
                };
                println!(
                    "RGB {} — {} ({}) Caps light={} Num light={}",
                    st,
                    k.input,
                    k.device_name,
                    k.caps.as_ref().map(|c| c.brightness).unwrap_or(0),
                    k.num.as_ref().map(|n| n.brightness).unwrap_or(0),
                );
            }
            0
        }
        Err(e) => {
            eprintln!("search: {e}");
            1
        }
    }
}

fn cmd_set(want: Option<u8>, device: Option<String>, all: bool, no_elevate: bool) -> i32 {
    let kbs = match discover() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("search: {e}");
            return 1;
        }
    };
    if kbs.is_empty() {
        eprintln!("no RGB keyboard found");
        return 1;
    }
    let targets = match filter(&kbs, &device, all) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let turning_on = want.map(|v| v > 0).unwrap_or(true);
    if turning_on && !is_root() {
        let session = desktop::detect_session();
        let home = home_dir();
        let config = config_home();
        if !desktop::xkb_is_configured(&session, home.as_deref(), config.as_deref()) {
            eprintln!("note: ScrollLock is not set up as an XKB modifier for this session.");
            eprintln!("      Run `scrollock-rgb fix` once (no root) so Caps/Num cannot kill RGB.");
            if session.kind == DesktopKind::Hyprland {
                eprintln!("      Hyprland will make the RGB flicker until this is fixed.");
            }
            eprintln!("      Turning RGB on directly for now…");
        }
    }
    let mut rc = 0;
    for t in targets {
        let want_on = match want {
            Some(v) => v > 0,
            None => reread(&t.scroll).brightness == 0,
        };
        // Write the command marker first: the guard may poll the LED between
        // the brightness write and a later save, especially while Caps is on.
        save_intended(&t.input, want_on);
        match apply_scroll(t, want_on) {
            Ok(_) => {
                println!(
                    "{} ({}): RGB {}",
                    t.input,
                    t.device_name,
                    if want_on { "on" } else { "off" }
                );
            }
            Err(e) => {
                if !no_elevate && needs_elevation_error(&e) {
                    elevate_self();
                }
                eprintln!("{}: {e}", t.input);
                rc = 1;
            }
        }
    }
    rc
}

fn cmd_fix_xkb(dry_run: bool, restore_legacy: bool, no_elevate: bool) -> i32 {
    desktop::fix_xkb(dry_run, restore_legacy, no_elevate)
}

// ---- guard ----

enum MonitorMsg {
    Scroll(String),
    Lock(String),
    Closed(String),
}

/// Read one evdev node and report ScrollLock key presses.
///
/// `struct input_event` is 24 bytes on 64-bit Linux and 16 bytes on 32-bit
/// (the timeval at the front is pointer-sized). Parsing the tail keeps this
/// correct on both, unlike a hardcoded 24-byte read.
fn monitor_event_node(path: String, input: String, tx: mpsc::Sender<MonitorMsg>) {
    use std::io::Read;

    const EVENT_SIZE: usize = if cfg!(target_pointer_width = "64") {
        24
    } else {
        16
    };

    let Ok(mut file) = fs::File::open(&path) else {
        thread::sleep(Duration::from_millis(500));
        let _ = tx.send(MonitorMsg::Closed(path));
        return;
    };
    let mut buf = [0u8; EVENT_SIZE];
    loop {
        if file.read_exact(&mut buf).is_err() {
            break;
        }
        let off = EVENT_SIZE - 8;
        let ev_type = u16::from_le_bytes([buf[off], buf[off + 1]]);
        let ev_code = u16::from_le_bytes([buf[off + 2], buf[off + 3]]);
        let ev_value = i32::from_le_bytes([buf[off + 4], buf[off + 5], buf[off + 6], buf[off + 7]]);
        // EV_KEY, value 1 == press. Report ScrollLock toggles and Caps/Num
        // presses; follow mode uses them to tell a user toggle from a kill.
        if ev_type == EV_KEY && ev_value == 1 {
            let msg = match ev_code {
                KEY_SCROLLLOCK => Some(MonitorMsg::Scroll(input.clone())),
                KEY_CAPSLOCK | KEY_NUMLOCK => Some(MonitorMsg::Lock(input.clone())),
                _ => None,
            };
            if let Some(msg) = msg {
                if tx.send(msg).is_err() {
                    return;
                }
            }
        }
    }
    thread::sleep(Duration::from_millis(250));
    let _ = tx.send(MonitorMsg::Closed(path));
}

fn drain_monitor_messages(
    rx: &mpsc::Receiver<MonitorMsg>,
    intents: &mut HashMap<String, bool>,
    last_toggle: &mut HashMap<String, Instant>,
    monitored: &mut HashSet<String>,
) {
    while let Ok(msg) = rx.try_recv() {
        match msg {
            MonitorMsg::Scroll(input) => {
                let now = Instant::now();
                let duplicate = last_toggle
                    .get(&input)
                    .map(|t| now.duration_since(*t) < Duration::from_millis(120))
                    .unwrap_or(false);
                if duplicate {
                    continue;
                }
                last_toggle.insert(input.clone(), now);
                let intended = intents
                    .entry(input.clone())
                    .or_insert_with(|| load_intended(&input).unwrap_or(true));
                *intended = !*intended;
                save_intended(&input, *intended);
            }
            MonitorMsg::Lock(_) => {}
            MonitorMsg::Closed(path) => {
                monitored.remove(&path);
            }
        }
    }
}

/// One guard step for a single keyboard. Applies the intended RGB state and
/// repairs Caps/Num kills. Toggle intent is updated separately by monitoring
/// ScrollLock key events, so Caps/Num activity can never be mistaken for a
/// user toggle (which previously caused strobing/off flicker).
fn guard_step(
    kb: &Keyboard,
    intended_on: &mut bool,
    suppress_locks: bool,
) -> Result<Option<String>, String> {
    // Optional legacy workaround: a lit Caps/Num LED is what latches RGB off
    // on a few broken firmwares. After the XKB fix this is unnecessary.
    let mut lock_activity = false;
    if suppress_locks {
        for slot in [&kb.caps, &kb.num] {
            let Some(led) = slot else { continue };
            let cur = reread(led);
            let trig_bad = cur.trigger_path.is_some() && cur.trigger_active != "none";
            let lit = cur.brightness != 0;
            if trig_bad {
                write_trigger_none(&cur)?;
            }
            if lit {
                write_brightness(&reread(led), 0)?;
            }
            if trig_bad || lit {
                lock_activity = true;
            }
        }
    }

    // ScrollLock trigger always detached so the kernel never auto-drives it.
    let s = reread(&kb.scroll);
    if s.trigger_path.is_some() && s.trigger_active != "none" {
        write_trigger_none(&s)?;
    }

    let want: u8 = if *intended_on { 1 } else { 0 };
    let s2 = reread(&kb.scroll);
    let cur: u8 = if s2.brightness > 0 { 1 } else { 0 };
    if cur == want {
        return Ok(None);
    }
    write_brightness(&s2, want)?;
    let verify = reread(&kb.scroll);
    let got: u8 = if verify.brightness > 0 { 1 } else { 0 };
    if got != want {
        return Err(format!(
            "{}: could not set RGB (wanted {}, got {}, trigger={})",
            kb.input, want, verify.brightness, verify.trigger_active
        ));
    }
    if want == 1 && lock_activity {
        return Ok(Some(format!(
            "{}: RGB back on (a Caps/Num press had turned it off)",
            kb.input
        )));
    }
    if want == 1 {
        Ok(Some(format!("{}: RGB on", kb.input)))
    } else {
        Ok(Some(format!("{}: RGB off", kb.input)))
    }
}

/// State for the default "follow the compositor" guard mode.
///
/// The desktop owns the ScrollLock LED once `scrolllock:mod3` is active. The
/// guard must therefore *follow* LED changes (user toggles, compositor resets)
/// instead of enforcing a remembered toggle, otherwise the two fight and the
/// RGB flickers. Caps/Num kills are the only thing the guard repairs.
struct FollowState {
    last_on: bool,
    caps: Option<u8>,
    num: Option<u8>,
    /// Last state-file record seen, used to recognise `on`/`off`/`toggle`
    /// commands even when a lock LED is lit.
    file_record: Option<(bool, u128)>,
    /// A Caps/Num key press (or lock-LED activity) was seen recently.
    lock_until: Option<Instant>,
    /// A physical ScrollLock key press was seen recently; this is a user
    /// toggle, not a Caps/Num kill.
    scroll_until: Option<Instant>,
}

fn drain_follow_messages(
    rx: &mpsc::Receiver<MonitorMsg>,
    states: &mut HashMap<String, FollowState>,
    monitored: &mut HashSet<String>,
) {
    while let Ok(msg) = rx.try_recv() {
        match msg {
            MonitorMsg::Scroll(input) => {
                if let Some(state) = states.get_mut(&input) {
                    state.scroll_until = Some(Instant::now() + Duration::from_millis(250));
                }
            }
            MonitorMsg::Lock(input) => {
                if let Some(state) = states.get_mut(&input) {
                    state.lock_until = Some(Instant::now() + Duration::from_millis(400));
                }
            }
            MonitorMsg::Closed(path) => {
                monitored.remove(&path);
            }
        }
    }
}

fn lock_brightness(led: &Option<LedState>) -> Option<u8> {
    led.as_ref().map(|l| reread(l).brightness)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FollowAction {
    Keep,
    Adopt,
    Repair,
}

/// Decide whether an observed ScrollLock LED change is a user toggle / normal
/// compositor update (adopt it) or a Caps/Num kill (repair it).
fn follow_action(
    last_on: bool,
    cur_on: bool,
    recent_lock: bool,
    user_toggle: bool,
    command_changed: bool,
) -> FollowAction {
    if cur_on == last_on {
        FollowAction::Keep
    } else if command_changed || user_toggle {
        FollowAction::Adopt
    } else if !cur_on && last_on && recent_lock {
        FollowAction::Repair
    } else {
        FollowAction::Adopt
    }
}

/// Observe the LEDs, repair a Caps/Num kill, and otherwise adopt whatever the
/// compositor decided. A Caps/Num kill is a ScrollLock LED drop that happens
/// together with lock-LED activity; a normal ScrollLock toggle has no such
/// activity and is accepted.
fn guard_step_follow(
    kb: &Keyboard,
    state: &mut FollowState,
    suppress_locks: bool,
) -> Result<Option<String>, String> {
    let now = Instant::now();
    let mut caps = lock_brightness(&kb.caps);
    let mut num = lock_brightness(&kb.num);

    let lock_activity =
        caps != state.caps || num != state.num || caps.unwrap_or(0) > 0 || num.unwrap_or(0) > 0;
    if lock_activity {
        state.lock_until = Some(now + Duration::from_millis(400));
    }

    if suppress_locks {
        for led in [&kb.caps, &kb.num].into_iter().flatten() {
            let cur = reread(led);
            let trig_bad = cur.trigger_path.is_some() && cur.trigger_active != "none";
            let lit = cur.brightness != 0;
            if trig_bad {
                write_trigger_none(&cur)?;
            }
            if lit {
                write_brightness(&reread(led), 0)?;
            }
        }
        caps = lock_brightness(&kb.caps);
        num = lock_brightness(&kb.num);
    }
    state.caps = caps;
    state.num = num;

    // Keep the kernel from auto-driving the ScrollLock LED.
    let s = reread(&kb.scroll);
    if s.trigger_path.is_some() && s.trigger_active != "none" {
        write_trigger_none(&s)?;
    }

    let cur = reread(&kb.scroll);
    let cur_on = cur.brightness > 0;
    let recent_lock = state.lock_until.map(|t| now <= t).unwrap_or(false);
    let user_toggle = state.scroll_until.map(|t| now <= t).unwrap_or(false);
    let file_record = load_intent_record(&kb.input);
    let command_changed = file_record != state.file_record;
    if command_changed {
        state.file_record = file_record;
    }
    if std::env::var_os("SCROLLOCK_RGB_DEBUG").is_some() {
        eprintln!(
            "debug {} last_on={} cur_on={} caps={:?} num={:?} lock={} scroll_key={} command={}",
            kb.input, state.last_on, cur_on, caps, num, recent_lock, user_toggle, command_changed
        );
    }

    match follow_action(
        state.last_on,
        cur_on,
        recent_lock,
        user_toggle,
        command_changed,
    ) {
        FollowAction::Keep => Ok(None),
        FollowAction::Repair => {
            write_brightness(&cur, 1)?;
            let verify = reread(&kb.scroll);
            if verify.brightness == 0 {
                return Err(format!(
                    "{}: could not restore RGB (trigger={})",
                    kb.input, verify.trigger_active
                ));
            }
            state.lock_until = None;
            Ok(Some(format!(
                "{}: RGB back on (a Caps/Num press had turned it off)",
                kb.input
            )))
        }
        FollowAction::Adopt => {
            // A real user toggle, a CLI command, or a compositor reset:
            // accept it and remember it for the next boot. Each write gets a
            // fresh timestamp, so the next CLI command stays distinguishable.
            state.last_on = cur_on;
            state.lock_until = None;
            state.scroll_until = None;
            save_intended(&kb.input, cur_on);
            state.file_record = load_intent_record(&kb.input);
            if cur_on {
                Ok(Some(format!("{}: RGB on", kb.input)))
            } else {
                Ok(Some(format!("{}: RGB off", kb.input)))
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_guard(
    device: Option<String>,
    all: bool,
    suppress_locks: bool,
    emulate_toggle: bool,
    interval_ms: u64,
    iterations: u64,
    no_elevate: bool,
) -> i32 {
    let interval = Duration::from_millis(interval_ms.clamp(20, 2000));
    let pre = match discover() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("watch: {e}");
            return 1;
        }
    };
    let pre_targets = match filter(&pre, &device, all) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("watch: {e}");
            return 1;
        }
    };
    if pre_targets.is_empty() {
        eprintln!("watch: no matching keyboards");
        return 1;
    }
    if !no_elevate && pre_targets.iter().any(|t| !sysfs_writable(t)) {
        elevate_self();
    }

    // Warn about the Hyprland flicker when running from a user session.
    let session = desktop::detect_session();
    if session.kind == DesktopKind::Hyprland {
        let home = home_dir();
        let config = config_home();
        if !desktop::xkb_is_configured(&session, home.as_deref(), config.as_deref()) {
            eprintln!("warning: Hyprland controls the ScrollLock LED and scrolllock:mod3");
            eprintln!("         is not configured. The RGB may flicker while typing.");
            eprintln!("         Run `scrollock-rgb fix` once, then log out/in.");
        }
    }

    if !emulate_toggle {
        return cmd_guard_follow(
            &pre_targets,
            &device,
            all,
            interval,
            iterations,
            suppress_locks,
            no_elevate,
        );
    }

    // --emulate-toggle: the old pre-fix behaviour. Kept only as an explicit
    // escape hatch; the service uses follow mode so it cannot fight Hyprland.
    // Remember each keyboard's RGB intent across polls (and restarts, via
    // small state files). No file yet (first boot) means RGB on.
    let mut intents: HashMap<String, bool> = HashMap::new();
    for t in &pre_targets {
        let v = match load_intended(&t.input) {
            Some(v) => v,
            None => {
                save_intended(&t.input, true);
                true
            }
        };
        intents.insert(t.input.clone(), v);
    }

    // Monitor ScrollLock key presses on the keyboards' event nodes so toggle
    // intent follows the physical key. This is independent of Caps/Num LED
    // state, which previously caused the guard to misclassify kills as toggles.
    let (tx, rx) = mpsc::channel::<MonitorMsg>();
    let mut monitored: HashSet<String> = HashSet::new();
    let mut last_toggle: HashMap<String, std::time::Instant> = HashMap::new();

    println!(
        "watching {} keyboard(s) every {} ms — ScrollLock toggles RGB (Ctrl-C to stop)…",
        pre_targets.len(),
        interval.as_millis()
    );
    let mut n: u64 = 0;
    loop {
        let kbs = match discover() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("watch: {e}");
                return 1;
            }
        };
        let targets = match filter(&kbs, &device, all) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("watch: {e}");
                return 1;
            }
        };
        if targets.is_empty() {
            eprintln!("watch: no matching keyboards");
            return 1;
        }

        // Keep the event monitors in sync with hot-plugged keyboards.
        let desired: HashSet<String> = targets
            .iter()
            .flat_map(|t| t.events.iter().cloned())
            .collect();
        monitored.retain(|p| desired.contains(p));
        for t in &targets {
            for ev in &t.events {
                if monitored.insert(ev.clone()) {
                    let tx = tx.clone();
                    let input = t.input.clone();
                    let path = ev.clone();
                    thread::spawn(move || monitor_event_node(path, input, tx));
                }
            }
        }

        // Apply ScrollLock key toggles detected by the event monitor threads.
        drain_monitor_messages(&rx, &mut intents, &mut last_toggle, &mut monitored);

        for t in targets {
            // Catch a key event that arrived while we were discovering devices,
            // so a real ScrollLock-off toggle is not mistaken for a kill and
            // repaired for one poll (which would look like a flash).
            drain_monitor_messages(&rx, &mut intents, &mut last_toggle, &mut monitored);
            let intended =
                intents
                    .entry(t.input.clone())
                    .or_insert_with(|| match load_intended(&t.input) {
                        Some(v) => v,
                        None => {
                            save_intended(&t.input, true);
                            true
                        }
                    });
            // Pick up intent changes made by the CLI (or other processes).
            if let Some(v) = load_intended(&t.input) {
                *intended = v;
            }
            match guard_step(t, intended, suppress_locks) {
                Ok(Some(line)) => println!("{line}"),
                Ok(None) => {}
                Err(e) => {
                    if !no_elevate && needs_elevation_error(&e) {
                        elevate_self();
                    }
                    eprintln!("watch {}: {e}", t.input);
                }
            }
        }
        n += 1;
        if iterations > 0 && n >= iterations {
            return 0;
        }
        thread::sleep(interval);
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_guard_follow(
    pre_targets: &[&Keyboard],
    device: &Option<String>,
    all: bool,
    interval: Duration,
    iterations: u64,
    suppress_locks: bool,
    no_elevate: bool,
) -> i32 {
    let mut states: HashMap<String, FollowState> = HashMap::new();

    // Apply the remembered intent once at startup (so "off" survives a
    // reboot), then follow whatever the desktop does. No remembered toggle is
    // enforced during the session: Hyprland/KDE own the LED after `fix`.
    for t in pre_targets {
        let intended = load_intended(&t.input).unwrap_or_else(|| {
            save_intended(&t.input, true);
            true
        });
        let before = reread(&t.scroll);
        if (before.brightness > 0) != intended {
            if let Err(e) = write_brightness(&before, if intended { 1 } else { 0 }) {
                if !no_elevate && needs_elevation_error(&e) {
                    elevate_self();
                }
                eprintln!("watch {}: {e}", t.input);
            }
        }
        let now_on = reread(&t.scroll).brightness > 0;
        let caps = lock_brightness(&t.caps);
        let num = lock_brightness(&t.num);
        states.insert(
            t.input.clone(),
            FollowState {
                last_on: now_on,
                caps,
                num,
                file_record: load_intent_record(&t.input),
                lock_until: None,
                scroll_until: None,
            },
        );
    }

    // Key monitors distinguish a real ScrollLock toggle from a Caps/Num kill.
    let (tx, rx) = mpsc::channel::<MonitorMsg>();
    let mut monitored: HashSet<String> = HashSet::new();

    println!(
        "watching {} keyboard(s) every {} ms — following the desktop; repairing Caps/Num kills (Ctrl-C to stop)…",
        pre_targets.len(),
        interval.as_millis()
    );

    let mut n: u64 = 0;
    loop {
        let kbs = match discover() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("watch: {e}");
                return 1;
            }
        };
        let targets = match filter(&kbs, device, all) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("watch: {e}");
                return 1;
            }
        };
        if targets.is_empty() {
            eprintln!("watch: no matching keyboards");
            return 1;
        }
        states.retain(|input, _| targets.iter().any(|t| &t.input == input));

        // Register hot-plugged keyboards before draining their key events.
        for t in &targets {
            if !states.contains_key(&t.input) {
                let now_on = reread(&t.scroll).brightness > 0;
                states.insert(
                    t.input.clone(),
                    FollowState {
                        last_on: now_on,
                        caps: lock_brightness(&t.caps),
                        num: lock_brightness(&t.num),
                        file_record: load_intent_record(&t.input),
                        lock_until: None,
                        scroll_until: None,
                    },
                );
            }
        }

        let desired: HashSet<String> = targets
            .iter()
            .flat_map(|t| t.events.iter().cloned())
            .collect();
        monitored.retain(|p| desired.contains(p));
        for t in &targets {
            for ev in &t.events {
                if monitored.insert(ev.clone()) {
                    let tx = tx.clone();
                    let input = t.input.clone();
                    let path = ev.clone();
                    thread::spawn(move || monitor_event_node(path, input, tx));
                }
            }
        }
        drain_follow_messages(&rx, &mut states, &mut monitored);

        for t in targets {
            let Some(state) = states.get_mut(&t.input) else {
                continue;
            };
            match guard_step_follow(t, state, suppress_locks) {
                Ok(Some(line)) => println!("{line}"),
                Ok(None) => {}
                Err(e) => {
                    if !no_elevate && needs_elevation_error(&e) {
                        elevate_self();
                    }
                    eprintln!("watch {}: {e}", t.input);
                }
            }
        }
        n += 1;
        if iterations > 0 && n >= iterations {
            return 0;
        }
        thread::sleep(interval);
    }
}

fn cmd_probe() -> i32 {
    let session = desktop::detect_session();
    let home = home_dir();
    let config = config_home();
    println!("session : {}", session.label());
    match desktop::xkb_option_supported(home.as_deref()) {
        Ok(base) => println!("xkb data: {}", base.display()),
        Err(e) => println!("xkb data: unavailable ({e})"),
    }
    match desktop::runtime_xkb(&session) {
        Some(rt) => println!("xkb live: [{}] (from {})", rt.options, rt.source),
        None => println!("xkb live: unavailable"),
    }
    match desktop::configured_xkb(&session, home.as_deref(), config.as_deref()) {
        Some(cf) => println!("xkb conf: [{}] ({})", cf.options, cf.source),
        None => println!("xkb conf: no config file found"),
    }
    println!(
        "legacy  : {}",
        if desktop::legacy_patch_present(home.as_deref()) {
            "old symbols/pc patch present"
        } else {
            "none"
        }
    );
    println!();

    match discover() {
        Ok(kbs) if kbs.is_empty() => {
            println!("no RGB keyboard found.");
            1
        }
        Ok(kbs) => {
            for k in &kbs {
                println!("keyboard: {} ({})", k.input, k.device_name);
                println!("  id      : {}", k.vid_pid.as_deref().unwrap_or("?"));
                println!(
                    "  events  : {}",
                    if k.events.is_empty() {
                        "-".into()
                    } else {
                        k.events.join(", ")
                    }
                );
                println!(
                    "  scroll  : {} brightness={}/{} trigger={} (this LED is the RGB switch)",
                    k.scroll.base.display(),
                    k.scroll.brightness,
                    k.scroll.max_brightness,
                    if k.scroll.trigger_path.is_some() {
                        format!("[{}]", k.scroll.trigger_active)
                    } else {
                        "-".to_string()
                    }
                );
                for (label, slot) in [("caps", &k.caps), ("num", &k.num)] {
                    match slot {
                        Some(l) => println!(
                            "  {label:<7}: brightness={} trigger={}",
                            l.brightness,
                            if l.trigger_path.is_some() {
                                format!("[{}]", l.trigger_active)
                            } else {
                                "-".to_string()
                            }
                        ),
                        None => println!("  {label:<7}: -"),
                    }
                }
                println!(
                    "  intent  : {}",
                    match load_intended(&k.input) {
                        Some(true) => "on",
                        Some(false) => "off",
                        None => "on (default)",
                    }
                );
                println!();
            }
            0
        }
        Err(e) => {
            eprintln!("search: {e}");
            1
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let rc = match cli.cmd {
        None => cmd_status(None),
        Some(Cmd::Status { device }) => cmd_status(device),
        Some(Cmd::On { device, all, .. }) => cmd_set(Some(1), device, all, cli.no_elevate),
        Some(Cmd::Off { device, all }) => cmd_set(Some(0), device, all, cli.no_elevate),
        Some(Cmd::Toggle { device, all }) => cmd_set(None, device, all, cli.no_elevate),
        Some(Cmd::Guard {
            device,
            all,
            suppress_lock_leds,
            keep_lock_leds,
            emulate_toggle,
            interval_ms,
            iterations,
            ..
        }) => {
            let _ = keep_lock_leds;
            cmd_guard(
                device,
                all,
                suppress_lock_leds,
                emulate_toggle,
                interval_ms,
                iterations,
                cli.no_elevate,
            )
        }
        Some(Cmd::Doctor) => cmd_doctor(),
        Some(Cmd::FixXkb {
            dry_run,
            restore_legacy,
        }) => cmd_fix_xkb(dry_run, restore_legacy, cli.no_elevate),
        Some(Cmd::Probe) => cmd_probe(),
    };
    process::exit(rc);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_parsing() {
        assert_eq!(
            parse_trigger_active("[none] usb-gadget kbd-scrolllock"),
            "none"
        );
        assert_eq!(
            parse_trigger_active("default [kbd-scrolllock] kbd-numlock"),
            "kbd-scrolllock"
        );
        assert_eq!(parse_trigger_active("no brackets"), "");
    }

    #[test]
    fn input_and_led_names_are_validated() {
        assert!(valid_input_name("input4"));
        assert!(valid_input_name("input123"));
        assert!(!valid_input_name("input"));
        assert!(!valid_input_name("input4::scrolllock"));
        assert!(!valid_input_name("input4/../x"));
        assert!(valid_led_name("input4::scrolllock"));
        assert!(valid_led_name("input4::capslock"));
        assert!(!valid_led_name("input4::backlight"));
        assert!(!valid_led_name("../../input4::scrolllock"));
    }

    #[test]
    fn follow_mode_adopts_toggles_and_repairs_kills() {
        // A normal ScrollLock-off toggle has no lock-key activity.
        assert_eq!(
            follow_action(true, false, false, false, false),
            FollowAction::Adopt
        );
        // A scroll-off press has a ScrollLock key event even if a lock LED
        // also changed in the same poll.
        assert_eq!(
            follow_action(true, false, true, true, false),
            FollowAction::Adopt
        );
        // An explicit CLI `off` wins even when Caps is lit.
        assert_eq!(
            follow_action(true, false, true, false, true),
            FollowAction::Adopt
        );
        // Caps/Num activity + LED drop without a ScrollLock event is a kill.
        assert_eq!(
            follow_action(true, false, true, false, false),
            FollowAction::Repair
        );
        // Off stays off; turning on is always accepted.
        assert_eq!(
            follow_action(false, false, true, false, false),
            FollowAction::Keep
        );
        assert_eq!(
            follow_action(false, true, false, false, false),
            FollowAction::Adopt
        );
    }

    #[test]
    fn matching_is_case_insensitive() {
        let led = LedState {
            base: PathBuf::from("/tmp"),
            brightness_path: PathBuf::from("/tmp"),
            trigger_path: None,
            brightness: 0,
            max_brightness: 1,
            trigger_active: String::new(),
            trigger_raw: String::new(),
        };
        let kb = Keyboard {
            input: "input4".into(),
            device_name: "SEMICO USB Keyboard".into(),
            vid_pid: Some("1a2c:6003".into()),
            events: vec![],
            scroll: led,
            caps: None,
            num: None,
        };
        assert!(matches(&kb, "semico"));
        assert!(matches(&kb, "1A2C"));
        assert!(!matches(&kb, "0c45"));
    }
}
