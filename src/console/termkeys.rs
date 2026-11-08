//! Keyboard input for the terminal console: egui keys to the byte sequences
//! an xterm-compatible terminal sends.
//!
//! Printable text arrives as `egui::Event::Text` and is sent as UTF-8
//! ([`text_bytes`]); everything that produces no text (Enter, arrows,
//! function keys, Ctrl combinations) goes through [`key_bytes`].

use egui::{Key, Modifiers};

/// The bytes for a key press, or `None` when the key is printable and will
/// arrive as text (or has no terminal meaning).
pub fn key_bytes(key: Key, mods: Modifiers, app_cursor: bool) -> Option<Vec<u8>> {
    let ctrl = mods.ctrl;
    let alt = mods.alt;
    // xterm's modifier parameter: 1 + shift + 2*alt + 4*ctrl.
    let m = 1 + u8::from(mods.shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    let csi_mod = |final_byte: char| -> Vec<u8> { format!("\x1b[1;{m}{final_byte}").into_bytes() };
    let cursor = |final_byte: char| -> Vec<u8> {
        if m > 1 {
            csi_mod(final_byte)
        } else if app_cursor {
            format!("\x1bO{final_byte}").into_bytes()
        } else {
            format!("\x1b[{final_byte}").into_bytes()
        }
    };
    let tilde = |n: u8| -> Vec<u8> { if m > 1 { format!("\x1b[{n};{m}~").into_bytes() } else { format!("\x1b[{n}~").into_bytes() } };
    let ss3 = |final_byte: char| -> Vec<u8> { if m > 1 { csi_mod(final_byte) } else { format!("\x1bO{final_byte}").into_bytes() } };

    let seq = match key {
        Key::Enter => alt_prefix(alt, b"\r"),
        Key::Tab if mods.shift => b"\x1b[Z".to_vec(),
        Key::Tab => alt_prefix(alt, b"\t"),
        Key::Backspace if ctrl => alt_prefix(alt, b"\x08"),
        Key::Backspace => alt_prefix(alt, b"\x7f"),
        Key::Escape => b"\x1b".to_vec(),
        Key::ArrowUp => cursor('A'),
        Key::ArrowDown => cursor('B'),
        Key::ArrowRight => cursor('C'),
        Key::ArrowLeft => cursor('D'),
        Key::Home => cursor('H'),
        Key::End => cursor('F'),
        Key::Insert => tilde(2),
        Key::Delete => tilde(3),
        Key::PageUp => tilde(5),
        Key::PageDown => tilde(6),
        Key::F1 => ss3('P'),
        Key::F2 => ss3('Q'),
        Key::F3 => ss3('R'),
        Key::F4 => ss3('S'),
        Key::F5 => tilde(15),
        Key::F6 => tilde(17),
        Key::F7 => tilde(18),
        Key::F8 => tilde(19),
        Key::F9 => tilde(20),
        Key::F10 => tilde(21),
        Key::F11 => tilde(23),
        Key::F12 => tilde(24),
        _ if ctrl => return ctrl_byte(key).map(|b| alt_prefix(alt, &[b])),
        _ => return None,
    };
    Some(seq)
}

/// Ctrl+key as a C0 control byte (Ctrl+A = 0x01 .. Ctrl+Z = 0x1a, Ctrl+[ = ESC, ...).
pub fn ctrl_byte(key: Key) -> Option<u8> {
    let name = key.name();
    if name.len() == 1 {
        let c = name.as_bytes()[0];
        if c.is_ascii_uppercase() {
            return Some(c - b'A' + 1);
        }
    }
    Some(match key {
        Key::Space | Key::Num2 => 0x00,
        Key::OpenBracket | Key::Num3 => 0x1b,
        Key::Backslash | Key::Num4 => 0x1c,
        Key::CloseBracket | Key::Num5 => 0x1d,
        Key::Num6 => 0x1e,
        Key::Minus | Key::Slash | Key::Num7 => 0x1f,
        Key::Num8 => 0x7f,
        _ => return None,
    })
}

fn alt_prefix(alt: bool, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 1);
    if alt {
        out.push(0x1b);
    }
    out.extend_from_slice(bytes);
    out
}

/// Typed text as UTF-8; with Alt (Meta) held, ESC-prefixed like xterm's
/// `metaSendsEscape`.
pub fn text_bytes(text: &str, alt: bool) -> Vec<u8> {
    alt_prefix(alt, text.as_bytes())
}

/// Pasted text: line breaks become CR (what Enter sends); wrapped in the
/// bracketed-paste markers when the application asked for them.
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
    let mut out = Vec::with_capacity(normalized.len() + 12);
    if bracketed {
        out.extend_from_slice(b"\x1b[200~");
    }
    // A pasted ESC[201~ must not end the bracket early.
    out.extend_from_slice(if bracketed { normalized.replace("\x1b[201~", "") } else { normalized }.as_bytes());
    if bracketed {
        out.extend_from_slice(b"\x1b[201~");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> Modifiers {
        Modifiers::NONE
    }
    fn ctrl() -> Modifiers {
        Modifiers { ctrl: true, command: !cfg!(target_os = "macos"), ..Modifiers::NONE }
    }

    #[test]
    fn cursor_keys_follow_application_mode() {
        assert_eq!(key_bytes(Key::ArrowUp, none(), false).unwrap(), b"\x1b[A");
        assert_eq!(key_bytes(Key::ArrowUp, none(), true).unwrap(), b"\x1bOA");
        assert_eq!(key_bytes(Key::ArrowLeft, none(), false).unwrap(), b"\x1b[D");
        assert_eq!(key_bytes(Key::ArrowRight, none(), true).unwrap(), b"\x1bOC");
        assert_eq!(key_bytes(Key::Home, none(), false).unwrap(), b"\x1b[H");
        assert_eq!(key_bytes(Key::End, none(), true).unwrap(), b"\x1bOF");
        // Modified arrows use the CSI 1;m form in both modes.
        assert_eq!(key_bytes(Key::ArrowUp, ctrl(), true).unwrap(), b"\x1b[1;5A");
        assert_eq!(key_bytes(Key::ArrowDown, Modifiers::SHIFT, false).unwrap(), b"\x1b[1;2B");
    }

    #[test]
    fn editing_and_function_keys() {
        assert_eq!(key_bytes(Key::Enter, none(), false).unwrap(), b"\r");
        assert_eq!(key_bytes(Key::Backspace, none(), false).unwrap(), b"\x7f");
        assert_eq!(key_bytes(Key::Tab, none(), false).unwrap(), b"\t");
        assert_eq!(key_bytes(Key::Tab, Modifiers::SHIFT, false).unwrap(), b"\x1b[Z");
        assert_eq!(key_bytes(Key::Escape, none(), false).unwrap(), b"\x1b");
        assert_eq!(key_bytes(Key::PageUp, none(), false).unwrap(), b"\x1b[5~");
        assert_eq!(key_bytes(Key::PageDown, none(), false).unwrap(), b"\x1b[6~");
        assert_eq!(key_bytes(Key::Insert, none(), false).unwrap(), b"\x1b[2~");
        assert_eq!(key_bytes(Key::Delete, none(), false).unwrap(), b"\x1b[3~");
        assert_eq!(key_bytes(Key::F1, none(), false).unwrap(), b"\x1bOP");
        assert_eq!(key_bytes(Key::F4, none(), false).unwrap(), b"\x1bOS");
        assert_eq!(key_bytes(Key::F5, none(), false).unwrap(), b"\x1b[15~");
        assert_eq!(key_bytes(Key::F10, none(), false).unwrap(), b"\x1b[21~");
        assert_eq!(key_bytes(Key::F12, none(), false).unwrap(), b"\x1b[24~");
        assert_eq!(key_bytes(Key::Delete, ctrl(), false).unwrap(), b"\x1b[3;5~");
    }

    #[test]
    fn ctrl_and_alt_combinations() {
        assert_eq!(key_bytes(Key::C, ctrl(), false).unwrap(), [0x03]);
        assert_eq!(key_bytes(Key::A, ctrl(), false).unwrap(), [0x01]);
        assert_eq!(key_bytes(Key::Z, ctrl(), false).unwrap(), [0x1a]);
        assert_eq!(key_bytes(Key::OpenBracket, ctrl(), false).unwrap(), [0x1b]);
        assert_eq!(key_bytes(Key::Space, ctrl(), false).unwrap(), [0x00]);
        let ctrl_alt = Modifiers { alt: true, ..ctrl() };
        assert_eq!(key_bytes(Key::X, ctrl_alt, false).unwrap(), [0x1b, 0x18]);
        assert_eq!(key_bytes(Key::Enter, Modifiers::ALT, false).unwrap(), b"\x1b\r");
        // Printable keys without Ctrl arrive as text.
        assert_eq!(key_bytes(Key::A, none(), false), None);
        assert_eq!(key_bytes(Key::Space, Modifiers::SHIFT, false), None);
        assert_eq!(text_bytes("ä", false), "ä".as_bytes());
        assert_eq!(text_bytes("b", true), b"\x1bb");
    }

    #[test]
    fn paste_normalizes_newlines_and_brackets() {
        assert_eq!(paste_bytes("a\nb\r\nc", false), b"a\rb\rc");
        assert_eq!(paste_bytes("x\x1b[201~y", true), b"\x1b[200~xy\x1b[201~");
    }
}
