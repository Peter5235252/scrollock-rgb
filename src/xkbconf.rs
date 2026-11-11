//! Pure, testable XKB option and desktop-config parsing/editing.
//!
//! Every editor takes the old text and returns `(new_text, changed)`. They are
//! deliberately line-oriented: desktop configs are user files, not a format we
//! want to rewrite wholesale.

use crate::util::escape_quoted;

pub const XKB_OPTION: &str = "scrolllock:mod3";
pub const XPROFILE_BEGIN: &str = "# >>> scrollock-rgb >>>";
pub const XPROFILE_END: &str = "# <<< scrollock-rgb <<<";

/* ---------------- generic helpers ---------------- */

pub fn is_active_line(line: &str) -> bool {
    let t = line.trim_start();
    !t.is_empty()
        && !t.starts_with('#')
        && !t.starts_with("//")
        && !t.starts_with(';')
        && !t.starts_with("--")
}

fn leading_ws(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// Split at the first comment marker, returning `(code, comment_with_marker)`.
fn split_comment<'a>(line: &'a str, markers: &[&str]) -> (&'a str, &'a str) {
    let mut best: Option<usize> = None;
    for marker in markers {
        if let Some(pos) = line.find(marker) {
            if best.map(|b| pos < b).unwrap_or(true) {
                best = Some(pos);
            }
        }
    }
    match best {
        Some(pos) => (&line[..pos], &line[pos..]),
        None => (line, ""),
    }
}

fn find_token(haystack: &str, token: &str) -> Option<usize> {
    let bytes = haystack.as_bytes();
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(token) {
        let i = start + pos;
        let before_ok = i == 0 || {
            let b = bytes[i - 1];
            !b.is_ascii_alphanumeric() && b != b'_'
        };
        let after = i + token.len();
        let after_ok = after >= bytes.len() || {
            let b = bytes[after];
            !b.is_ascii_alphanumeric() && b != b'_'
        };
        if before_ok && after_ok {
            return Some(i);
        }
        start = i + token.len();
    }
    None
}

/// Find the `=` of an assignment whose key is `key`.
/// Handles `key =`, `input:key =` and `["input.key"] =` without a parser.
fn assignment_eq(line: &str, key: &str) -> Option<usize> {
    let code = split_comment(line, &["#", "//"]).0;
    let eq = code.find('=')?;
    let key_part = code[..eq].trim();
    let token = key_part.rsplit([':', '.']).next().unwrap_or(key_part);
    let token = token
        .trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == '[' || c == ']')
        .trim();
    (token == key).then_some(eq)
}

/// Like `assignment_eq`, but finds `key` anywhere on the line first. Needed
/// for Lua lines such as `hl.config({ input = { kb_options = "..." } })`.
fn key_value_eq(line: &str, key: &str) -> Option<usize> {
    let code = split_comment(line, &["#", "--"]).0;
    let idx = find_token(code, key)?;
    let rel = code.get(idx + key.len()..)?.find('=')?;
    Some(idx + key.len() + rel)
}

fn unquote(value: &str) -> (&str, Option<char>) {
    let t = value.trim();
    if t.len() >= 2 {
        let first = t.chars().next().unwrap_or(' ');
        let last = t.chars().next_back().unwrap_or(' ');
        if (first == '"' || first == '\'') && last == first {
            return (&t[1..t.len() - 1], Some(first));
        }
    }
    (t, None)
}

/// `(start, end, quote)` where `end` is the index of the closing quote.
fn find_quoted(text: &str) -> Option<(usize, usize, char)> {
    let mut chars = text.char_indices();
    let (start, quote) = loop {
        let (i, ch) = chars.next()?;
        if ch == '"' || ch == '\'' {
            break (i, ch);
        }
    };
    let mut escaped = false;
    for (i, ch) in chars {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == quote {
            return Some((start, i, quote));
        }
    }
    None
}

/// Merge `add` into the first quoted value on the line (Niri's `options "..."`).
fn merge_first_quoted_value(line: &str, add: &str, full: &str) -> Option<String> {
    let (start, end, quote) = find_quoted(line)?;
    let existing = &line[start + 1..end];
    let base = if existing.trim().is_empty() {
        full
    } else {
        existing
    };
    let merged = xkb_options_merge(base, add);
    if merged == existing {
        return Some(line.to_string());
    }
    let mut out = String::with_capacity(line.len() + merged.len());
    out.push_str(&line[..start + 1]);
    out.push_str(&escape_quoted(&merged, quote));
    out.push_str(&line[end..]);
    Some(out)
}

/// Merge `add` into the quoted value that follows the first `=`.
fn merge_assignment_quoted(line: &str, add: &str, full: &str) -> Option<String> {
    let eq = line.find('=')?;
    let after = &line[eq + 1..];
    let (rel_start, rel_end, quote) = find_quoted(after)?;
    let start = eq + 1 + rel_start;
    let end = eq + 1 + rel_end;
    let existing = &line[start + 1..end];
    let base = if existing.trim().is_empty() {
        full
    } else {
        existing
    };
    let merged = xkb_options_merge(base, add);
    if merged == existing {
        return Some(line.to_string());
    }
    let mut out = String::with_capacity(line.len() + merged.len());
    out.push_str(&line[..start + 1]);
    out.push_str(&escape_quoted(&merged, quote));
    out.push_str(&line[end..]);
    Some(out)
}

/* ---------------- XKB option list ---------------- */

pub fn xkb_options_split(options: &str) -> Vec<String> {
    options
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn xkb_options_contain(options: &str, wanted: &str) -> bool {
    xkb_options_split(options).iter().any(|o| o == wanted)
}

/// Append `add` if missing, preserving order and removing duplicates.
pub fn xkb_options_merge(existing: &str, add: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for opt in xkb_options_split(existing) {
        if !out.iter().any(|o| o == &opt) {
            out.push(opt);
        }
    }
    for opt in xkb_options_split(add) {
        if !out.iter().any(|o| o == &opt) {
            out.push(opt);
        }
    }
    out.join(",")
}

/* ---------------- desktop config editors ---------------- */

/// Hyprland hyprlang config: `kb_options = ...`, `input:kb_options = ...`
/// or a `kb_options` line inside an `input {}` block.
pub fn edit_hypr_conf(text: &str, add: &str, full: &str) -> (String, bool) {
    let mut out = String::with_capacity(text.len() + 160);
    let mut changed = false;
    let mut found = false;

    for line in text.lines() {
        if is_active_line(line) {
            if let Some(eq) = key_value_eq(line, "kb_options") {
                let (code, comment) = split_comment(line, &["#"]);
                let after = code.get(eq + 1..).unwrap_or("");
                let ws = leading_ws(after);
                let raw = after.trim();
                let (existing, quote) = unquote(raw);
                let base = if existing.trim().is_empty() {
                    full
                } else {
                    existing
                };
                let merged = xkb_options_merge(base, add);
                found = true;
                if merged == existing {
                    out.push_str(line);
                } else {
                    let value = match quote {
                        Some(q) => format!("{q}{}{q}", escape_quoted(&merged, q)),
                        None => merged,
                    };
                    let mut new_line = format!("{}{}{}", &code[..=eq], ws, value);
                    if !comment.is_empty() {
                        new_line.push(' ');
                        new_line.push_str(comment);
                    }
                    changed = true;
                    out.push_str(&new_line);
                }
                out.push('\n');
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }

    if !found {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("\n# scrollock-rgb: make ScrollLock a real Mod3 modifier\n");
        out.push_str("input {\n");
        out.push_str(&format!("    kb_options = {full}\n"));
        out.push_str("}\n");
        changed = true;
    }
    (out, changed)
}

/// Hyprland's newer Lua config (`hl.config({ input = { kb_options = "..." } })`).
pub fn edit_hypr_lua(text: &str, add: &str, full: &str) -> (String, bool) {
    let mut out = String::with_capacity(text.len() + 160);
    let mut changed = false;
    let mut found = false;

    for line in text.lines() {
        if is_active_line(line)
            && line.contains("kb_options")
            && key_value_eq(line, "kb_options").is_some()
        {
            let (code, comment) = split_comment(line, &["--"]);
            if let Some(merged_line) = merge_assignment_quoted(code, add, full) {
                found = true;
                if merged_line == code {
                    out.push_str(line);
                } else {
                    changed = true;
                    out.push_str(&merged_line);
                    if !comment.is_empty() {
                        out.push(' ');
                        out.push_str(comment);
                    }
                }
                out.push('\n');
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }

    if !found {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("\n-- scrollock-rgb: make ScrollLock a real Mod3 modifier\n");
        out.push_str(&format!(
            "hl.config({{ input = {{ kb_options = \"{}\" }} }})\n",
            escape_quoted(full, '"')
        ));
        changed = true;
    }
    (out, changed)
}

/// Sway config: `xkb_options ...` rows, including inside `input {}` blocks.
pub fn edit_sway_conf(text: &str, add: &str, full: &str) -> (String, bool) {
    let mut out = String::with_capacity(text.len() + 160);
    let mut changed = false;
    let mut found = false;

    for line in text.lines() {
        if is_active_line(line) {
            let (code, comment) = split_comment(line, &["#"]);
            if let Some(idx) = find_token(code, "xkb_options") {
                let after = code.get(idx + "xkb_options".len()..).unwrap_or("");
                let ws = leading_ws(after);
                let raw = after.trim();
                let (existing, quote) = unquote(raw);
                let base = if existing.trim().is_empty() {
                    full
                } else {
                    existing
                };
                let merged = xkb_options_merge(base, add);
                found = true;
                if merged == existing {
                    out.push_str(line);
                } else {
                    let value = match quote {
                        Some(q) => format!("{q}{}{q}", escape_quoted(&merged, q)),
                        None => merged,
                    };
                    let mut new_line =
                        format!("{}{}{}", &code[..idx + "xkb_options".len()], ws, value);
                    if !comment.is_empty() {
                        new_line.push(' ');
                        new_line.push_str(comment);
                    }
                    changed = true;
                    out.push_str(&new_line);
                }
                out.push('\n');
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }

    if !found {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("\n# scrollock-rgb: make ScrollLock a real Mod3 modifier\n");
        out.push_str(&format!("input \"type:keyboard\" xkb_options {full}\n"));
        changed = true;
    }
    (out, changed)
}

/// KDE Plasma `~/.config/kxkbrc`.
pub fn edit_kxkbrc(text: &str, add: &str, full: &str) -> (String, bool) {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut section_start: Option<usize> = None;
    let mut section_end = lines.len();
    let mut in_layout = false;
    let mut option_idx: Option<usize> = None;

    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if t.starts_with('[') && t.ends_with(']') {
            if in_layout {
                section_end = i;
                in_layout = false;
            }
            if t.eq_ignore_ascii_case("[Layout]") {
                in_layout = true;
                section_start = Some(i);
            }
        } else if in_layout && is_active_line(line) {
            if let Some(eq) = line.find('=') {
                if line[..eq].trim().eq_ignore_ascii_case("Options") {
                    option_idx = Some(i);
                }
            }
        }
    }
    if in_layout {
        section_end = lines.len();
    }

    if let Some(i) = option_idx {
        let line = &lines[i];
        let eq = line.find('=').unwrap_or(0);
        let existing = line[eq + 1..].trim();
        let base = if existing.is_empty() { full } else { existing };
        let merged = xkb_options_merge(base, add);
        if merged == existing {
            return (text.to_string(), false);
        }
        lines[i] = format!("{}={}", &line[..eq], merged);
        return (lines.join("\n") + "\n", true);
    }

    let value = format!("Options={full}");
    if section_start.is_some() {
        lines.insert(section_end.min(lines.len()), value);
    } else {
        if !lines.is_empty() && !lines.last().map(|l| l.is_empty()).unwrap_or(false) {
            lines.push(String::new());
        }
        lines.push("[Layout]".into());
        lines.push(value);
    }
    (lines.join("\n") + "\n", true)
}

/// niri's KDL config (`input { keyboard { xkb { options "..." } } }`).
pub fn edit_niri_kdl(text: &str, add: &str, full: &str) -> (String, bool) {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();

    // Existing `xkb { ... }` block: edit or insert its options line.
    if let Some(open) = lines.iter().position(|line| {
        is_active_line(line) && {
            let t = line.trim();
            t == "xkb {" || t.starts_with("xkb {") || (t.starts_with("xkb ") && t.ends_with('{'))
        }
    }) {
        let indent = leading_ws(&lines[open]).to_string();
        let child = format!("{indent}    ");
        let mut depth = 0i32;
        let mut close = lines.len();
        for (i, line) in lines.iter().enumerate().skip(open) {
            let code = split_comment(line, &["//"]).0;
            depth += code.matches('{').count() as i32;
            depth -= code.matches('}').count() as i32;
            if depth <= 0 && i > open {
                close = i;
                break;
            }
        }

        let mut found = false;
        let mut changed = false;
        for line in lines.iter_mut().take(close).skip(open + 1) {
            let code = split_comment(line, &["//"]).0;
            if is_active_line(line) && find_token(code, "options").is_some() {
                if let Some(merged) = merge_first_quoted_value(line, add, full) {
                    found = true;
                    if merged != *line {
                        *line = merged;
                        changed = true;
                    }
                    break;
                }
            }
        }
        if !found {
            lines.insert(
                open + 1,
                format!("{child}options \"{}\"", escape_quoted(full, '"')),
            );
            changed = true;
        }
        return (lines.join("\n") + "\n", changed);
    }

    // Existing keyboard block, but no xkb block.
    if let Some(open) = lines.iter().position(|line| {
        is_active_line(line) && {
            let t = line.trim();
            t == "keyboard {" || t.starts_with("keyboard {")
        }
    }) {
        let indent = leading_ws(&lines[open]).to_string();
        let child = format!("{indent}    ");
        lines.insert(open + 1, format!("{child}xkb {{"));
        lines.insert(
            open + 2,
            format!("{child}    options \"{}\"", escape_quoted(full, '"')),
        );
        lines.insert(open + 3, format!("{child}}}"));
        return (lines.join("\n") + "\n", true);
    }

    // Existing input block, but no keyboard block.
    if let Some(open) = lines.iter().position(|line| {
        is_active_line(line) && {
            let t = line.trim();
            t == "input {" || t.starts_with("input {")
        }
    }) {
        let indent = leading_ws(&lines[open]).to_string();
        let child = format!("{indent}    ");
        lines.insert(open + 1, format!("{child}keyboard {{"));
        lines.insert(open + 2, format!("{child}    xkb {{"));
        lines.insert(
            open + 3,
            format!("{child}        options \"{}\"", escape_quoted(full, '"')),
        );
        lines.insert(open + 4, format!("{child}    }}"));
        lines.insert(open + 5, format!("{child}}}"));
        return (lines.join("\n") + "\n", true);
    }

    // Fresh config.
    if !lines.is_empty() && !lines.last().map(|l| l.is_empty()).unwrap_or(false) {
        lines.push(String::new());
    }
    lines.push("// scrollock-rgb: make ScrollLock a real Mod3 modifier".into());
    lines.push("input {".into());
    lines.push("    keyboard {".into());
    lines.push("        xkb {".into());
    lines.push(format!(
        "            options \"{}\"",
        escape_quoted(full, '"')
    ));
    lines.push("        }".into());
    lines.push("    }".into());
    lines.push("}".into());
    (lines.join("\n") + "\n", true)
}

/// `~/.config/environment.d/*.conf`: `XKB_DEFAULT_OPTIONS=...`.
pub fn edit_envd(text: &str, add: &str, full: &str) -> (String, bool) {
    let mut out = String::with_capacity(text.len() + 128);
    let mut changed = false;
    let mut found = false;

    for line in text.lines() {
        if is_active_line(line) {
            if let Some(eq) = assignment_eq(line, "XKB_DEFAULT_OPTIONS") {
                let (code, comment) = split_comment(line, &["#"]);
                let after = code.get(eq + 1..).unwrap_or("");
                let ws = leading_ws(after);
                let raw = after.trim();
                let (existing, quote) = unquote(raw);
                let base = if existing.trim().is_empty() {
                    full
                } else {
                    existing
                };
                let merged = xkb_options_merge(base, add);
                found = true;
                if merged == existing {
                    out.push_str(line);
                } else {
                    let value = match quote {
                        Some(q) => format!("{q}{}{q}", escape_quoted(&merged, q)),
                        None => merged,
                    };
                    let mut new_line = format!("{}{}{}", &code[..=eq], ws, value);
                    if !comment.is_empty() {
                        new_line.push(' ');
                        new_line.push_str(comment);
                    }
                    changed = true;
                    out.push_str(&new_line);
                }
                out.push('\n');
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }

    if !found {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("\n# scrollock-rgb: make ScrollLock a real Mod3 modifier\n");
        out.push_str(&format!("XKB_DEFAULT_OPTIONS={full}\n"));
        changed = true;
    }
    (out, changed)
}

/// `~/.xprofile` / `~/.xsessionrc`. A tagged block makes it safe to update.
pub fn edit_xprofile(text: &str, add: &str, full: &str) -> (String, bool) {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let begin = lines.iter().position(|l| l.trim() == XPROFILE_BEGIN);
    let end = lines.iter().position(|l| l.trim() == XPROFILE_END);

    if let (Some(begin), Some(end)) = (begin, end) {
        if begin < end {
            let block = lines[begin..=end].join("\n");
            let existing = xprofile_options_from_text(&block);
            let base = if existing.trim().is_empty() {
                full.to_string()
            } else {
                existing.clone()
            };
            let merged = xkb_options_merge(&base, add);
            let new_line = format!("setxkbmap -option \"{}\"", escape_quoted(&merged, '"'));
            let inner = &lines[begin + 1..end];
            if inner.len() == 1 && inner[0] == new_line && merged == existing {
                return (text.to_string(), false);
            }
            lines.splice(begin + 1..end, [new_line]);
            return (lines.join("\n") + "\n", true);
        }
    }

    if !lines.is_empty() && !lines.last().map(|l| l.is_empty()).unwrap_or(false) {
        lines.push(String::new());
    }
    lines.push(XPROFILE_BEGIN.into());
    lines.push(format!(
        "setxkbmap -option \"{}\"",
        escape_quoted(full, '"')
    ));
    lines.push(XPROFILE_END.into());
    (lines.join("\n") + "\n", true)
}

/* ---------------- config readers ---------------- */

pub fn hypr_options_from_text(text: &str) -> String {
    for line in text.lines() {
        if !is_active_line(line) || !line.contains("kb_options") {
            continue;
        }
        let Some(eq) = key_value_eq(line, "kb_options") else {
            continue;
        };
        let code = split_comment(line, &["#", "--"]).0;
        let after = code.get(eq + 1..).unwrap_or("");
        if let Some((start, end, _)) = find_quoted(after) {
            return after[start + 1..end].to_string();
        }
        let raw = after.trim();
        if !raw.is_empty() {
            return unquote(raw).0.to_string();
        }
    }
    String::new()
}

pub fn sway_options_from_text(text: &str) -> String {
    for line in text.lines() {
        if !is_active_line(line) {
            continue;
        }
        let code = split_comment(line, &["#"]).0;
        let Some(idx) = find_token(code, "xkb_options") else {
            continue;
        };
        let after = code.get(idx + "xkb_options".len()..).unwrap_or("").trim();
        if !after.is_empty() {
            return unquote(after).0.to_string();
        }
    }
    String::new()
}

pub fn kde_options_from_text(text: &str) -> String {
    let mut in_layout = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') && t.ends_with(']') {
            in_layout = t.eq_ignore_ascii_case("[Layout]");
            continue;
        }
        if in_layout && is_active_line(line) {
            if let Some(eq) = line.find('=') {
                if line[..eq].trim().eq_ignore_ascii_case("Options") {
                    return line[eq + 1..].trim().to_string();
                }
            }
        }
    }
    String::new()
}

pub fn niri_options_from_text(text: &str) -> String {
    for line in text.lines() {
        if !is_active_line(line) {
            continue;
        }
        if find_token(line, "options").is_some() {
            if let Some((start, end, _)) = find_quoted(line) {
                return line[start + 1..end].to_string();
            }
        }
    }
    String::new()
}

pub fn envd_options_from_text(text: &str) -> String {
    for line in text.lines() {
        if !is_active_line(line) {
            continue;
        }
        if assignment_eq(line, "XKB_DEFAULT_OPTIONS").is_some() {
            if let Some(eq) = line.find('=') {
                return unquote(line[eq + 1..].trim()).0.to_string();
            }
        }
    }
    String::new()
}

pub fn xprofile_options_from_text(text: &str) -> String {
    if let (Some(begin), Some(end)) = (
        text.lines().position(|l| l.trim() == XPROFILE_BEGIN),
        text.lines().position(|l| l.trim() == XPROFILE_END),
    ) {
        if begin < end {
            for line in text.lines().skip(begin + 1).take(end - begin - 1) {
                if let Some(opts) = extract_setxkbmap_options(line) {
                    return opts;
                }
            }
        }
    }
    text.lines()
        .filter(|l| is_active_line(l))
        .filter_map(extract_setxkbmap_options)
        .next_back()
        .unwrap_or_default()
}

pub fn extract_setxkbmap_options(line: &str) -> Option<String> {
    find_token(line, "setxkbmap")?;
    let idx = find_token(line, "-option").or_else(|| find_token(line, "-options"))?;
    let after = line.get(idx..)?;
    let after = after
        .split_once(char::is_whitespace)
        .map(|(_, r)| r)
        .unwrap_or("");
    let raw = after.trim();
    if raw.is_empty() {
        return None;
    }
    Some(unquote(raw).0.to_string())
}

/* ---------------- command output parsers ---------------- */

pub fn parse_hypr_getoption(output: &str) -> String {
    for line in output.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("str:") {
            return unquote(rest).0.to_string();
        }
    }
    String::new()
}

pub fn parse_setxkbmap_options(output: &str) -> String {
    for line in output.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("options:") {
            return rest.trim().to_string();
        }
    }
    String::new()
}

pub fn parse_gsettings_list(output: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    let mut escaped = false;
    for ch in output.chars() {
        if in_string {
            if escaped {
                current.push(ch);
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '\'' {
                out.push(current.clone());
                current.clear();
                in_string = false;
            } else {
                current.push(ch);
            }
        } else if ch == '\'' {
            in_string = true;
        }
    }
    out
}

pub fn format_gsettings_list(options: &[String]) -> String {
    let quoted: Vec<String> = options
        .iter()
        .map(|o| format!("'{}'", o.replace('\\', "\\\\").replace('\'', "\\'")))
        .collect();
    format!("[{}]", quoted.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_lists_merge_and_dedupe() {
        assert_eq!(xkb_options_merge("", XKB_OPTION), XKB_OPTION);
        assert_eq!(
            xkb_options_merge("caps:ctrl_modifier", XKB_OPTION),
            "caps:ctrl_modifier,scrolllock:mod3"
        );
        assert_eq!(
            xkb_options_merge("caps:ctrl_modifier,scrolllock:mod3", XKB_OPTION),
            "caps:ctrl_modifier,scrolllock:mod3"
        );
        assert!(xkb_options_contain("a,scrolllock:mod3,b", XKB_OPTION));
        assert!(!xkb_options_contain("a,b", XKB_OPTION));
    }

    #[test]
    fn hypr_conf_edits_and_is_idempotent() {
        let src = "input {\n    kb_options = caps:ctrl_modifier\n}\n";
        let (out, changed) = edit_hypr_conf(src, XKB_OPTION, XKB_OPTION);
        assert!(changed);
        assert!(out.contains("kb_options = caps:ctrl_modifier,scrolllock:mod3"));
        let (again, changed) = edit_hypr_conf(&out, XKB_OPTION, XKB_OPTION);
        assert!(!changed);
        assert_eq!(again, out);
    }

    #[test]
    fn hypr_conf_appends_block() {
        let (out, changed) = edit_hypr_conf("misc {\n}\n", XKB_OPTION, XKB_OPTION);
        assert!(changed);
        assert!(out.contains("input {\n    kb_options = scrolllock:mod3\n}"));
        let (_, changed) = edit_hypr_conf(&out, XKB_OPTION, XKB_OPTION);
        assert!(!changed);
    }

    #[test]
    fn hypr_lua_replaces_option() {
        let src = "hl.config({\n  input = {\n    kb_options = \"caps:ctrl_modifier\",\n  },\n})\n";
        let (out, changed) = edit_hypr_lua(src, XKB_OPTION, XKB_OPTION);
        assert!(changed);
        assert!(out.contains("kb_options = \"caps:ctrl_modifier,scrolllock:mod3\""));
        let (again, changed) = edit_hypr_lua(&out, XKB_OPTION, XKB_OPTION);
        assert!(!changed);
        assert_eq!(again, out);
    }

    #[test]
    fn hypr_lua_uses_full_value_when_empty() {
        let src = "hl.config({\n  input = {\n    kb_options = \"\",\n  },\n})\n";
        let (out, changed) = edit_hypr_lua(src, XKB_OPTION, "caps:ctrl_modifier,scrolllock:mod3");
        assert!(changed);
        assert!(
            out.contains("kb_options = \"caps:ctrl_modifier,scrolllock:mod3\""),
            "{out}"
        );
    }

    #[test]
    fn hypr_lua_appends_config_call() {
        let (out, changed) = edit_hypr_lua("-- empty\n", XKB_OPTION, XKB_OPTION);
        assert!(changed);
        assert!(out.contains("hl.config({ input = { kb_options = \"scrolllock:mod3\" } })"));
        let (_, changed) = edit_hypr_lua(&out, XKB_OPTION, XKB_OPTION);
        assert!(!changed);
    }

    #[test]
    fn sway_edits_and_appends() {
        let src = "input \"type:keyboard\" {\n    xkb_options \"caps:ctrl_modifier\"\n}\n";
        let (out, changed) = edit_sway_conf(src, XKB_OPTION, XKB_OPTION);
        assert!(changed);
        assert!(out.contains("xkb_options \"caps:ctrl_modifier,scrolllock:mod3\""));
        let (again, changed) = edit_sway_conf(&out, XKB_OPTION, XKB_OPTION);
        assert!(!changed);
        assert_eq!(again, out);

        let (out, changed) = edit_sway_conf("", XKB_OPTION, XKB_OPTION);
        assert!(changed);
        assert!(out.contains("input \"type:keyboard\" xkb_options scrolllock:mod3"));
    }

    #[test]
    fn kxkbrc_creates_and_merges_options() {
        let (out, changed) = edit_kxkbrc("", XKB_OPTION, XKB_OPTION);
        assert!(changed);
        assert!(out.contains("[Layout]"));
        assert!(out.contains("Options=scrolllock:mod3"));

        let src = "[Layout]\nUse=true\nOptions=grp:alt_shift_toggle\n\n[Other]\nX=1\n";
        let (out, changed) = edit_kxkbrc(src, XKB_OPTION, XKB_OPTION);
        assert!(changed);
        assert!(out.contains("Options=grp:alt_shift_toggle,scrolllock:mod3"));
        assert!(out.contains("[Other]\nX=1"));
        let (again, changed) = edit_kxkbrc(&out, XKB_OPTION, XKB_OPTION);
        assert!(!changed);
        assert_eq!(again, out);
    }

    #[test]
    fn niri_adds_options_block() {
        let (out, changed) = edit_niri_kdl("", XKB_OPTION, XKB_OPTION);
        assert!(changed);
        assert!(out.contains("options \"scrolllock:mod3\""));
        assert!(out.contains("xkb {"));
    }

    #[test]
    fn niri_replaces_existing_options() {
        let src = "input {\n    keyboard {\n        xkb {\n            options \"caps:ctrl_modifier\"\n        }\n    }\n}\n";
        let (out, changed) = edit_niri_kdl(src, XKB_OPTION, XKB_OPTION);
        assert!(changed);
        assert!(out.contains("options \"caps:ctrl_modifier,scrolllock:mod3\""));
        let (_, changed) = edit_niri_kdl(&out, XKB_OPTION, XKB_OPTION);
        assert!(!changed);
    }

    #[test]
    fn envd_merges_existing_value() {
        let src = "XKB_DEFAULT_OPTIONS=caps:ctrl_modifier\n";
        let (out, changed) = edit_envd(src, XKB_OPTION, XKB_OPTION);
        assert!(changed);
        assert_eq!(
            out.trim(),
            "XKB_DEFAULT_OPTIONS=caps:ctrl_modifier,scrolllock:mod3"
        );
        let (_, changed) = edit_envd(&out, XKB_OPTION, XKB_OPTION);
        assert!(!changed);
    }

    #[test]
    fn xprofile_block_is_updated_in_place() {
        let (out, changed) = edit_xprofile("", XKB_OPTION, XKB_OPTION);
        assert!(changed);
        assert!(out.contains(XPROFILE_BEGIN));
        assert!(out.contains("setxkbmap -option \"scrolllock:mod3\""));
        let (again, changed) = edit_xprofile(&out, XKB_OPTION, XKB_OPTION);
        assert!(!changed);
        assert_eq!(again, out);

        let empty = format!("{XPROFILE_BEGIN}\nsetxkbmap -option \"\"\n{XPROFILE_END}\n");
        let (out, changed) = edit_xprofile(&empty, XKB_OPTION, XKB_OPTION);
        assert!(changed);
        assert!(out.contains("setxkbmap -option \"scrolllock:mod3\""));
        assert_eq!(out.matches("setxkbmap").count(), 1);
    }

    #[test]
    fn readers_find_the_option() {
        assert_eq!(
            hypr_options_from_text("input {\n kb_options = us,ru\n}\n"),
            "us,ru"
        );
        assert_eq!(
            hypr_options_from_text("hl.config({ input = { kb_options = \"us,ru\" } })\n"),
            "us,ru"
        );
        assert_eq!(
            sway_options_from_text("input type:keyboard xkb_options grp:alt_shift_toggle\n"),
            "grp:alt_shift_toggle"
        );
        assert_eq!(
            kde_options_from_text("[Layout]\nOptions=grp:alt_shift_toggle\n"),
            "grp:alt_shift_toggle"
        );
        assert_eq!(envd_options_from_text("XKB_DEFAULT_OPTIONS=foo\n"), "foo");
        assert_eq!(
            xprofile_options_from_text("setxkbmap -option \"foo\"\n"),
            "foo"
        );
    }

    #[test]
    fn command_output_parsers() {
        assert_eq!(
            parse_hypr_getoption("str: \"caps:ctrl_modifier\"\nset: true\n"),
            "caps:ctrl_modifier"
        );
        assert_eq!(
            parse_setxkbmap_options("rules: evdev\noptions: caps:ctrl_modifier\n"),
            "caps:ctrl_modifier"
        );
        assert_eq!(
            parse_gsettings_list("['caps:ctrl_modifier', 'scrolllock:mod3']"),
            vec!["caps:ctrl_modifier", "scrolllock:mod3"]
        );
        assert!(parse_gsettings_list("@as []").is_empty());
        assert_eq!(
            format_gsettings_list(&["a".into(), "b".into()]),
            "['a', 'b']"
        );
        assert_eq!(
            extract_setxkbmap_options("setxkbmap -option \"foo,bar\"").as_deref(),
            Some("foo,bar")
        );
    }
}
