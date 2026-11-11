//! Small cross-distro helpers shared by the CLI, the guard and the XKB setup.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Resolve an executable the same way a shell would.
pub fn find_in_path(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let p = PathBuf::from(name);
        return is_executable(&p).then_some(p);
    }
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let p = dir.join(name);
        if is_executable(&p) {
            return Some(p);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111) != 0)
        .unwrap_or(false)
}

/// Run a command and return `(success, stdout, stderr)`.
pub fn run_output(program: &str, args: &[&str]) -> Option<(bool, String, String)> {
    let path = find_in_path(program)?;
    let out = Command::new(path).args(args).output().ok()?;
    Some((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// Run a command and return stdout only when it succeeded.
pub fn run_capture(program: &str, args: &[&str]) -> Option<String> {
    run_output(program, args)
        .filter(|(ok, _, _)| *ok)
        .map(|(_, out, _)| out)
}

pub fn is_root() -> bool {
    current_euid() == Some(0)
}

pub fn current_euid() -> Option<u32> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("Uid:"))?;
    let mut fields = line.split_whitespace();
    let _ = fields.next()?; // "Uid:"
    let _real = fields.next()?;
    fields.next()?.parse().ok()
}

/// `$HOME`, falling back to the passwd entry for the effective uid.
pub fn home_dir() -> Option<PathBuf> {
    if let Some(h) = env::var_os("HOME") {
        if !h.is_empty() {
            return Some(PathBuf::from(h));
        }
    }
    let uid = current_euid()?;
    passwd_by_uid(uid).map(|(_, home)| home)
}

/// `$XDG_CONFIG_HOME` when absolute, otherwise `~/.config`.
pub fn config_home() -> Option<PathBuf> {
    if let Some(x) = env::var_os("XDG_CONFIG_HOME") {
        let p = PathBuf::from(x);
        if p.is_absolute() {
            return Some(p);
        }
    }
    home_dir().map(|h| h.join(".config"))
}

pub fn passwd_by_uid(uid: u32) -> Option<(String, PathBuf)> {
    passwd_entries()
        .into_iter()
        .find(|(_, u, _)| *u == uid)
        .map(|(name, _, home)| (name, home))
}

fn passwd_entries() -> Vec<(String, u32, PathBuf)> {
    let text = fs::read_to_string("/etc/passwd").unwrap_or_default();
    text.lines()
        .filter_map(|line| {
            if line.starts_with('#') || line.trim().is_empty() {
                return None;
            }
            let mut fields = line.splitn(7, ':');
            let name = fields.next()?;
            let _passwd = fields.next()?;
            let uid = fields.next()?.parse().ok()?;
            let _gid = fields.next()?;
            let _gecos = fields.next()?;
            let home = fields.next()?;
            if name.is_empty() || home.is_empty() {
                return None;
            }
            Some((name.to_string(), uid, PathBuf::from(home)))
        })
        .collect()
}

pub fn args_without_first() -> Vec<OsString> {
    env::args_os().skip(1).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistroFamily {
    Arch,
    Debian,
    Fedora,
    Suse,
    Alpine,
    Void,
    Gentoo,
    NixOS,
    Other,
}

pub fn distro_family() -> DistroFamily {
    distro_family_from(&fs::read_to_string("/etc/os-release").unwrap_or_default())
}

pub fn distro_family_from(os_release: &str) -> DistroFamily {
    let mut id = String::new();
    let mut like = String::new();
    for line in os_release.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_ascii_lowercase();
        match key.trim() {
            "ID" => id = value,
            "ID_LIKE" => like = value,
            _ => {}
        }
    }
    let all = format!("{id} {like}");
    let has = |word: &str| all.split_whitespace().any(|w| w == word);
    if has("arch") || has("cachyos") || has("endeavouros") || has("manjaro") || has("garuda") {
        DistroFamily::Arch
    } else if has("debian") || has("ubuntu") || has("linuxmint") || has("pop") || has("zorin") {
        DistroFamily::Debian
    } else if has("fedora") || has("rhel") || has("centos") || has("rocky") || has("alma") {
        DistroFamily::Fedora
    } else if has("suse") || has("opensuse") || has("sles") {
        DistroFamily::Suse
    } else if has("alpine") {
        DistroFamily::Alpine
    } else if has("void") {
        DistroFamily::Void
    } else if has("gentoo") || has("funtoo") {
        DistroFamily::Gentoo
    } else if has("nixos") {
        DistroFamily::NixOS
    } else {
        DistroFamily::Other
    }
}

pub fn xkb_install_hint() -> &'static str {
    match distro_family() {
        DistroFamily::Arch => "sudo pacman -S xkeyboard-config",
        DistroFamily::Debian => "sudo apt install xkb-data",
        DistroFamily::Fedora => "sudo dnf install xkeyboard-config",
        DistroFamily::Suse => "sudo zypper install xkeyboard-config",
        DistroFamily::Alpine => "sudo apk add xkeyboard-config",
        DistroFamily::Void => "sudo xbps-install -S xkeyboard-config",
        DistroFamily::Gentoo => "sudo emerge x11-misc/xkeyboard-config",
        DistroFamily::NixOS => "add xkeyboard-config to environment.systemPackages",
        DistroFamily::Other => "install the xkeyboard-config package for your distro",
    }
}

pub fn polkit_install_hint() -> String {
    match distro_family() {
        DistroFamily::Arch => "sudo pacman -S polkit".into(),
        DistroFamily::Debian => "sudo apt install pkexec polkitd".into(),
        DistroFamily::Fedora => "sudo dnf install polkit".into(),
        DistroFamily::Suse => "sudo zypper install polkit".into(),
        DistroFamily::Alpine => "sudo apk add polkit".into(),
        DistroFamily::Void => "sudo xbps-install -S polkit".into(),
        DistroFamily::Gentoo => "sudo emerge sys-auth/polkit".into(),
        DistroFamily::NixOS => "add pkgs.polkit to environment.systemPackages".into(),
        DistroFamily::Other => "install polkit (provides pkexec)".into(),
    }
}

/// Quote a value for inclusion in a shell-free config file.
pub fn escape_quoted(value: &str, quote: char) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distro_detection() {
        assert_eq!(
            distro_family_from("ID=cachyos\nID_LIKE=arch\n"),
            DistroFamily::Arch
        );
        assert_eq!(
            distro_family_from("ID=ubuntu\nID_LIKE=debian\n"),
            DistroFamily::Debian
        );
        assert_eq!(distro_family_from("ID=fedora\n"), DistroFamily::Fedora);
        assert_eq!(
            distro_family_from("ID=\"opensuse-tumbleweed\"\nID_LIKE=\"opensuse suse\"\n"),
            DistroFamily::Suse
        );
        assert_eq!(distro_family_from("ID=nixos\n"), DistroFamily::NixOS);
        assert_eq!(distro_family_from("ID=alpine\n"), DistroFamily::Alpine);
        assert_eq!(distro_family_from("ID=void\n"), DistroFamily::Void);
        assert_eq!(distro_family_from("ID=gentoo\n"), DistroFamily::Gentoo);
        assert_eq!(distro_family_from("ID=weirdos\n"), DistroFamily::Other);
    }
}
