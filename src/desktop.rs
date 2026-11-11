//! Desktop/WM detection and the cross-distro `scrollock-rgb fix` implementation.
//!
//! The permanent fix is the standard XKB option `scrolllock:mod3`. It is
//! applied to the user's own compositor configuration, never by patching
//! `/usr/share/X11/xkb/symbols/pc`. This is what makes Caps/Num stop killing
//! RGB on every compositor, including Hyprland (which otherwise forces the
//! ScrollLock LED back to its own XKB state and makes it flicker).

use std::env;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::elevate::{elevate_self, needs_elevation_error};
use crate::util::{
    config_home, distro_family, find_in_path, home_dir, is_root, run_capture, run_output,
    xkb_install_hint, DistroFamily,
};
use crate::xkbconf::{
    edit_envd, edit_hypr_conf, edit_hypr_lua, edit_kxkbrc, edit_niri_kdl, edit_sway_conf,
    edit_xprofile, envd_options_from_text, format_gsettings_list, hypr_options_from_text,
    kde_options_from_text, niri_options_from_text, parse_gsettings_list, parse_hypr_getoption,
    parse_setxkbmap_options, sway_options_from_text, xkb_options_contain, xkb_options_merge,
    xprofile_options_from_text, XKB_OPTION,
};

const CONFIG_BACKUP_SUFFIX: &str = ".bak-scrollock-rgb";
const LEGACY_LINE: &str = "modifier_map Mod3 { Scroll_Lock };";
const MAX_CONFIG_BYTES: u64 = 2 * 1024 * 1024;

/* ---------------- session detection ---------------- */

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopKind {
    Hyprland,
    Sway,
    Kde,
    Gnome,
    Niri,
    X11,
    OtherWayland,
    Unknown,
}

impl DesktopKind {
    fn label(self) -> &'static str {
        match self {
            DesktopKind::Hyprland => "Hyprland",
            DesktopKind::Sway => "Sway",
            DesktopKind::Kde => "KDE Plasma",
            DesktopKind::Gnome => "GNOME",
            DesktopKind::Niri => "niri",
            DesktopKind::X11 => "X11",
            DesktopKind::OtherWayland => "Wayland compositor",
            DesktopKind::Unknown => "unknown session",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Session {
    pub kind: DesktopKind,
    pub wayland: bool,
    pub x11: bool,
    desktop: String,
}

impl Session {
    pub fn label(&self) -> String {
        let mut label = self.kind.label().to_string();
        if !matches!(
            self.kind,
            DesktopKind::X11 | DesktopKind::OtherWayland | DesktopKind::Unknown
        ) {
            if self.wayland {
                label.push_str(" (Wayland)");
            } else if self.x11 {
                label.push_str(" (X11)");
            }
        }
        if self.kind == DesktopKind::Unknown && !self.desktop.is_empty() {
            label.push_str(&format!(" [{}]", self.desktop));
        } else if self.kind == DesktopKind::Unknown {
            if self.wayland {
                label.push_str(" (Wayland)");
            } else if self.x11 {
                label.push_str(" (X11)");
            }
        }
        label
    }
}

fn env_nonempty(name: &str) -> Option<String> {
    env::var_os(name).and_then(|v| {
        let s = v.to_string_lossy().trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    })
}

pub fn detect_session() -> Session {
    detect_session_for(crate::util::current_euid())
}

pub fn detect_session_for(uid: Option<u32>) -> Session {
    let mut desktop = String::new();
    let mut kind: Option<DesktopKind> = None;

    if env_nonempty("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        kind = Some(DesktopKind::Hyprland);
    } else if env_nonempty("SWAYSOCK").is_some() {
        kind = Some(DesktopKind::Sway);
    } else if env_nonempty("NIRI_SOCKET").is_some() {
        kind = Some(DesktopKind::Niri);
    }

    if kind.is_none() {
        let names: Vec<String> = [
            "XDG_CURRENT_DESKTOP",
            "XDG_SESSION_DESKTOP",
            "DESKTOP_SESSION",
        ]
        .iter()
        .filter_map(|k| env_nonempty(k))
        .collect();
        desktop = names.join(":");
        let lower = desktop.to_ascii_lowercase();
        if lower.contains("hyprland") {
            kind = Some(DesktopKind::Hyprland);
        } else if lower.contains("sway") {
            kind = Some(DesktopKind::Sway);
        } else if lower.contains("kde") || lower.contains("plasma") {
            kind = Some(DesktopKind::Kde);
        } else if lower.contains("gnome") || lower.contains("ubuntu") {
            kind = Some(DesktopKind::Gnome);
        } else if lower.contains("niri") {
            kind = Some(DesktopKind::Niri);
        } else if lower.contains("xfce")
            || lower.contains("lxde")
            || lower.contains("lxqt")
            || lower.contains("mate")
            || lower.contains("cinnamon")
            || lower.contains("budgie")
            || lower.contains("pantheon")
            || lower.contains("unity")
            || lower.contains("i3")
            || lower.contains("bspwm")
            || lower.contains("openbox")
            || lower.contains("awesome")
            || lower.contains("dwm")
            || lower.contains("icewm")
            || lower.contains("fluxbox")
            || lower.contains("enlightenment")
            || lower.contains("qtile")
        {
            kind = Some(DesktopKind::X11);
        } else if lower.contains("river")
            || lower.contains("wayfire")
            || lower.contains("labwc")
            || lower.contains("cosmic")
        {
            kind = Some(DesktopKind::OtherWayland);
        }
    }

    if kind.is_none() {
        kind = detect_from_processes(uid);
    }

    let session_type = env_nonempty("XDG_SESSION_TYPE")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let session_says_wayland = session_type.contains("wayland");
    let session_says_x11 = session_type.contains("x11");
    let wayland = if session_says_wayland {
        true
    } else if session_says_x11 {
        false
    } else {
        env_nonempty("WAYLAND_DISPLAY").is_some()
            || matches!(
                kind,
                Some(DesktopKind::Hyprland | DesktopKind::Sway | DesktopKind::Niri)
            )
    };
    let x11 = if session_says_x11 {
        true
    } else if session_says_wayland {
        false
    } else {
        env_nonempty("DISPLAY").is_some() || kind == Some(DesktopKind::X11)
    };

    if kind.is_none() {
        kind = Some(if wayland {
            DesktopKind::OtherWayland
        } else if x11 {
            DesktopKind::X11
        } else {
            DesktopKind::Unknown
        });
    }

    Session {
        kind: kind.unwrap_or(DesktopKind::Unknown),
        wayland,
        x11,
        desktop,
    }
}

fn detect_from_processes(uid: Option<u32>) -> Option<DesktopKind> {
    let mut found = Vec::new();
    let entries = fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str() else {
            continue;
        };
        if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if let Some(uid) = uid {
            match fs::metadata(entry.path()) {
                Ok(meta) if meta.uid() == uid => {}
                _ => continue,
            }
        }
        let Ok(comm) = fs::read_to_string(entry.path().join("comm")) else {
            continue;
        };
        match comm.trim() {
            "Hyprland" => found.push(DesktopKind::Hyprland),
            "sway" => found.push(DesktopKind::Sway),
            "niri" => found.push(DesktopKind::Niri),
            "kwin_wayland" | "kwin_x11" | "plasmashell" => found.push(DesktopKind::Kde),
            "gnome-shell" => found.push(DesktopKind::Gnome),
            "Xorg" => found.push(DesktopKind::X11),
            _ => {}
        }
    }
    if found.contains(&DesktopKind::Hyprland) {
        Some(DesktopKind::Hyprland)
    } else if found.contains(&DesktopKind::Sway) {
        Some(DesktopKind::Sway)
    } else if found.contains(&DesktopKind::Niri) {
        Some(DesktopKind::Niri)
    } else if found.contains(&DesktopKind::Kde) {
        Some(DesktopKind::Kde)
    } else if found.contains(&DesktopKind::Gnome) {
        Some(DesktopKind::Gnome)
    } else if found.contains(&DesktopKind::X11) {
        Some(DesktopKind::X11)
    } else {
        None
    }
}

/* ---------------- config paths ---------------- */

fn hypr_config_file(config: &Path) -> PathBuf {
    let dir = config.join("hypr");
    let lua = dir.join("hyprland.lua");
    let conf = dir.join("hyprland.conf");
    if lua.is_file() {
        lua
    } else {
        conf
    }
}

fn sway_config_file(config: &Path) -> PathBuf {
    config.join("sway/config")
}

fn kxkbrc_file(config: &Path) -> PathBuf {
    config.join("kxkbrc")
}

fn niri_config_file(config: &Path) -> PathBuf {
    config.join("niri/config.kdl")
}

fn envd_file(config: &Path) -> PathBuf {
    config.join("environment.d/90-scrollock-rgb.conf")
}

fn xprofile_candidates(home: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = [".xprofile", ".xsessionrc"]
        .iter()
        .map(|name| home.join(name))
        .filter(|p| p.is_file())
        .collect();
    if files.is_empty() {
        let name = if distro_family() == DistroFamily::Debian {
            ".xsessionrc"
        } else {
            ".xprofile"
        };
        files.push(home.join(name));
    }
    files
}

/* ---------------- XKB data discovery ---------------- */

pub fn xkb_base_dirs(home: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(root) = env::var_os("XKB_CONFIG_ROOT") {
        candidates.push(PathBuf::from(root));
    }
    for p in [
        "/usr/share/X11/xkb",
        "/usr/share/xkeyboard-config-2",
        "/usr/local/share/X11/xkb",
        "/usr/share/xkb",
        "/run/current-system/sw/share/X11/xkb",
        "/nix/var/nix/profiles/default/share/X11/xkb",
    ] {
        candidates.push(PathBuf::from(p));
    }
    if let Some(home) = home {
        candidates.push(home.join(".nix-profile/share/X11/xkb"));
        candidates.push(home.join(".local/share/X11/xkb"));
    }
    let mut out = Vec::new();
    let mut seen = Vec::new();
    for candidate in candidates {
        if !candidate.is_dir() {
            continue;
        }
        let key = fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());
        if !seen.iter().any(|s| s == &key) {
            seen.push(key);
            out.push(candidate);
        }
    }
    out
}

pub fn xkb_option_supported(home: Option<&Path>) -> Result<PathBuf, String> {
    for base in xkb_base_dirs(home) {
        for rules in ["rules/evdev", "rules/base"] {
            if let Ok(text) = read_small(&base.join(rules)) {
                if text.contains(XKB_OPTION) {
                    return Ok(base);
                }
            }
        }
        if let Ok(text) = read_small(&base.join("symbols/scrolllock")) {
            if text.contains("mod3") && text.contains("Scroll_Lock") {
                return Ok(base);
            }
        }
    }
    if let Some(out) = run_capture(
        "xkbcli",
        &["compile-keymap", "--layout", "us", "--options", XKB_OPTION],
    ) {
        if out
            .lines()
            .any(|l| l.contains("modifier_map Mod3") && l.contains("<SCLK>"))
        {
            return Ok(PathBuf::from("xkbcli"));
        }
    }
    Err("xkeyboard-config does not provide the scrolllock:mod3 option".into())
}

pub fn legacy_pairs(home: Option<&Path>) -> Vec<(PathBuf, PathBuf)> {
    let mut out = Vec::new();
    for base in xkb_base_dirs(home) {
        let pc = base.join("symbols/pc");
        let backup = base.join(format!("symbols/pc{CONFIG_BACKUP_SUFFIX}"));
        if pc.is_file() || backup.is_file() {
            out.push((pc, backup));
        }
    }
    out
}

pub fn legacy_patch_present(home: Option<&Path>) -> bool {
    legacy_pairs(home).iter().any(|(pc, _)| {
        read_small(pc)
            .map(|text| text.lines().any(|l| l.trim() == LEGACY_LINE))
            .unwrap_or(false)
    })
}

fn read_small(path: &Path) -> Result<String, String> {
    let meta = fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.len() > MAX_CONFIG_BYTES {
        return Err(format!("{}: refusing oversized file", path.display()));
    }
    fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

/* ---------------- runtime / configured option queries ---------------- */

#[derive(Debug, Clone)]
pub struct XkbOptions {
    pub options: String,
    pub source: String,
}

pub fn runtime_xkb(session: &Session) -> Option<XkbOptions> {
    match session.kind {
        DesktopKind::Hyprland => {
            let out = run_capture("hyprctl", &["getoption", "input:kb_options"])?;
            Some(XkbOptions {
                options: parse_hypr_getoption(&out),
                source: "hyprctl".into(),
            })
        }
        DesktopKind::Sway => None,
        DesktopKind::Kde => {
            let out = run_capture("setxkbmap", &["-query"])?;
            Some(XkbOptions {
                options: parse_setxkbmap_options(&out),
                source: "setxkbmap (XWayland)".into(),
            })
        }
        DesktopKind::Gnome => {
            let out = run_capture(
                "gsettings",
                &["get", "org.gnome.desktop.input-sources", "xkb-options"],
            )?;
            Some(XkbOptions {
                options: parse_gsettings_list(&out).join(","),
                source: "gsettings".into(),
            })
        }
        DesktopKind::X11 | DesktopKind::Unknown if session.x11 => {
            let out = run_capture("setxkbmap", &["-query"])?;
            Some(XkbOptions {
                options: parse_setxkbmap_options(&out),
                source: "setxkbmap".into(),
            })
        }
        _ => None,
    }
}

pub fn configured_xkb(
    session: &Session,
    home: Option<&Path>,
    config: Option<&Path>,
) -> Option<XkbOptions> {
    let home = home?;
    let config = config?;
    match session.kind {
        DesktopKind::Hyprland => {
            let path = hypr_config_file(config);
            let text = read_small(&path).ok()?;
            Some(XkbOptions {
                options: hypr_options_from_text(&text),
                source: path.display().to_string(),
            })
        }
        DesktopKind::Sway => {
            let path = sway_config_file(config);
            let text = read_small(&path).ok()?;
            Some(XkbOptions {
                options: sway_options_from_text(&text),
                source: path.display().to_string(),
            })
        }
        DesktopKind::Kde => {
            let path = kxkbrc_file(config);
            let text = read_small(&path).ok()?;
            Some(XkbOptions {
                options: kde_options_from_text(&text),
                source: path.display().to_string(),
            })
        }
        DesktopKind::Gnome => {
            let out = run_capture(
                "gsettings",
                &["get", "org.gnome.desktop.input-sources", "xkb-options"],
            )?;
            Some(XkbOptions {
                options: parse_gsettings_list(&out).join(","),
                source: "gsettings".into(),
            })
        }
        DesktopKind::Niri => {
            let path = niri_config_file(config);
            let text = read_small(&path).ok()?;
            Some(XkbOptions {
                options: niri_options_from_text(&text),
                source: path.display().to_string(),
            })
        }
        DesktopKind::X11 | DesktopKind::OtherWayland | DesktopKind::Unknown => {
            for path in xprofile_candidates(home) {
                if let Ok(text) = read_small(&path) {
                    let options = xprofile_options_from_text(&text);
                    if !options.is_empty() {
                        return Some(XkbOptions {
                            options,
                            source: path.display().to_string(),
                        });
                    }
                }
            }
            let path = envd_file(config);
            let text = read_small(&path).ok()?;
            Some(XkbOptions {
                options: envd_options_from_text(&text),
                source: path.display().to_string(),
            })
        }
    }
}

pub fn xkb_is_configured(session: &Session, home: Option<&Path>, config: Option<&Path>) -> bool {
    if let Some(rt) = runtime_xkb(session) {
        if xkb_options_contain(&rt.options, XKB_OPTION) {
            return true;
        }
    }
    if let Some(cf) = configured_xkb(session, home, config) {
        if xkb_options_contain(&cf.options, XKB_OPTION) {
            return true;
        }
    }
    if env_nonempty("XKB_DEFAULT_OPTIONS")
        .map(|v| xkb_options_contain(&v, XKB_OPTION))
        .unwrap_or(false)
    {
        return true;
    }
    legacy_patch_present(home)
}

fn options_for_fix(session: &Session, home: &Path, config: &Path) -> (String, String) {
    if let Some(rt) = runtime_xkb(session) {
        if !rt.options.trim().is_empty() {
            return (rt.options, rt.source);
        }
    }
    if let Some(cf) = configured_xkb(session, Some(home), Some(config)) {
        if !cf.options.trim().is_empty() {
            return (cf.options, cf.source);
        }
    }
    (
        env_nonempty("XKB_DEFAULT_OPTIONS").unwrap_or_default(),
        "environment".into(),
    )
}

/* ---------------- safe user config writing ---------------- */

fn canonical_roots(home: &Path, config: &Path) -> Result<Vec<PathBuf>, String> {
    let mut roots = Vec::new();
    for p in [home, config] {
        fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
        let canon = fs::canonicalize(p).map_err(|e| format!("{}: {e}", p.display()))?;
        if !roots.iter().any(|r| r == &canon) {
            roots.push(canon);
        }
    }
    Ok(roots)
}

fn safe_user_path(path: &Path, roots: &[PathBuf]) -> Result<PathBuf, String> {
    let resolved = if path.exists() {
        fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))?
    } else {
        let parent = path
            .parent()
            .ok_or_else(|| format!("{}: no parent directory", path.display()))?;
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        let canon_parent =
            fs::canonicalize(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        let name = path
            .file_name()
            .ok_or_else(|| format!("{}: invalid file name", path.display()))?;
        canon_parent.join(name)
    };
    if !roots.iter().any(|root| resolved.starts_with(root)) {
        return Err(format!(
            "refusing to edit {}: it points outside your home/config directory",
            path.display()
        ));
    }
    Ok(resolved)
}

fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(CONFIG_BACKUP_SUFFIX);
    PathBuf::from(name)
}

fn read_config(path: &Path) -> Result<Option<String>, String> {
    match fs::metadata(path) {
        Ok(meta) if meta.len() > MAX_CONFIG_BYTES => Err(format!(
            "{}: refusing oversized config file",
            path.display()
        )),
        Ok(_) => fs::read_to_string(path)
            .map(Some)
            .map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Write a user config file atomically, keeping one backup.
fn write_config(path: &Path, text: &str, roots: &[PathBuf]) -> Result<Option<PathBuf>, String> {
    let resolved = safe_user_path(path, roots)?;
    let existing = read_config(&resolved)?;
    if existing.as_deref() == Some(text) {
        return Ok(None);
    }

    let backup = backup_path(&resolved);
    let mut created_backup = None;
    if existing.is_some() && !backup.exists() {
        fs::copy(&resolved, &backup).map_err(|e| format!("backup {}: {e}", backup.display()))?;
        created_backup = Some(backup.clone());
    }

    let parent = resolved
        .parent()
        .ok_or_else(|| format!("{}: no parent directory", resolved.display()))?;
    let mut tmp_name = resolved
        .file_name()
        .ok_or_else(|| format!("{}: invalid file name", resolved.display()))?
        .to_os_string();
    tmp_name.push(format!(".scrollock-rgb.tmp.{}", std::process::id()));
    let tmp = parent.join(tmp_name);
    let mode = fs::metadata(&resolved).ok().map(|m| m.permissions().mode());
    fs::write(&tmp, text.as_bytes()).map_err(|e| format!("{}: {e}", tmp.display()))?;
    if let Some(mode) = mode {
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(mode & 0o7777));
    }
    if let Err(e) = fs::rename(&tmp, &resolved) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("{}: {e}", resolved.display()));
    }
    Ok(created_backup)
}

/* ---------------- fix ---------------- */

pub fn fix_xkb(dry_run: bool, restore_legacy: bool, no_elevate: bool) -> i32 {
    if restore_legacy {
        return restore_legacy_patch(dry_run, no_elevate);
    }
    if is_root() {
        eprintln!("refusing to edit desktop configuration as root.");
        eprintln!("Run `scrollock-rgb fix` as your normal desktop user; it needs no root.");
        return 1;
    }

    let Some(home) = home_dir() else {
        eprintln!("fix: cannot determine your home directory");
        return 1;
    };
    let Some(config) = config_home() else {
        eprintln!("fix: cannot determine your XDG config directory");
        return 1;
    };
    let session = detect_session();

    println!("== scrollock-rgb fix ==");
    println!("session: {}", session.label());

    let base = match xkb_option_supported(Some(&home)) {
        Ok(base) => base,
        Err(e) => {
            eprintln!("[FAIL] {e}");
            eprintln!(
                "       Install/update xkeyboard-config: {}",
                xkb_install_hint()
            );
            if legacy_patch_present(Some(&home)) {
                eprintln!("       A legacy symbols/pc patch is present; it still works on X11,");
                eprintln!("       but the clean fix needs a current xkeyboard-config.");
            }
            return 1;
        }
    };
    println!("xkeyboard-config: {}", base.display());

    let (existing, source) = options_for_fix(&session, &home, &config);
    let full = xkb_options_merge(&existing, XKB_OPTION);
    if existing.is_empty() {
        println!("current XKB options: (none, from {source})");
    } else {
        println!("current XKB options: {existing} (from {source})");
    }
    println!("will use: {full}");

    let result = match session.kind {
        DesktopKind::Hyprland => fix_hyprland(&session, &home, &config, &full, dry_run),
        DesktopKind::Sway => fix_sway(&session, &full, &config, dry_run),
        DesktopKind::Kde => fix_kde(&session, &home, &config, &full, dry_run),
        DesktopKind::Gnome => fix_gnome(&full, dry_run),
        DesktopKind::Niri => fix_niri(&full, &config, dry_run),
        DesktopKind::X11 => fix_x11(&session, &home, &config, &full, dry_run),
        DesktopKind::OtherWayland => fix_other_wayland(&full, &config, dry_run),
        DesktopKind::Unknown => {
            if session.x11 {
                fix_x11(&session, &home, &config, &full, dry_run)
            } else if session.wayland {
                fix_other_wayland(&full, &config, dry_run)
            } else {
                eprintln!("no graphical session detected; configure your compositor manually:");
                print_manual_instructions();
                return 1;
            }
        }
    };

    if let Err(e) = result {
        eprintln!("[FAIL] {e}");
        return 1;
    }

    if legacy_patch_present(Some(&home)) {
        println!();
        println!("note: an old system-wide symbols/pc patch is still present.");
        println!("      It is obsolete. Undo it safely with:");
        println!("      scrollock-rgb fix --restore-legacy");
    }

    println!();
    println!("done. Press ScrollLock once to turn RGB on and lock the state.");
    println!("Caps/Num no longer turn RGB off.");
    0
}

fn fix_hyprland(
    session: &Session,
    home: &Path,
    config: &Path,
    full: &str,
    dry_run: bool,
) -> Result<(), String> {
    let path = hypr_config_file(config);
    let text = read_config(&path)?.unwrap_or_default();
    let is_lua = path.extension().and_then(|e| e.to_str()) == Some("lua");
    let (new_text, changed) = if is_lua {
        edit_hypr_lua(&text, XKB_OPTION, full)
    } else {
        edit_hypr_conf(&text, XKB_OPTION, full)
    };

    if dry_run {
        println!("would update: {}", path.display());
        if !changed {
            println!("  already configured; no file change needed");
        } else if is_lua {
            println!("  would set input.kb_options to: {full}");
        } else {
            println!("  would set kb_options to: {full}");
        }
        println!("  would apply live with: hyprctl keyword input:kb_options {full}");
        return Ok(());
    }

    let roots = canonical_roots(home, config)?;
    if changed {
        let backup = write_config(&path, &new_text, &roots)?;
        println!("updated: {}", path.display());
        if let Some(backup) = backup {
            println!("backup : {}", backup.display());
        }
    } else {
        println!("already configured: {}", path.display());
    }

    let live = runtime_xkb(session)
        .map(|rt| xkb_options_merge(&rt.options, XKB_OPTION))
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| full.to_string());
    let applied = match run_output("hyprctl", &["keyword", "input:kb_options", &live]) {
        Some((true, _, _)) => {
            println!("applied live with hyprctl");
            true
        }
        Some((false, _, err)) => {
            eprintln!("hyprctl could not apply the option live: {}", err.trim());
            false
        }
        None => {
            eprintln!("hyprctl not found; the config file will apply at next login");
            false
        }
    };
    if applied {
        if let Some(rt) = runtime_xkb(session) {
            if !xkb_options_contain(&rt.options, XKB_OPTION) {
                let _ = run_output("hyprctl", &["reload"]);
                if let Some(rt) = runtime_xkb(session) {
                    if !xkb_options_contain(&rt.options, XKB_OPTION) {
                        eprintln!(
                            "warning: Hyprland still reports kb_options='{}'; log out/in",
                            rt.options
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

fn fix_sway(session: &Session, full: &str, config: &Path, dry_run: bool) -> Result<(), String> {
    let path = sway_config_file(config);
    let text = read_config(&path)?.unwrap_or_default();
    let (new_text, changed) = edit_sway_conf(&text, XKB_OPTION, full);

    if dry_run {
        println!("would update: {}", path.display());
        if changed {
            println!("  would set xkb_options to: {full}");
        } else {
            println!("  already configured; no file change needed");
        }
        println!("  would apply live with: swaymsg input type:keyboard xkb_options {full}");
        return Ok(());
    }

    let roots = canonical_roots(
        &home_dir().ok_or_else(|| "cannot determine home directory".to_string())?,
        config,
    )?;
    if changed {
        let backup = write_config(&path, &new_text, &roots)?;
        println!("updated: {}", path.display());
        if let Some(backup) = backup {
            println!("backup : {}", backup.display());
        }
    } else {
        println!("already configured: {}", path.display());
    }

    let live = runtime_xkb(session)
        .map(|rt| xkb_options_merge(&rt.options, XKB_OPTION))
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| full.to_string());
    match run_output("swaymsg", &["input", "type:keyboard", "xkb_options", &live]) {
        Some((true, _, _)) => println!("applied live with swaymsg"),
        Some((false, _, err)) => {
            eprintln!("swaymsg could not apply the option live: {}", err.trim());
            eprintln!("the config file will apply at next login");
        }
        None => eprintln!("swaymsg not found; the config file will apply at next login"),
    }
    Ok(())
}

fn fix_kde(
    session: &Session,
    home: &Path,
    config: &Path,
    full: &str,
    dry_run: bool,
) -> Result<(), String> {
    let path = kxkbrc_file(config);
    let existing = read_config(&path)?;
    if let Some(text) = existing.as_deref() {
        if text
            .lines()
            .any(|l| l.trim().eq_ignore_ascii_case("Use=false"))
        {
            eprintln!("warning: KDE custom keyboard layouts are disabled (Use=false in kxkbrc).");
            eprintln!("         Enable System Settings → Keyboard → Layouts, or the option will not load.");
        }
    }
    let (new_text, changed) = match existing.as_deref() {
        Some(text) => edit_kxkbrc(text, XKB_OPTION, full),
        None => (kde_bootstrap_text(full)?, true),
    };

    if dry_run {
        println!("would update: {}", path.display());
        if changed {
            println!("  would set [Layout] Options={full}");
            if existing.is_none() {
                println!("  would create the file with the current layout/model");
            }
        } else {
            println!("  already configured; no file change needed");
        }
        if session.x11 {
            println!("  would apply live with: setxkbmap -option {full}");
        } else {
            println!("  KDE reads kxkbrc at login; log out/in after applying");
        }
        return Ok(());
    }

    let roots = canonical_roots(home, config)?;
    if changed {
        let backup = write_config(&path, &new_text, &roots)?;
        println!("updated: {}", path.display());
        if let Some(backup) = backup {
            println!("backup : {}", backup.display());
        }
    } else {
        println!("already configured: {}", path.display());
    }

    if session.x11 {
        match run_output("setxkbmap", &["-option", full]) {
            Some((true, _, _)) => println!("applied live with setxkbmap"),
            _ => eprintln!("setxkbmap could not apply the option live; log out/in"),
        }
    } else {
        println!("KDE reads keyboard options at login. Log out and back in to activate.");
    }
    Ok(())
}

fn fix_gnome(_full: &str, dry_run: bool) -> Result<(), String> {
    let current = run_capture(
        "gsettings",
        &["get", "org.gnome.desktop.input-sources", "xkb-options"],
    )
    .unwrap_or_default();
    let existing = parse_gsettings_list(&current).join(",");
    let merged = xkb_options_merge(&existing, XKB_OPTION);
    let value = format_gsettings_list(&crate::xkbconf::xkb_options_split(&merged));

    if dry_run {
        println!("would set GNOME xkb-options to: {value}");
        return Ok(());
    }

    if find_in_path("gsettings").is_none() {
        return Err(
            "gsettings not found; set the XKB option scrolllock:mod3 in GNOME Settings".into(),
        );
    }
    match run_output(
        "gsettings",
        &[
            "set",
            "org.gnome.desktop.input-sources",
            "xkb-options",
            &value,
        ],
    ) {
        Some((true, _, _)) => {
            println!("updated GNOME xkb-options (applies immediately): {value}");
            Ok(())
        }
        Some((false, _, err)) => Err(format!(
            "gsettings failed: {} (try: gsettings set org.gnome.desktop.input-sources xkb-options \"{value}\")",
            err.trim()
        )),
        None => Err("could not run gsettings".into()),
    }
}

fn fix_niri(full: &str, config: &Path, dry_run: bool) -> Result<(), String> {
    let path = niri_config_file(config);
    let text = read_config(&path)?.unwrap_or_default();
    let (new_text, changed) = edit_niri_kdl(&text, XKB_OPTION, full);
    if dry_run {
        println!("would update: {}", path.display());
        if changed {
            println!("  would set xkb options to: {full}");
        } else {
            println!("  already configured; no file change needed");
        }
        return Ok(());
    }
    let home = home_dir().ok_or_else(|| "cannot determine home directory".to_string())?;
    let roots = canonical_roots(&home, config)?;
    if changed {
        let backup = write_config(&path, &new_text, &roots)?;
        println!("updated: {}", path.display());
        if let Some(backup) = backup {
            println!("backup : {}", backup.display());
        }
    } else {
        println!("already configured: {}", path.display());
    }
    println!("niri reloads its config automatically; if not, restart niri.");
    Ok(())
}

fn fix_x11(
    session: &Session,
    home: &Path,
    config: &Path,
    full: &str,
    dry_run: bool,
) -> Result<(), String> {
    let files = xprofile_candidates(home);
    if dry_run {
        for path in &files {
            println!("would update: {}", path.display());
        }
        println!("  would set setxkbmap -option \"{full}\"");
        println!("  would apply live with: setxkbmap -option {full}");
        return Ok(());
    }
    let roots = canonical_roots(home, config)?;
    for path in &files {
        let text = read_config(path)?.unwrap_or_default();
        let (new_text, changed) = edit_xprofile(&text, XKB_OPTION, full);
        if changed {
            let backup = write_config(path, &new_text, &roots)?;
            println!("updated: {}", path.display());
            if let Some(backup) = backup {
                println!("backup : {}", backup.display());
            }
        } else {
            println!("already configured: {}", path.display());
        }
    }
    match run_output("setxkbmap", &["-option", full]) {
        Some((true, _, _)) => println!("applied live with setxkbmap"),
        Some((false, _, err)) => {
            eprintln!("setxkbmap could not apply the option live: {}", err.trim());
            eprintln!("log out/in to apply it from {}", files[0].display());
        }
        None => {
            if session.x11 {
                eprintln!("setxkbmap not found; install x11-xkb-utils / xorg-xsetxkbmap");
            }
        }
    }
    Ok(())
}

fn fix_other_wayland(_full: &str, config: &Path, dry_run: bool) -> Result<(), String> {
    let path = envd_file(config);
    let text = read_config(&path)?.unwrap_or_default();
    let env_existing = env_nonempty("XKB_DEFAULT_OPTIONS").unwrap_or_default();
    let file_existing = envd_options_from_text(&text);
    let base = xkb_options_merge(&env_existing, &file_existing);
    let merged = xkb_options_merge(&base, XKB_OPTION);
    let (new_text, changed) = edit_envd(&text, XKB_OPTION, &merged);

    if dry_run {
        println!("would update: {}", path.display());
        if changed {
            println!("  would set XKB_DEFAULT_OPTIONS={merged}");
        } else {
            println!("  already configured; no file change needed");
        }
        println!("  applies to wlroots-based compositors at next login");
        return Ok(());
    }
    let home = home_dir().ok_or_else(|| "cannot determine home directory".to_string())?;
    let roots = canonical_roots(&home, config)?;
    if changed {
        let backup = write_config(&path, &new_text, &roots)?;
        println!("updated: {}", path.display());
        if let Some(backup) = backup {
            println!("backup : {}", backup.display());
        }
    } else {
        println!("already configured: {}", path.display());
    }
    println!("applies at next login (systemd user environment / wlroots XKB_DEFAULT_OPTIONS).");
    Ok(())
}

fn kde_bootstrap_text(full: &str) -> Result<String, String> {
    let Some(query) = run_capture("setxkbmap", &["-query"]) else {
        return Err(
            "~/.config/kxkbrc does not exist and the current KDE layout could not be queried. \
Set System Settings → Keyboard → Layouts → Configure → XKB options to scrolllock:mod3."
                .into(),
        );
    };
    let layout = query_field(&query, "layout");
    let model = query_field(&query, "model");
    let variant = query_field(&query, "variant");
    if layout.is_empty() {
        return Err(
            "~/.config/kxkbrc does not exist and no current layout was found. \
Set System Settings → Keyboard → Layouts → Configure → XKB options to scrolllock:mod3."
                .into(),
        );
    }
    let mut text = String::from("[Layout]\nUse=true\n");
    text.push_str(&format!("LayoutList={layout}\n"));
    if !model.is_empty() {
        text.push_str(&format!("Model={model}\n"));
    }
    if !variant.is_empty() {
        text.push_str(&format!("VariantList={variant}\n"));
    }
    text.push_str(&format!("Options={full}\n"));
    Ok(text)
}

fn query_field(output: &str, key: &str) -> String {
    for line in output.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim() == key {
                return v.trim().to_string();
            }
        }
    }
    String::new()
}

fn print_manual_instructions() {
    println!("  Hyprland : input:kb_options = scrolllock:mod3");
    println!("  Sway     : input \"type:keyboard\" xkb_options scrolllock:mod3");
    println!("  KDE      : System Settings → Keyboard → Layouts → Configure → XKB options");
    println!("  GNOME    : gsettings set org.gnome.desktop.input-sources xkb-options \"['scrolllock:mod3']\"");
    println!("  niri     : input {{ keyboard {{ xkb {{ options \"scrolllock:mod3\" }} }} }}");
    println!("  X11      : setxkbmap -option scrolllock:mod3");
}

/* ---------------- legacy system patch ---------------- */

fn remove_legacy_line(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut removed = false;
    for line in text.lines() {
        if !removed && line.trim() == LEGACY_LINE {
            removed = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    removed.then_some(out)
}

fn restore_legacy_patch(dry_run: bool, no_elevate: bool) -> i32 {
    let home = home_dir();
    let pairs = legacy_pairs(home.as_deref());
    let mut any_patch = false;
    for (pc, backup) in &pairs {
        if backup.is_file() {
            if let Ok(current) = read_small(pc) {
                if current.lines().any(|l| l.trim() == LEGACY_LINE) {
                    any_patch = true;
                    break;
                }
            }
        }
    }
    if !any_patch {
        println!("no legacy symbols/pc patch found.");
        return 0;
    }

    let mut chosen: Option<(PathBuf, PathBuf, String, String)> = None;
    for (pc, backup) in pairs {
        let Ok(current) = read_small(&pc) else {
            continue;
        };
        let Ok(saved) = read_small(&backup) else {
            continue;
        };
        let Some(without) = remove_legacy_line(&current) else {
            continue;
        };
        if without == saved {
            chosen = Some((pc, backup, current, saved));
            break;
        }
    }

    let Some((pc, backup, current, saved)) = chosen else {
        eprintln!("no safe legacy patch found (the file was changed after the old fix).");
        eprintln!("Restore /usr/share/X11/xkb/symbols/pc from your package manager instead.");
        return 1;
    };

    if dry_run {
        println!(
            "would restore: {} (from {})",
            pc.display(),
            backup.display()
        );
        println!(
            "current size: {} bytes, original size: {} bytes",
            current.len(),
            saved.len()
        );
        return 0;
    }

    match fs::write(&pc, saved.as_bytes()) {
        Ok(()) => {
            let _ = fs::remove_file(&backup);
            println!("restored: {}", pc.display());
            println!("removed backup: {}", backup.display());
            0
        }
        Err(e) if !no_elevate && needs_elevation_error(&e.to_string()) => elevate_self(),
        Err(e) => {
            eprintln!("restore {}: {e}", pc.display());
            1
        }
    }
}

/* ---------------- tests ---------------- */

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_line_removal_round_trip() {
        let original = "key <SCLK> { [ Scroll_Lock ] };\nkey <PAUS> { [ Pause ] };\n";
        let patched = format!(
            "key <SCLK> {{ [ Scroll_Lock ] }};\n    {LEGACY_LINE}\nkey <PAUS> {{ [ Pause ] }};\n"
        );
        assert_eq!(remove_legacy_line(&patched).as_deref(), Some(original));
        assert!(remove_legacy_line(original).is_none());
    }

    #[test]
    fn query_fields() {
        let out = "rules: evdev\nmodel: pc105\nlayout: us,hu\nvariant: ,\n";
        assert_eq!(query_field(out, "layout"), "us,hu");
        assert_eq!(query_field(out, "model"), "pc105");
        assert_eq!(query_field(out, "missing"), "");
    }

    #[test]
    fn xkb_paths_do_not_panic() {
        let _ = xkb_base_dirs(None);
        let _ = legacy_pairs(None);
    }
}
