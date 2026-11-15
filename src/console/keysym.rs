//! Keyboard input for the VNC console: egui events to X11 keysyms (RFB key
//! events carry keysyms; QEMU maps them to scancodes with its keymap).
//!
//! * Printable text (`Event::Text`) is sent as press+release of the
//!   character's keysym: Latin-1 code points are their own keysym, anything
//!   else is `0x0100_0000 + code point`.
//! * Keys that produce no text (Enter, arrows, F-keys...) map through the X11
//!   keysym table and keep their press/release timing.
//! * Ctrl+letter is reported by egui as a `Key` without `Text` (and Ctrl+C /
//!   Ctrl+X / Ctrl+V as Copy / Cut / Paste), so those are mapped from the key.
//! * egui has no events for modifier keys alone; the modifier state is
//!   diffed instead and Control/Alt/Shift/Super presses are sent before the
//!   key, releases after it.

use egui::{Event, Key, Modifiers};

pub const XK_BACKSPACE: u32 = 0xff08;
pub const XK_TAB: u32 = 0xff09;
pub const XK_RETURN: u32 = 0xff0d;
pub const XK_ESCAPE: u32 = 0xff1b;
pub const XK_HOME: u32 = 0xff50;
pub const XK_LEFT: u32 = 0xff51;
pub const XK_UP: u32 = 0xff52;
pub const XK_RIGHT: u32 = 0xff53;
pub const XK_DOWN: u32 = 0xff54;
pub const XK_PAGE_UP: u32 = 0xff55;
pub const XK_PAGE_DOWN: u32 = 0xff56;
pub const XK_END: u32 = 0xff57;
pub const XK_INSERT: u32 = 0xff63;
pub const XK_DELETE: u32 = 0xffff;
pub const XK_F1: u32 = 0xffbe;
pub const XK_SHIFT_L: u32 = 0xffe1;
pub const XK_CONTROL_L: u32 = 0xffe3;
pub const XK_ALT_L: u32 = 0xffe9;
pub const XK_SUPER_L: u32 = 0xffeb;

/// The keysym of a character.
pub fn char_keysym(c: char) -> u32 {
    match c {
        '\n' | '\r' => XK_RETURN,
        '\t' => XK_TAB,
        '\u{8}' => XK_BACKSPACE,
        '\u{1b}' => XK_ESCAPE,
        '\u{20}'..='\u{7e}' | '\u{a0}'..='\u{ff}' => c as u32,
        _ => 0x0100_0000 + c as u32,
    }
}

/// Keysyms of keys that never produce text.
pub fn special_keysym(key: Key) -> Option<u32> {
    Some(match key {
        Key::Backspace => XK_BACKSPACE,
        Key::Tab => XK_TAB,
        Key::Enter => XK_RETURN,
        Key::Escape => XK_ESCAPE,
        Key::Home => XK_HOME,
        Key::ArrowLeft => XK_LEFT,
        Key::ArrowUp => XK_UP,
        Key::ArrowRight => XK_RIGHT,
        Key::ArrowDown => XK_DOWN,
        Key::PageUp => XK_PAGE_UP,
        Key::PageDown => XK_PAGE_DOWN,
        Key::End => XK_END,
        Key::Insert => XK_INSERT,
        Key::Delete => XK_DELETE,
        k => {
            let n = function_key_number(k)?;
            XK_F1 + n - 1
        }
    })
}

fn function_key_number(key: Key) -> Option<u32> {
    let name = key.name();
    let n: u32 = name.strip_prefix('F')?.parse().ok()?;
    (1..=35).contains(&n).then_some(n)
}

/// The unshifted keysym of a printable key, for Ctrl/Cmd combinations that
/// produce no text.
pub fn plain_keysym(key: Key) -> Option<u32> {
    let name = key.name();
    if name.len() == 1 {
        let c = name.as_bytes()[0];
        if c.is_ascii_uppercase() {
            return Some(u32::from(c.to_ascii_lowercase()));
        }
        if c.is_ascii_digit() {
            return Some(u32::from(c));
        }
    }
    Some(match key {
        Key::Space => 0x20,
        Key::Minus => '-' as u32,
        Key::Equals | Key::Plus => '=' as u32,
        Key::OpenBracket => '[' as u32,
        Key::CloseBracket => ']' as u32,
        Key::Backslash => '\\' as u32,
        Key::Semicolon => ';' as u32,
        Key::Quote => '\'' as u32,
        Key::Backtick => '`' as u32,
        Key::Comma => ',' as u32,
        Key::Period => '.' as u32,
        Key::Slash => '/' as u32,
        _ => return None,
    })
}

/// Turns egui input into RFB key events, tracking what the server believes
/// is held down.
#[derive(Default)]
pub struct KeyMapper {
    /// Modifier state as last sent: (ctrl, alt, shift, super).
    sent: [bool; 4],
    /// Keysyms pressed through `Key` events and not released yet.
    held: Vec<u32>,
}

const MOD_KEYSYMS: [u32; 4] = [XK_CONTROL_L, XK_ALT_L, XK_SHIFT_L, XK_SUPER_L];

fn mod_state(m: Modifiers) -> [bool; 4] {
    [m.ctrl, m.alt, m.shift, cfg!(target_os = "macos") && m.mac_cmd]
}

impl KeyMapper {
    /// Brings the server's modifier state in line with `m`: presses in
    /// Ctrl, Alt, Shift, Super order, releases in reverse.
    pub fn sync_modifiers(&mut self, m: Modifiers, out: &mut Vec<(u32, bool)>) {
        let want = mod_state(m);
        for i in 0..4 {
            if want[i] && !self.sent[i] {
                out.push((MOD_KEYSYMS[i], true));
                self.sent[i] = true;
            }
        }
        for i in (0..4).rev() {
            if !want[i] && self.sent[i] {
                out.push((MOD_KEYSYMS[i], false));
                self.sent[i] = false;
            }
        }
    }

    /// Releases everything (focus lost, window closing).
    pub fn release_all(&mut self, out: &mut Vec<(u32, bool)>) {
        for k in self.held.drain(..) {
            out.push((k, false));
        }
        self.sync_modifiers(Modifiers::NONE, out);
    }

    fn tap(out: &mut Vec<(u32, bool)>, ks: u32) {
        out.push((ks, true));
        out.push((ks, false));
    }

    /// Translates one event. `Paste` text is not typed (use the clipboard
    /// button); with Ctrl held it is the Ctrl+V the user pressed.
    pub fn event(&mut self, ev: &Event, out: &mut Vec<(u32, bool)>) {
        match ev {
            Event::Key { key, pressed, modifiers, .. } => {
                if *pressed {
                    self.sync_modifiers(*modifiers, out);
                }
                let ks = special_keysym(*key).or_else(|| {
                    // Without Ctrl/Cmd the key's text arrives as Event::Text.
                    let no_text = modifiers.ctrl || (cfg!(target_os = "macos") && modifiers.mac_cmd);
                    if no_text || self.held.contains(&plain_keysym(*key).unwrap_or(0)) { plain_keysym(*key) } else { None }
                });
                if let Some(ks) = ks {
                    if *pressed {
                        if !self.held.contains(&ks) {
                            self.held.push(ks);
                        }
                        out.push((ks, true));
                    } else if let Some(i) = self.held.iter().position(|&k| k == ks) {
                        self.held.remove(i);
                        out.push((ks, false));
                    }
                }
            }
            Event::Text(t) => {
                for c in t.chars() {
                    Self::tap(out, char_keysym(c));
                }
            }
            Event::Copy => Self::tap(out, 'c' as u32),
            Event::Cut => Self::tap(out, 'x' as u32),
            Event::Paste(_) => Self::tap(out, 'v' as u32),
            Event::ModifiersChanged(m) => self.sync_modifiers(*m, out),
            Event::WindowFocused(false) => self.release_all(out),
            _ => {}
        }
    }
}

/// The Ctrl+Alt+Del sequence.
pub fn ctrl_alt_del() -> Vec<(u32, bool)> {
    vec![(XK_CONTROL_L, true), (XK_ALT_L, true), (XK_DELETE, true), (XK_DELETE, false), (XK_ALT_L, false), (XK_CONTROL_L, false)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(k: Key, pressed: bool, modifiers: Modifiers) -> Event {
        Event::Key { key: k, physical_key: None, pressed, repeat: false, modifiers }
    }
    fn ctrl() -> Modifiers {
        Modifiers { ctrl: true, command: !cfg!(target_os = "macos"), ..Modifiers::NONE }
    }

    #[test]
    fn characters() {
        assert_eq!(char_keysym('a'), 0x61);
        assert_eq!(char_keysym('Z'), 0x5a);
        assert_eq!(char_keysym(' '), 0x20);
        assert_eq!(char_keysym('ä'), 0xe4);
        assert_eq!(char_keysym('ß'), 0xdf);
        // Outside Latin-1: Unicode keysyms.
        assert_eq!(char_keysym('€'), 0x0100_20ac);
        assert_eq!(char_keysym('ж'), 0x0100_0436);
        assert_eq!(char_keysym('漢'), 0x0100_6f22);
        assert_eq!(char_keysym('\n'), XK_RETURN);
    }

    #[test]
    fn special_keys() {
        assert_eq!(special_keysym(Key::ArrowUp), Some(0xff52));
        assert_eq!(special_keysym(Key::ArrowLeft), Some(0xff51));
        assert_eq!(special_keysym(Key::Enter), Some(0xff0d));
        assert_eq!(special_keysym(Key::Escape), Some(0xff1b));
        assert_eq!(special_keysym(Key::Delete), Some(0xffff));
        assert_eq!(special_keysym(Key::F1), Some(0xffbe));
        assert_eq!(special_keysym(Key::F12), Some(0xffc9));
        assert_eq!(special_keysym(Key::A), None);
        assert_eq!(special_keysym(Key::Space), None);
    }

    #[test]
    fn typing_text_taps_keysyms() {
        let mut m = KeyMapper::default();
        let mut out = Vec::new();
        m.event(&key(Key::A, true, Modifiers::NONE), &mut out);
        m.event(&Event::Text("a".into()), &mut out);
        m.event(&key(Key::A, false, Modifiers::NONE), &mut out);
        assert_eq!(out, vec![(0x61, true), (0x61, false)]);

        let mut out = Vec::new();
        m.event(&Event::Text("ж".into()), &mut out);
        assert_eq!(out, vec![(0x0100_0436, true), (0x0100_0436, false)]);
    }

    #[test]
    fn shifted_text_holds_shift() {
        let mut m = KeyMapper::default();
        let mut out = Vec::new();
        m.event(&key(Key::A, true, Modifiers::SHIFT), &mut out);
        m.event(&Event::Text("A".into()), &mut out);
        m.event(&key(Key::A, false, Modifiers::SHIFT), &mut out);
        m.sync_modifiers(Modifiers::NONE, &mut out);
        assert_eq!(out, vec![(XK_SHIFT_L, true), (0x41, true), (0x41, false), (XK_SHIFT_L, false)]);
    }

    #[test]
    fn ctrl_combinations() {
        let mut m = KeyMapper::default();
        let mut out = Vec::new();
        // Ctrl+L: egui sends the key without text.
        m.event(&key(Key::L, true, ctrl()), &mut out);
        m.event(&key(Key::L, false, ctrl()), &mut out);
        m.sync_modifiers(Modifiers::NONE, &mut out);
        assert_eq!(out, vec![(XK_CONTROL_L, true), (0x6c, true), (0x6c, false), (XK_CONTROL_L, false)]);

        // Ctrl+C arrives as Event::Copy.
        let mut out = Vec::new();
        m.sync_modifiers(ctrl(), &mut out);
        m.event(&Event::Copy, &mut out);
        m.sync_modifiers(Modifiers::NONE, &mut out);
        assert_eq!(out, vec![(XK_CONTROL_L, true), (0x63, true), (0x63, false), (XK_CONTROL_L, false)]);

        // Ctrl released before the letter: the letter's release is still sent.
        let mut out = Vec::new();
        m.event(&key(Key::D, true, ctrl()), &mut out);
        m.event(&Event::ModifiersChanged(Modifiers::NONE), &mut out);
        m.event(&key(Key::D, false, Modifiers::NONE), &mut out);
        assert_eq!(out, vec![(XK_CONTROL_L, true), (0x64, true), (XK_CONTROL_L, false), (0x64, false)]);
    }

    #[test]
    fn arrows_and_function_keys_keep_timing() {
        let mut m = KeyMapper::default();
        let mut out = Vec::new();
        m.event(&key(Key::ArrowDown, true, Modifiers::NONE), &mut out);
        m.event(&key(Key::ArrowDown, true, Modifiers::NONE), &mut out); // auto-repeat
        m.event(&key(Key::ArrowDown, false, Modifiers::NONE), &mut out);
        m.event(&key(Key::F5, true, Modifiers::NONE), &mut out);
        assert_eq!(out, vec![(XK_DOWN, true), (XK_DOWN, true), (XK_DOWN, false), (0xffc2, true)]);
        let mut out = Vec::new();
        m.release_all(&mut out);
        assert_eq!(out, vec![(0xffc2, false)]);
    }

    #[test]
    fn ctrl_alt_del_order() {
        let s = ctrl_alt_del();
        assert_eq!(s.first(), Some(&(XK_CONTROL_L, true)));
        assert_eq!(s.last(), Some(&(XK_CONTROL_L, false)));
        assert!(s.contains(&(XK_DELETE, true)));
    }
}
