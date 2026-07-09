use anyhow::{anyhow, Result};
use crossterm::event::{KeyCode, KeyModifiers};

pub fn encode_key_specs(keys: &[String]) -> Result<String> {
    if keys.is_empty() {
        return Err(anyhow!("at least one key is required"));
    }
    let mut data = String::new();
    for key in keys {
        data.push_str(&encode_key_spec(key)?);
    }
    Ok(data)
}

pub fn encode_key_spec(spec: &str) -> Result<String> {
    let (code, modifiers) = parse_key_binding(spec)?;
    key_to_input(code, modifiers).ok_or_else(|| anyhow!("unsupported key {spec}"))
}

pub fn parse_key_binding(spec: &str) -> Result<(KeyCode, KeyModifiers)> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(anyhow!("key cannot be empty"));
    }

    let parts = spec
        .split(['+', '-'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let Some(key) = parts.last() else {
        return Err(anyhow!("key cannot be empty"));
    };

    let mut modifiers = KeyModifiers::empty();
    for modifier in &parts[..parts.len().saturating_sub(1)] {
        match modifier.to_ascii_lowercase().as_str() {
            "c" | "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
            "a" | "alt" | "meta" | "option" => modifiers |= KeyModifiers::ALT,
            "s" | "shift" => modifiers |= KeyModifiers::SHIFT,
            other => return Err(anyhow!("unknown key modifier {other}")),
        }
    }

    let code = parse_key_code(key)?;
    Ok((code, modifiers))
}

fn parse_key_code(key: &str) -> Result<KeyCode> {
    let lower = key.to_ascii_lowercase();
    let code = match lower.as_str() {
        "enter" | "return" => KeyCode::Enter,
        "backspace" | "bs" => KeyCode::Backspace,
        "tab" => KeyCode::Tab,
        "backtab" | "shift-tab" | "shift+tab" => KeyCode::BackTab,
        "esc" | "escape" => KeyCode::Esc,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "page-up" | "pgup" => KeyCode::PageUp,
        "pagedown" | "page-down" | "pgdown" => KeyCode::PageDown,
        "insert" | "ins" => KeyCode::Insert,
        "delete" | "del" => KeyCode::Delete,
        "space" => KeyCode::Char(' '),
        _ if lower.starts_with('f') => {
            let n = lower[1..]
                .parse::<u8>()
                .map_err(|_| anyhow!("unknown key {key}"))?;
            KeyCode::F(n)
        }
        _ => {
            let mut chars = key.chars();
            let Some(ch) = chars.next() else {
                return Err(anyhow!("key cannot be empty"));
            };
            if chars.next().is_some() {
                return Err(anyhow!("unknown key {key}"));
            }
            KeyCode::Char(ch)
        }
    };
    Ok(code)
}

pub fn key_to_input(code: KeyCode, modifiers: KeyModifiers) -> Option<String> {
    let mut data = match code {
        KeyCode::Enter => modified_other_key(13, modifiers).unwrap_or_else(|| "\r".to_string()),
        KeyCode::Backspace => {
            modified_other_key(127, modifiers).unwrap_or_else(|| "\u{7f}".to_string())
        }
        KeyCode::BackTab => "\x1b[Z".to_string(),
        KeyCode::Tab if modifiers.contains(KeyModifiers::CONTROL) => {
            modified_other_key(9, modifiers).unwrap_or_else(|| "\t".to_string())
        }
        KeyCode::Tab if modifiers.contains(KeyModifiers::SHIFT) => "\x1b[Z".to_string(),
        KeyCode::Tab => "\t".to_string(),
        KeyCode::Esc => "\x1b".to_string(),
        KeyCode::Left => modified_arrow("D", modifiers),
        KeyCode::Right => modified_arrow("C", modifiers),
        KeyCode::Up => modified_arrow("A", modifiers),
        KeyCode::Down => modified_arrow("B", modifiers),
        KeyCode::Home => modified_csi("H", modifiers),
        KeyCode::End => modified_csi("F", modifiers),
        KeyCode::PageUp => modified_tilde(5, modifiers),
        KeyCode::PageDown => modified_tilde(6, modifiers),
        KeyCode::Insert => modified_tilde(2, modifiers),
        KeyCode::Delete => modified_tilde(3, modifiers),
        KeyCode::F(n) => function_key(n, modifiers)?,
        KeyCode::Char(ch) => char_to_input(ch, modifiers)?,
        _ => return None,
    };
    if modifiers.contains(KeyModifiers::ALT) && !data.starts_with('\x1b') {
        data.insert(0, '\x1b');
    }
    Some(data)
}

fn char_to_input(ch: char, modifiers: KeyModifiers) -> Option<String> {
    if modifiers.contains(KeyModifiers::CONTROL) {
        return ctrl_char(ch).map(|byte| (byte as char).to_string());
    }
    Some(ch.to_string())
}

fn ctrl_char(ch: char) -> Option<u8> {
    match ch {
        'a'..='z' => Some(ch as u8 - b'a' + 1),
        'A'..='Z' => Some(ch as u8 - b'A' + 1),
        ' ' | '2' => Some(0),
        '[' | '3' => Some(27),
        '\\' | '4' => Some(28),
        ']' | '5' => Some(29),
        '^' | '6' => Some(30),
        '_' | '7' | '/' => Some(31),
        '8' | '?' => Some(127),
        _ => None,
    }
}

fn modified_arrow(final_byte: &str, modifiers: KeyModifiers) -> String {
    if let Some(code) = modifier_code(modifiers) {
        format!("\x1b[1;{code}{final_byte}")
    } else {
        format!("\x1b[{final_byte}")
    }
}

fn modified_csi(final_byte: &str, modifiers: KeyModifiers) -> String {
    if let Some(code) = modifier_code(modifiers) {
        format!("\x1b[1;{code}{final_byte}")
    } else {
        format!("\x1b[{final_byte}")
    }
}

fn modified_tilde(number: u8, modifiers: KeyModifiers) -> String {
    if let Some(code) = modifier_code(modifiers) {
        format!("\x1b[{number};{code}~")
    } else {
        format!("\x1b[{number}~")
    }
}

fn function_key(n: u8, modifiers: KeyModifiers) -> Option<String> {
    let base = match n {
        1 => return Some(modified_csi("P", modifiers)),
        2 => return Some(modified_csi("Q", modifiers)),
        3 => return Some(modified_csi("R", modifiers)),
        4 => return Some(modified_csi("S", modifiers)),
        5 => 15,
        6 => 17,
        7 => 18,
        8 => 19,
        9 => 20,
        10 => 21,
        11 => 23,
        12 => 24,
        _ => return None,
    };
    Some(modified_tilde(base, modifiers))
}

/// Encode Ctrl-modified Enter/Tab/Backspace as xterm modifyOtherKeys
/// (CSI-u) sequences, e.g. Ctrl+Enter -> `\x1b[13;5u`. Returns `None`
/// when the Control modifier is absent so callers fall back to the plain
/// control byte (`\r`, `\t`, DEL).
fn modified_other_key(code: u32, modifiers: KeyModifiers) -> Option<String> {
    if !modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }
    let modifier = modifier_code(modifiers)?;
    Some(format!("\x1b[{code};{modifier}u"))
}

fn modifier_code(modifiers: KeyModifiers) -> Option<u8> {
    let shift = modifiers.contains(KeyModifiers::SHIFT);
    let alt = modifiers.contains(KeyModifiers::ALT);
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    match (shift, alt, ctrl) {
        (false, false, false) => None,
        (true, false, false) => Some(2),
        (false, true, false) => Some(3),
        (true, true, false) => Some(4),
        (false, false, true) => Some(5),
        (true, false, true) => Some(6),
        (false, true, true) => Some(7),
        (true, true, true) => Some(8),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_common_navigation_keys_to_escape_sequences() {
        assert_eq!(
            key_to_input(KeyCode::Left, KeyModifiers::empty()).as_deref(),
            Some("\x1b[D")
        );
        assert_eq!(
            key_to_input(KeyCode::Right, KeyModifiers::CONTROL).as_deref(),
            Some("\x1b[1;5C")
        );
        assert_eq!(
            key_to_input(KeyCode::Delete, KeyModifiers::SHIFT).as_deref(),
            Some("\x1b[3;2~")
        );
        assert_eq!(
            key_to_input(KeyCode::BackTab, KeyModifiers::empty()).as_deref(),
            Some("\x1b[Z")
        );
        assert_eq!(
            key_to_input(KeyCode::F(5), KeyModifiers::empty()).as_deref(),
            Some("\x1b[15~")
        );
    }

    #[test]
    fn maps_control_and_alt_characters() {
        assert_eq!(
            key_to_input(KeyCode::Char('c'), KeyModifiers::CONTROL).as_deref(),
            Some("\x03")
        );
        assert_eq!(
            key_to_input(KeyCode::Char('x'), KeyModifiers::ALT).as_deref(),
            Some("\x1bx")
        );
        assert_eq!(
            key_to_input(
                KeyCode::Char('m'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            )
            .as_deref(),
            Some("\x1b\r")
        );
    }

    #[test]
    fn maps_ctrl_modified_control_keys_to_csi_u() {
        assert_eq!(
            key_to_input(KeyCode::Enter, KeyModifiers::CONTROL).as_deref(),
            Some("\x1b[13;5u")
        );
        assert_eq!(
            key_to_input(KeyCode::Tab, KeyModifiers::CONTROL).as_deref(),
            Some("\x1b[9;5u")
        );
        assert_eq!(
            key_to_input(KeyCode::Backspace, KeyModifiers::CONTROL).as_deref(),
            Some("\x1b[127;5u")
        );
        // Plain and shift-only variants keep their legacy encodings.
        assert_eq!(
            key_to_input(KeyCode::Enter, KeyModifiers::empty()).as_deref(),
            Some("\r")
        );
        assert_eq!(
            key_to_input(KeyCode::Tab, KeyModifiers::SHIFT).as_deref(),
            Some("\x1b[Z")
        );
    }

    #[test]
    fn parses_named_key_specs() {
        assert_eq!(encode_key_spec("enter").unwrap(), "\r");
        assert_eq!(encode_key_spec("C-c").unwrap(), "\x03");
        assert_eq!(encode_key_spec("Ctrl+Right").unwrap(), "\x1b[1;5C");
        assert_eq!(encode_key_spec("Shift-Tab").unwrap(), "\x1b[Z");
        assert_eq!(encode_key_spec("Alt-x").unwrap(), "\x1bx");
    }

    #[test]
    fn parses_key_bindings_for_ui_config() {
        let (code, modifiers) = parse_key_binding("Ctrl-b").unwrap();
        assert_eq!(code, KeyCode::Char('b'));
        assert_eq!(modifiers, KeyModifiers::CONTROL);
        let (code, modifiers) = parse_key_binding("Alt-x").unwrap();
        assert_eq!(code, KeyCode::Char('x'));
        assert_eq!(modifiers, KeyModifiers::ALT);
    }

    #[test]
    fn joins_key_specs_in_order() {
        let keys = vec!["a".to_string(), "enter".to_string()];
        assert_eq!(encode_key_specs(&keys).unwrap(), "a\r");
    }

    #[test]
    fn rejects_empty_key_sequence() {
        assert!(encode_key_specs(&[]).is_err());
    }
}
