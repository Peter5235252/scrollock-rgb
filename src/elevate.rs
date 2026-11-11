//! Elevation via polkit (`pkexec`) first, then the classic `sudo`/`doas`.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::{self, Command};

use crate::util::{args_without_first, find_in_path, is_root, polkit_install_hint};

pub fn needs_elevation_error(msg: &str) -> bool {
    msg.contains("Permission denied")
        || msg.contains("permission denied")
        || msg.contains("Operation not permitted")
        || msg.contains("operation not permitted")
}

/// Re-run this executable through pkexec/sudo/doas and exit with its status.
///
/// pkexec (polkit) is preferred because it works on every desktop, gives the
/// normal graphical authentication dialog, and does not depend on a sudoers
/// file. If pkexec is not installed we fall back to sudo, then doas.
pub fn elevate_self() -> ! {
    if is_root() {
        eprintln!("still cannot access the keyboard LEDs even as root.");
        eprintln!("They may be read-only in this kernel, or the device may not expose an RGB LED.");
        process::exit(1);
    }

    let exe = env::current_exe()
        .ok()
        .and_then(|p| fs::canonicalize(p).ok())
        .unwrap_or_else(|| PathBuf::from("/proc/self/exe"));

    let mut args = args_without_first();
    if !args.iter().any(|a| a.to_string_lossy() == "--no-elevate") {
        args.push(OsString::from("--no-elevate"));
    }

    let preferred = env::var("SCROLLOCK_RGB_ELEVATOR").unwrap_or_default();
    let mut order: Vec<&str> = Vec::new();
    if matches!(preferred.as_str(), "pkexec" | "sudo" | "doas") {
        order.push(match preferred.as_str() {
            "pkexec" => "pkexec",
            "sudo" => "sudo",
            _ => "doas",
        });
    }
    for name in ["pkexec", "sudo", "doas"] {
        if !order.contains(&name) {
            order.push(name);
        }
    }

    let mut last_problem = String::new();
    for (idx, name) in order.iter().enumerate() {
        let Some(path) = find_in_path(name) else {
            continue;
        };
        let mut cmd = Command::new(&path);
        if *name == "sudo" || *name == "doas" {
            cmd.arg("--");
        }
        cmd.arg(&exe);
        cmd.args(&args);
        eprintln!("need permission, re-running with {name}…");
        match cmd.status() {
            Ok(status) => {
                let code = status.code().unwrap_or(1);
                if code == 127 && idx + 1 < order.len() {
                    // pkexec exists but cannot execute (e.g. polkit not usable
                    // in this environment); try the next elevator.
                    last_problem = format!("{name} could not execute the command");
                    continue;
                }
                process::exit(code);
            }
            Err(e) => {
                last_problem = format!("failed to run {name}: {e}");
            }
        }
    }

    if !last_problem.is_empty() {
        eprintln!("{last_problem}");
    }
    eprintln!("need permission to touch keyboard LEDs, but no elevator worked.");
    eprintln!(
        "Install polkit so `pkexec` is available: {}",
        polkit_install_hint()
    );
    eprintln!("Alternatively run this command as root (the boot service already does).");
    process::exit(1);
}
