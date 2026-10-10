//! scrollock-rgb — friendly RGB control for ScrollLock-driven keyboards.
//!
//! How it works (1a2c:6003 class): the RGB strip is wired to the ScrollLock
//! LED. Pressing ScrollLock toggles RGB (needs the `fix` one-time setup).
//! Pressing Caps/Num makes the kernel send a combined LED report that can
//! turn the ScrollLock LED off and light the Caps/Num LEDs; on some firmware
//! a lit Caps/Num LED cuts RGB power and latches it off. The `guard` daemon
//! repairs kills and keeps Caps/Num LEDs off, and it *follows* your
//! ScrollLock toggle instead of fighting it (no strobing).
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
//! - No panicking indexing: `strip_prefix`, checked parsing, `unwrap_or`
//!   only. All reads are tiny bounded sysfs/state files.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::time::Duration;

use clap::{Parser, Subcommand};

const LEDS_CLASS: &str = "/sys/class/leds";
const INPUT_CLASS: &str = "/sys/class/input";
const STATE_DIR: &str = "/var/lib/scrollock-rgb";

#[derive(Parser, Debug)]
#[command(
    name = "scrollock-rgb",
    version,
    about = "RGB control for ScrollLock-driven keyboards (ScrollLock toggles RGB)",
    long_about = "RGB control for keyboards whose backlight follows the ScrollLock LED.\n\nEXAMPLES:\n  scrollock-rgb               show RGB state\n  scrollock-rgb on            turn RGB on\n  scrollock-rgb off           turn RGB off\n  scrollock-rgb toggle        flip RGB on/off\n  scrollock-rgb fix           one-time setup so ScrollLock toggles RGB (then reboot)\n  scrollock-rgb doctor        check what is wrong"
)]
struct Cli {
    /// Do not try pkexec elevation (internal flag, prevents recursion)
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
    /// Keep RGB on across Caps/Num presses; follows your ScrollLock toggle
    #[command(visible_aliases = ["watch", "daemon", "protect"])]
    Guard {
        #[arg(long)]
        device: Option<String>,
        #[arg(long, hide = true)]
        all: bool,
        /// Leave Caps/Num indicator lights alone (default: keep them off so
        /// they cannot cut RGB power on firmware where the circuits interact)
        #[arg(long)]
        keep_lock_leds: bool,
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
    /// One-time setup so ScrollLock toggles RGB (needs reboot after)
    #[command(visible_aliases = ["fix", "repair", "setup"])]
    FixXkb,
    /// Low-level hardware details (for bug reports)
    #[command(visible_aliases = ["info", "details"])]
    Probe,
}

// ---- sysfs model ----

#[derive(Debug, Clone)]
struct LedState {
    base: PathBuf, // e.g. /sys/class/leds/input5::scrolllock
    brightness_path: PathBuf,
    trigger_path: PathBuf,
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
    let trigger_path = base.join("trigger");
    if !brightness_path.is_file() || !trigger_path.is_file() {
        return None;
    }
    let brightness = read_trim(&brightness_path)
        .and_then(|s| s.parse::<u8>().ok())
        .unwrap_or(0);
    let max_brightness = read_trim(&base.join("max_brightness"))
        .and_then(|s| s.parse::<u8>().ok())
        .unwrap_or(1)
        .min(16);
    let trigger_raw = read_trim(&trigger_path).unwrap_or_default();
    let trigger_active = parse_trigger_active(&trigger_raw);
    Some(LedState {
        base,
        brightness_path,
        trigger_path,
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
    let p = intended_path(input)?;
    // State files are written by us as "1\n"/"0\n"; bound the read anyway.
    let s = fs::read_to_string(&p).ok()?;
    match s.trim() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
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
    let _ = fs::write(p, if on { "1\n" } else { "0\n" });
}

// ---- validated privileged writes + pkexec ----

/// Validate that `path` is a real sysfs LED file before writing.
fn validate_sysfs_file(path: &Path, expect: &str) -> Result<PathBuf, String> {
    if path.as_os_str().len() > 256 {
        return Err("sysfs path too long".into());
    }
    let canon = fs::canonicalize(path).map_err(|e| format!("canonicalize {}: {e}", path.display()))?;
    let s = canon.to_string_lossy();
    if !s.starts_with("/sys/devices/") {
        return Err(format!("refusing to write outside /sys/devices: {}", path.display()));
    }
    match canon.file_name().and_then(|n| n.to_str()) {
        Some(n) if n == expect => Ok(canon),
        _ => Err(format!("refusing to write unexpected file: {}", path.display())),
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
    if led.trigger_active == "none" {
        return Ok(());
    }
    write_sysfs(&led.trigger_path, "trigger", "none\n")
}

fn write_brightness(led: &LedState, value: u8) -> Result<(), String> {
    let v = if value > 0 { 1 } else { 0 };
    if led.brightness == v {
        return Ok(());
    }
    write_sysfs(&led.brightness_path, "brightness", if v > 0 { "1\n" } else { "0\n" })
}

/// Refresh cached LED state from sysfs (bounded, non-panicking).
fn reread(led: &LedState) -> LedState {
    let mut l = led.clone();
    l.brightness = read_trim(&l.brightness_path)
        .and_then(|s| s.parse::<u8>().ok())
        .unwrap_or(l.brightness);
    let raw = read_trim(&l.trigger_path).unwrap_or_default();
    l.trigger_active = parse_trigger_active(&raw);
    l.trigger_raw = raw;
    l
}

/// One-shot set of the ScrollLock LED only. Order: trigger `none` first
/// (detach kernel auto-follow), then brightness, then verify.
fn apply_scroll(kb: &Keyboard, want_on: bool) -> Result<bool, String> {
    let scroll = reread(&kb.scroll);
    let mut changed = false;
    if scroll.trigger_active != "none" {
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

fn needs_elevation_error(msg: &str) -> bool {
    msg.contains("Permission denied") || msg.contains("permission denied")
}

fn elevate_self() -> ! {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/proc/self/exe"));
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !Path::new("/usr/bin/pkexec").exists() && !Path::new("/bin/pkexec").exists() {
        eprintln!("need permission to touch keyboard LEDs and pkexec is missing.");
        eprintln!("Run as root, or install udev/99-scrollock-rgb.rules (boot service covers boot).");
        process::exit(1);
    }
    eprintln!("need permission, asking via pkexec…");
    let mut cmd = Command::new("pkexec");
    cmd.arg(exe);
    cmd.args(&args);
    if !args.iter().any(|a| a == "--no-elevate") {
        cmd.arg("--no-elevate");
    }
    match cmd.status() {
        Ok(st) => process::exit(st.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("failed to run pkexec: {e}");
            process::exit(1);
        }
    }
}

// ---- XKB one-time setup ----

const XKB_PC: &str = "/usr/share/X11/xkb/symbols/pc";
const XKB_LINE: &str = "    modifier_map Mod3 { Scroll_Lock };";

fn xkb_pc_path() -> Result<PathBuf, String> {
    let p = Path::new(XKB_PC);
    let canon = fs::canonicalize(p).map_err(|e| format!("xkb file missing {XKB_PC}: {e}"))?;
    if canon.file_name().and_then(|n| n.to_str()) != Some("pc") {
        return Err(format!("refusing unexpected xkb path: {}", canon.display()));
    }
    if canon.parent().and_then(|n| n.file_name()).and_then(|n| n.to_str()) != Some("symbols") {
        return Err(format!("refusing unexpected xkb path: {}", canon.display()));
    }
    let s = canon.to_string_lossy();
    if !(s.starts_with("/usr/share/X11/xkb/symbols/")
        || s.starts_with("/usr/share/xkeyboard-config-2/symbols/"))
    {
        return Err(format!("refusing unexpected xkb path: {}", canon.display()));
    }
    Ok(canon)
}

fn xkb_fixed() -> bool {
    let Ok(text) = fs::read_to_string(XKB_PC) else {
        return false;
    };
    for line in text.lines().take(500) {
        let t = line.trim();
        if t.starts_with("//") || t.starts_with('#') {
            continue;
        }
        if t.contains("modifier_map") && t.contains("Mod3") && t.contains("Scroll_Lock") {
            return true;
        }
    }
    false
}

fn cmd_doctor() -> i32 {
    let mut need_fix = false;
    println!("== scrollock-rgb check ==");
    if xkb_fixed() {
        println!("[OK] ScrollLock key toggles RGB ({XKB_PC} has the Mod3 setup)");
    } else {
        println!("[FAIL] ScrollLock key is not set up to toggle RGB.");
        println!("       Run `scrollock-rgb fix`, then reboot, then press ScrollLock once.");
        need_fix = true;
        if let Err(e) = xkb_pc_path() {
            println!("       (path check: {e})");
        }
    }
    match discover() {
        Ok(kbs) if kbs.is_empty() => {
            println!("[FAIL] no RGB keyboard found");
            need_fix = true;
        }
        Ok(kbs) => {
            for k in &kbs {
                let st = if k.scroll.brightness > 0 { "on " } else { "off" };
                println!(
                    "[{}] {} ({}) Caps/Num lights={}/{} (want off)",
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
            need_fix = true;
        }
    }
    if need_fix { 1 } else { 0 }
}

fn cmd_fix_xkb(no_elevate: bool) -> i32 {
    let path = match xkb_pc_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("setup: {e}");
            return 1;
        }
    };
    let meta = match fs::metadata(&path) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("setup: cannot read keyboard layout file: {e}");
            return 1;
        }
    };
    if meta.len() > 1_000_000 {
        eprintln!("setup: refusing oversized layout file ({} bytes)", meta.len());
        return 1;
    }
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("setup: cannot read layout file: {e}");
            return 1;
        }
    };
    if text.contains(XKB_LINE) && xkb_fixed() {
        println!("already set up. If RGB still misbehaves you may need to reboot");
        println!("first (the desktop reads the layout at login), then press ScrollLock once.");
        return 0;
    }
    let lines: Vec<String> = text.lines().take(500).map(|s| s.to_string()).collect();
    if text.lines().count() > 500 {
        eprintln!("setup: refusing unexpected layout file (>500 lines)");
        return 1;
    }
    let mut idx: Option<usize> = None;
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim();
        if t.starts_with("//") || t.starts_with('#') {
            continue;
        }
        if l.contains("<SCLK>") && l.contains("Scroll_Lock") {
            idx = Some(i);
            break;
        }
    }
    let i = match idx {
        Some(i) => i,
        None => {
            eprintln!("setup: ScrollLock key entry not found, refusing to patch blindly.");
            return 1;
        }
    };
    let mut patched = lines;
    patched.insert(i + 1, XKB_LINE.to_string());
    let out = patched.join("\n") + "\n";
    if out.len() > 1_000_000 {
        eprintln!("setup: patched output too large, aborting.");
        return 1;
    }
    let bak = Path::new("/usr/share/X11/xkb/symbols/pc.bak-scrollock-rgb");
    let write_it = || -> Result<(), String> {
        if !bak.exists() {
            let orig = fs::read(&path).map_err(|e| format!("backup read: {e}"))?;
            fs::write(bak, orig).map_err(|e| format!("backup write {}: {e}", bak.display()))?;
        }
        fs::write(&path, out.as_bytes()).map_err(|e| format!("write {}: {e}", path.display()))
    };
    match write_it() {
        Ok(()) => {}
        Err(e) if !no_elevate && needs_elevation_error(&e) => elevate_self(),
        Err(e) => {
            eprintln!("setup: {e}");
            return 1;
        }
    }
    println!("done. Reboot (or log out/in), then press ScrollLock once to light RGB.");
    0
}

// ---- user commands ----

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
                let st = if k.scroll.brightness > 0 { "on " } else { "off" };
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
    if !xkb_fixed() && want.map(|v| v > 0).unwrap_or(true) {
        eprintln!("note: ScrollLock-key setup is missing — run `scrollock-rgb fix` + reboot");
        eprintln!("for the key itself to toggle RGB; turning RGB on directly for now…");
    }
    let mut rc = 0;
    for t in targets {
        let want_on = match want {
            Some(v) => v > 0,
            None => reread(&t.scroll).brightness == 0,
        };
        match apply_scroll(t, want_on) {
            Ok(_) => {
                save_intended(&t.input, want_on);
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

fn sysfs_writable(path: &Path) -> bool {
    match fs::OpenOptions::new().write(true).open(path) {
        Ok(_) => true,
        Err(e) => e.kind() != io::ErrorKind::PermissionDenied,
    }
}

/// One guard step for a single keyboard. Follows the ScrollLock toggle:
/// a lone ScrollLock change is respected; a change together with Caps/Num
/// activity is treated as a kill and repaired. Returns a log line when it
/// acted, or None when everything was already fine.
fn guard_step(
    kb: &Keyboard,
    intended_on: &mut bool,
    suppress_locks: bool,
) -> Result<Option<String>, String> {
    // Locks first: a lit Caps/Num LED is what latches RGB off on some
    // firmware, so clear those before touching ScrollLock.
    let mut lock_activity = false;
    if suppress_locks {
        for (label, slot) in [("caps", &kb.caps), ("num", &kb.num)] {
            let Some(led) = slot else { continue };
            let cur = reread(led);
            let trig_bad = cur.trigger_active != "none";
            let lit = cur.brightness != 0;
            if trig_bad {
                write_trigger_none(&cur)?;
            }
            if lit {
                write_brightness(&reread(led), 0)?;
            }
            if trig_bad || lit {
                lock_activity = true;
                let _ = label;
            }
        }
    }

    // ScrollLock trigger always detached so the kernel never auto-drives it.
    let s = reread(&kb.scroll);
    if s.trigger_active != "none" {
        write_trigger_none(&s)?;
    }

    let s2 = reread(&kb.scroll);
    if s2.brightness > 0 {
        if !*intended_on {
            *intended_on = true;
            save_intended(&kb.input, true);
            return Ok(Some(format!(
                "{}: RGB on (following your ScrollLock toggle)",
                kb.input
            )));
        }
        return Ok(None);
    }
    // ScrollLock LED is off.
    if lock_activity && *intended_on {
        write_brightness(&s2, 1)?;
        let verify = reread(&kb.scroll);
        if verify.brightness == 0 {
            return Err(format!("{}: RGB repair did not stick", kb.input));
        }
        return Ok(Some(format!(
            "{}: RGB back on (a Caps/Num press had turned it off)",
            kb.input
        )));
    }
    if !lock_activity && *intended_on {
        *intended_on = false;
        save_intended(&kb.input, false);
        return Ok(Some(format!(
            "{}: RGB off (following your ScrollLock toggle)",
            kb.input
        )));
    }
    Ok(None)
}

fn cmd_guard(
    device: Option<String>,
    all: bool,
    keep_lock_leds: bool,
    interval_ms: u64,
    iterations: u64,
    no_elevate: bool,
) -> i32 {
    let suppress_locks = !keep_lock_leds;
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
    if !no_elevate
        && pre_targets.iter().any(|t| {
            !sysfs_writable(&t.scroll.brightness_path) || !sysfs_writable(&t.scroll.trigger_path)
        })
    {
        elevate_self();
    }
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
    println!(
        "watching {} keyboard(s) every {} ms — ScrollLock toggles RGB, Caps/Num cannot kill it (Ctrl-C to stop)…",
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
        for t in targets {
            let intended = intents.entry(t.input.clone()).or_insert_with(|| {
                match load_intended(&t.input) {
                    Some(v) => v,
                    None => {
                        save_intended(&t.input, true);
                        true
                    }
                }
            });
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
        std::thread::sleep(interval);
    }
}

fn cmd_probe() -> i32 {
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
                    "  scroll  : {} brightness={}/{} trigger=[{}] (this LED is the RGB switch)",
                    k.scroll.base.display(),
                    k.scroll.brightness,
                    k.scroll.max_brightness,
                    k.scroll.trigger_active
                );
                for (label, slot) in [("caps", &k.caps), ("num", &k.num)] {
                    match slot {
                        Some(l) => println!(
                            "  {label:<7}: brightness={} trigger=[{}]",
                            l.brightness, l.trigger_active
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
            keep_lock_leds,
            interval_ms,
            iterations,
            ..
        }) => cmd_guard(device, all, keep_lock_leds, interval_ms, iterations, cli.no_elevate),
        Some(Cmd::Doctor) => cmd_doctor(),
        Some(Cmd::FixXkb) => cmd_fix_xkb(cli.no_elevate),
        Some(Cmd::Probe) => cmd_probe(),
    };
    process::exit(rc);
}
