//! Keyboard input for embedded sessions: the keyboard-protocol modes an app has
//! asked its terminal for, and the bytes a key event has to become to reach that
//! app the way a real terminal would send it.
//!
//! Pure — no I/O, no ratatui. The daemon tracks [`KeyModes`] on its
//! authoritative emulator so an attach snapshot can re-establish them
//! ([`KeyModes::setup`]); the client tracks them on its own emulator and passes
//! them to [`encode_key`].
//!
//! Why any of this exists: in the legacy encoding, Enter is `\r` whatever the
//! modifiers, so Shift+Enter is indistinguishable from Enter. Agent CLIs (Claude
//! Code, Codex) ask the terminal for the kitty keyboard protocol and/or xterm's
//! modifyOtherKeys precisely so Shift+Enter can mean "newline, don't submit".
//! asm is that terminal as far as they're concerned, so it has to honour the
//! request.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Deepest kitty flag stack kept. The protocol asks terminals to bound it; an
/// app that pushes without popping would otherwise grow it forever.
const KITTY_STACK_MAX: usize = 16;

/// kitty "disambiguate escape codes" flag — the one that makes modified
/// Enter/Tab/Backspace (and Ctrl+Shift combos) distinguishable.
const KITTY_DISAMBIGUATE: u16 = 1;

/// The keyboard-reporting modes an app has switched on, read off the escape
/// sequences it writes (see [`KeyModes::on_csi`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyModes {
    /// kitty keyboard protocol flag stack (`CSI > flags u` pushes, `CSI < n u`
    /// pops); the top entry is in force.
    kitty: Vec<u16>,
    /// xterm modifyOtherKeys level (`CSI > 4 ; n m`); 0 = off.
    modify_other_keys: u16,
}

impl KeyModes {
    /// The kitty flags currently in force (0 when the app never asked).
    pub fn kitty_flags(&self) -> u16 {
        self.kitty.last().copied().unwrap_or(0)
    }

    pub fn modify_other_keys(&self) -> u16 {
        self.modify_other_keys
    }

    /// Feed one CSI sequence the emulator didn't handle itself (vt100's
    /// `Callbacks::unhandled_csi`). `i1` is the private marker (`>`, `<`, `=`),
    /// which vte reports as the first intermediate.
    pub fn on_csi(&mut self, i1: Option<u8>, params: &[&[u16]], c: char) {
        let p = |i: usize| params.get(i).and_then(|v| v.first()).copied();
        match (i1, c) {
            (Some(b'>'), 'u') => {
                if self.kitty.len() >= KITTY_STACK_MAX {
                    self.kitty.remove(0);
                }
                self.kitty.push(p(0).unwrap_or(0));
            }
            (Some(b'<'), 'u') => {
                let n = p(0).unwrap_or(1).max(1);
                for _ in 0..n {
                    self.kitty.pop();
                }
            }
            (Some(b'='), 'u') => {
                let flags = p(0).unwrap_or(0);
                let cur = self.kitty_flags();
                let new = match p(1).unwrap_or(1) {
                    1 => flags,
                    2 => cur | flags,
                    3 => cur & !flags,
                    _ => return,
                };
                match self.kitty.last_mut() {
                    Some(top) => *top = new,
                    None => self.kitty.push(new),
                }
            }
            (Some(b'>'), 'm') if p(0) == Some(4) => {
                self.modify_other_keys = p(1).unwrap_or(0);
            }
            _ => {}
        }
    }

    /// Escape sequences that put a fresh emulator into these same modes, for an
    /// attach snapshot. The whole kitty stack is replayed, not just its top, so a
    /// later pop by the app lands on the same flags it would have in the daemon.
    pub fn setup(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for flags in &self.kitty {
            out.extend_from_slice(format!("\x1b[>{flags}u").as_bytes());
        }
        if self.modify_other_keys > 0 {
            out.extend_from_slice(format!("\x1b[>4;{}m", self.modify_other_keys).as_bytes());
        }
        out
    }
}

/// The xterm modifier parameter: 1 + shift(1) + alt(2) + ctrl(4). `1` means
/// "no modifiers" and is never emitted on its own.
fn modifier_param(m: KeyModifiers) -> u16 {
    let mut p = 1;
    if m.contains(KeyModifiers::SHIFT) {
        p += 1;
    }
    if m.contains(KeyModifiers::ALT) {
        p += 2;
    }
    if m.contains(KeyModifiers::CONTROL) {
        p += 4;
    }
    p
}

/// The legacy control byte for Ctrl+`c`, where one exists.
fn ctrl_byte(c: char) -> Option<u8> {
    match c {
        'a'..='z' | 'A'..='Z' => Some((c.to_ascii_uppercase() as u8) & 0x1f),
        '@' | ' ' | '2' => Some(0x00),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '/' | '7' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
}

/// Translate a key event into the bytes a PTY app expects.
///
/// `modes` are the keyboard modes the receiving app enabled; `app_cursor` is its
/// DECCKM state (vt100's `application_cursor`), which switches the unmodified
/// arrows to `ESC O x` — vim, less and readline's vi mode all set it.
///
/// Modified Enter/Tab/Backspace is the case the legacy encoding cannot carry.
/// With kitty's disambiguate flag on it goes as `CSI code ; mods u`; with
/// modifyOtherKeys as `CSI 27 ; mods ; code ~`. With neither, Shift/Alt+Enter
/// goes as `ESC CR` (Meta+Enter), which is what Claude Code's `/terminal-setup`
/// configures terminals to send and what both Claude Code and Codex read as
/// "insert a newline" — sending the bare `\r` a legacy terminal would send
/// submits the prompt instead.
pub fn encode_key(key: &KeyEvent, modes: &KeyModes, app_cursor: bool) -> Option<Vec<u8>> {
    let m = key.modifiers;
    let ctrl = m.contains(KeyModifiers::CONTROL);
    let alt = m.contains(KeyModifiers::ALT);
    let shift = m.contains(KeyModifiers::SHIFT);
    let mods = modifier_param(m);
    let kitty = modes.kitty_flags() & KITTY_DISAMBIGUATE != 0;
    let other_keys = modes.modify_other_keys() > 0;
    let csi_u = |code: u32| format!("\x1b[{code};{mods}u").into_bytes();
    let csi_27 = |code: u32| format!("\x1b[27;{mods};{code}~").into_bytes();

    // Enter / Tab / Backspace carry their modifiers only via a keyboard mode.
    let special = match key.code {
        KeyCode::Enter => Some((13, b"\r".as_slice())),
        KeyCode::Tab => Some((9, b"\t".as_slice())),
        KeyCode::Backspace => Some((127, b"\x7f".as_slice())),
        _ => None,
    };
    if let Some((code, plain)) = special {
        if mods == 1 {
            return Some(plain.to_vec());
        }
        if kitty {
            return Some(csi_u(code));
        }
        if other_keys {
            return Some(csi_27(code));
        }
        return Some(match key.code {
            KeyCode::Enter if shift || alt => b"\x1b\r".to_vec(),
            // Ctrl+Backspace: the ^H most terminals send, which readline and
            // zle treat as backward-delete-char.
            KeyCode::Backspace if ctrl => {
                if alt {
                    b"\x1b\x08".to_vec()
                } else {
                    b"\x08".to_vec()
                }
            }
            _ if alt => [b"\x1b".as_slice(), plain].concat(),
            _ => plain.to_vec(),
        });
    }

    let mut out: Vec<u8> = Vec::new();
    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                // Ctrl+Shift+letter and Ctrl+<punctuation without a control
                // byte> only exist in a keyboard mode; legacy has to drop them
                // to the plain control byte (or the bare character).
                let lower = c.to_ascii_lowercase();
                if kitty && (shift || ctrl_byte(c).is_none()) {
                    return Some(csi_u(lower as u32));
                }
                if other_keys && shift && c.is_ascii_alphabetic() {
                    return Some(csi_27(lower as u32));
                }
                if alt {
                    out.push(0x1b);
                }
                match ctrl_byte(c) {
                    Some(b) => out.push(b),
                    None => {
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                    }
                }
            } else {
                if alt {
                    out.push(0x1b);
                }
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
        KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
        KeyCode::Esc => {
            if alt {
                out.push(0x1b);
            }
            out.push(0x1b);
        }
        KeyCode::Up
        | KeyCode::Down
        | KeyCode::Right
        | KeyCode::Left
        | KeyCode::Home
        | KeyCode::End => {
            let letter = match key.code {
                KeyCode::Up => 'A',
                KeyCode::Down => 'B',
                KeyCode::Right => 'C',
                KeyCode::Left => 'D',
                KeyCode::Home => 'H',
                _ => 'F',
            };
            if mods > 1 {
                out.extend_from_slice(format!("\x1b[1;{mods}{letter}").as_bytes());
            } else if app_cursor {
                out.extend_from_slice(format!("\x1bO{letter}").as_bytes());
            } else {
                out.extend_from_slice(format!("\x1b[{letter}").as_bytes());
            }
        }
        KeyCode::Insert | KeyCode::Delete | KeyCode::PageUp | KeyCode::PageDown => {
            let n = match key.code {
                KeyCode::Insert => 2,
                KeyCode::Delete => 3,
                KeyCode::PageUp => 5,
                _ => 6,
            };
            out.extend_from_slice(tilde(n, mods).as_slice());
        }
        KeyCode::F(n @ 1..=4) => {
            let letter = (b'P' + (n - 1)) as char;
            if mods > 1 {
                out.extend_from_slice(format!("\x1b[1;{mods}{letter}").as_bytes());
            } else {
                out.extend_from_slice(format!("\x1bO{letter}").as_bytes());
            }
        }
        KeyCode::F(n @ 5..=12) => {
            let code = [15, 17, 18, 19, 20, 21, 23, 24][usize::from(n - 5)];
            out.extend_from_slice(tilde(code, mods).as_slice());
        }
        _ => return None,
    }
    Some(out)
}

/// `CSI n ~`, or `CSI n ; mods ~` when modified.
fn tilde(n: u16, mods: u16) -> Vec<u8> {
    if mods > 1 {
        format!("\x1b[{n};{mods}~").into_bytes()
    } else {
        format!("\x1b[{n}~").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    /// Feed `bytes` through a real emulator and return the modes it reported.
    fn modes_after(bytes: &[u8]) -> KeyModes {
        struct Track(KeyModes);
        impl vt100::Callbacks for Track {
            fn unhandled_csi(
                &mut self,
                _: &mut vt100::Screen,
                i1: Option<u8>,
                _i2: Option<u8>,
                params: &[&[u16]],
                c: char,
            ) {
                self.0.on_csi(i1, params, c);
            }
        }
        let mut p = vt100::Parser::new_with_callbacks(24, 80, 0, Track(KeyModes::default()));
        p.process(bytes);
        p.callbacks().0.clone()
    }

    fn enc(code: KeyCode, m: KeyModifiers, modes: &KeyModes) -> String {
        String::from_utf8(encode_key(&key(code, m), modes, false).unwrap()).unwrap()
    }

    const NONE: KeyModifiers = KeyModifiers::NONE;
    const SHIFT: KeyModifiers = KeyModifiers::SHIFT;
    const CTRL: KeyModifiers = KeyModifiers::CONTROL;
    const ALT: KeyModifiers = KeyModifiers::ALT;

    #[test]
    fn claude_codes_startup_sequences_turn_on_kitty_and_modify_other_keys() {
        // Captured from Claude Code 2.x starting in a PTY: it pushes kitty flags
        // 5 and sets modifyOtherKeys 2 (and queries `CSI ? u`, which is inert).
        let m = modes_after(b"\x1b[?u\x1b[>5u\x1b[>4;2m");
        assert_eq!(m.kitty_flags(), 5);
        assert_eq!(m.modify_other_keys(), 2);
    }

    #[test]
    fn popping_the_kitty_stack_restores_the_previous_flags() {
        let m = modes_after(b"\x1b[>1u\x1b[>5u\x1b[<u");
        assert_eq!(m.kitty_flags(), 1);
        let m = modes_after(b"\x1b[>1u\x1b[>5u\x1b[<2u");
        assert_eq!(m.kitty_flags(), 0);
        // Over-popping an empty stack is harmless.
        let m = modes_after(b"\x1b[<5u");
        assert_eq!(m.kitty_flags(), 0);
    }

    #[test]
    fn kitty_set_mode_replaces_ors_and_clears_bits() {
        assert_eq!(modes_after(b"\x1b[>4u\x1b[=1;1u").kitty_flags(), 1);
        assert_eq!(modes_after(b"\x1b[>4u\x1b[=1;2u").kitty_flags(), 5);
        assert_eq!(modes_after(b"\x1b[>5u\x1b[=4;3u").kitty_flags(), 1);
    }

    #[test]
    fn modify_other_keys_turns_off_again() {
        assert_eq!(modes_after(b"\x1b[>4;2m\x1b[>4m").modify_other_keys(), 0);
        assert_eq!(modes_after(b"\x1b[>4;2m\x1b[>4;0m").modify_other_keys(), 0);
    }

    #[test]
    fn plain_sgr_is_not_mistaken_for_modify_other_keys() {
        // `CSI 4 ; 2 m` is underline + dim, not `CSI > 4 ; 2 m`.
        assert_eq!(modes_after(b"\x1b[4;2m").modify_other_keys(), 0);
    }

    #[test]
    fn the_kitty_stack_is_bounded() {
        let many: Vec<u8> = (0..100).flat_map(|_| b"\x1b[>1u".to_vec()).collect();
        let m = modes_after(&many);
        assert_eq!(m.kitty.len(), KITTY_STACK_MAX);
    }

    #[test]
    fn setup_reproduces_the_same_modes_in_a_fresh_emulator() {
        let m = modes_after(b"\x1b[>1u\x1b[>5u\x1b[>4;2m");
        assert_eq!(modes_after(&m.setup()), m);
        assert!(KeyModes::default().setup().is_empty());
    }

    #[test]
    fn shift_enter_under_kitty_is_csi_u() {
        let m = modes_after(b"\x1b[>5u");
        assert_eq!(enc(KeyCode::Enter, SHIFT, &m), "\x1b[13;2u");
        assert_eq!(enc(KeyCode::Enter, CTRL, &m), "\x1b[13;5u");
        assert_eq!(enc(KeyCode::Enter, ALT, &m), "\x1b[13;3u");
        // An unmodified Enter is still a bare CR: kitty only re-encodes what
        // legacy got wrong.
        assert_eq!(enc(KeyCode::Enter, NONE, &m), "\r");
    }

    #[test]
    fn shift_enter_under_modify_other_keys_is_csi_27() {
        let m = modes_after(b"\x1b[>4;2m");
        assert_eq!(enc(KeyCode::Enter, SHIFT, &m), "\x1b[27;2;13~");
    }

    #[test]
    fn kitty_wins_over_modify_other_keys() {
        let m = modes_after(b"\x1b[>4;2m\x1b[>1u");
        assert_eq!(enc(KeyCode::Enter, SHIFT, &m), "\x1b[13;2u");
    }

    #[test]
    fn a_kitty_push_without_disambiguate_leaves_legacy_encoding() {
        // Flags 8 = "report all keys as escape codes" alone, without 1; asm only
        // speaks the disambiguate subset, so it must not claim the rest.
        let m = modes_after(b"\x1b[>8u");
        assert_eq!(enc(KeyCode::Enter, SHIFT, &m), "\x1b\r");
    }

    #[test]
    fn shift_or_alt_enter_without_a_keyboard_mode_is_meta_enter() {
        let m = KeyModes::default();
        assert_eq!(enc(KeyCode::Enter, SHIFT, &m), "\x1b\r");
        assert_eq!(enc(KeyCode::Enter, ALT, &m), "\x1b\r");
        assert_eq!(enc(KeyCode::Enter, NONE, &m), "\r");
    }

    #[test]
    fn alt_backspace_deletes_a_word() {
        // readline/zle/Claude Code: ESC DEL = backward-kill-word. Dropping the
        // Alt left it a plain one-character backspace.
        let m = KeyModes::default();
        assert_eq!(enc(KeyCode::Backspace, ALT, &m), "\x1b\x7f");
        assert_eq!(enc(KeyCode::Backspace, CTRL, &m), "\x08");
        assert_eq!(enc(KeyCode::Backspace, NONE, &m), "\x7f");
    }

    #[test]
    fn modified_arrows_carry_their_modifiers() {
        // Alt/Ctrl+Left/Right are word motion in shells and in both agent CLIs.
        let m = KeyModes::default();
        assert_eq!(enc(KeyCode::Left, ALT, &m), "\x1b[1;3D");
        assert_eq!(enc(KeyCode::Right, CTRL, &m), "\x1b[1;5C");
        assert_eq!(enc(KeyCode::Up, SHIFT, &m), "\x1b[1;2A");
        assert_eq!(enc(KeyCode::Delete, CTRL, &m), "\x1b[3;5~");
        assert_eq!(enc(KeyCode::Left, NONE, &m), "\x1b[D");
    }

    #[test]
    fn application_cursor_mode_switches_plain_arrows_to_ss3() {
        let m = KeyModes::default();
        let up = encode_key(&key(KeyCode::Up, NONE), &m, true).unwrap();
        assert_eq!(up, b"\x1bOA");
        let home = encode_key(&key(KeyCode::Home, NONE), &m, true).unwrap();
        assert_eq!(home, b"\x1bOH");
        // A modified arrow is CSI in either mode.
        let alt_up = encode_key(&key(KeyCode::Up, ALT), &m, true).unwrap();
        assert_eq!(alt_up, b"\x1b[1;3A");
    }

    #[test]
    fn function_keys_are_forwarded() {
        let m = KeyModes::default();
        assert_eq!(enc(KeyCode::F(1), NONE, &m), "\x1bOP");
        assert_eq!(enc(KeyCode::F(4), NONE, &m), "\x1bOS");
        assert_eq!(enc(KeyCode::F(5), NONE, &m), "\x1b[15~");
        assert_eq!(enc(KeyCode::F(12), NONE, &m), "\x1b[24~");
        assert_eq!(enc(KeyCode::F(2), SHIFT, &m), "\x1b[1;2Q");
        assert_eq!(enc(KeyCode::F(5), CTRL, &m), "\x1b[15;5~");
        assert!(encode_key(&key(KeyCode::F(13), NONE), &m, false).is_none());
    }

    #[test]
    fn ctrl_keys_map_to_their_control_bytes() {
        let m = KeyModes::default();
        assert_eq!(enc(KeyCode::Char('c'), CTRL, &m), "\x03");
        assert_eq!(enc(KeyCode::Char(' '), CTRL, &m), "\x00");
        assert_eq!(enc(KeyCode::Char('/'), CTRL, &m), "\x1f");
        assert_eq!(enc(KeyCode::Char('['), CTRL, &m), "\x1b");
        // Ctrl+Alt+x is ESC then the control byte.
        assert_eq!(enc(KeyCode::Char('x'), CTRL | ALT, &m), "\x1b\x18");
    }

    #[test]
    fn ctrl_shift_letters_are_distinct_only_under_kitty() {
        let kitty = modes_after(b"\x1b[>1u");
        assert_eq!(enc(KeyCode::Char('A'), CTRL | SHIFT, &kitty), "\x1b[97;6u");
        // Plain Ctrl+letter stays the control byte even under kitty.
        assert_eq!(enc(KeyCode::Char('a'), CTRL, &kitty), "\x01");
        let legacy = KeyModes::default();
        assert_eq!(enc(KeyCode::Char('A'), CTRL | SHIFT, &legacy), "\x01");
    }

    #[test]
    fn alt_letters_and_plain_text_are_unchanged() {
        let m = modes_after(b"\x1b[>5u");
        assert_eq!(enc(KeyCode::Char('b'), ALT, &m), "\x1bb");
        assert_eq!(enc(KeyCode::Char('é'), NONE, &m), "é");
        assert_eq!(enc(KeyCode::Char('A'), SHIFT, &m), "A");
        assert_eq!(enc(KeyCode::BackTab, SHIFT, &m), "\x1b[Z");
        assert_eq!(enc(KeyCode::Esc, NONE, &m), "\x1b");
    }
}
