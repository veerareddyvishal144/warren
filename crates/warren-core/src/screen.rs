//! Screen access for instance types that have a display (`mac` today): the
//! actions a client sends to `POST /v1/instances/{id}/input`, the PNG that
//! `GET /v1/instances/{id}/screenshot` answers with, and the X11 keysyms a
//! VNC-driven runtime sends for text and key chords.
//!
//! Coordinates are framebuffer pixels with the origin at the top left. A
//! chord is `[modifier+]*key`: `cmd+space`, `shift+a`, `ctrl+alt+delete`,
//! `enter`, `f5`, `down`. Modifiers are `cmd` (also `command`, `super`,
//! `meta`, `win`), `ctrl` (`control`), `alt` (`option`, `opt`) and `shift`.
//! The key is a single character or one of the names in [`keysym_for_name`].

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Most actions in one `input` request.
pub const MAX_ACTIONS: usize = 100;
/// Longest `type` text, in characters (a VNC server takes a key down and a
/// key up per character with a pause between them, so this is seconds of
/// typing; files go through the files API).
pub const MAX_TEXT_CHARS: usize = 1024;
/// Most clicks in one `click` action (a triple click).
pub const MAX_CLICK_COUNT: u32 = 3;
/// Most wheel steps per axis in one `scroll` action.
pub const MAX_SCROLL_STEPS: i32 = 50;
/// Longest key chord, in characters (`ctrl+alt+shift+cmd+pagedown` fits).
pub const MAX_CHORD_CHARS: usize = 40;

/// A mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MouseButton {
    #[default]
    Left,
    Middle,
    Right,
}

impl MouseButton {
    /// The button's bit in an RFB `PointerEvent` mask.
    pub fn rfb_mask(self) -> u8 {
        match self {
            MouseButton::Left => 1,
            MouseButton::Middle => 2,
            MouseButton::Right => 4,
        }
    }
}

/// One action on an instance's screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum InputAction {
    /// Move the pointer to (x, y) and click `count` times with `button`.
    Click {
        x: u32,
        y: u32,
        #[serde(default)]
        button: MouseButton,
        #[serde(default = "one", skip_serializing_if = "is_one")]
        count: u32,
    },
    /// Move the pointer to (x, y) without clicking (hover).
    Move { x: u32, y: u32 },
    /// Turn the wheel at (x, y): `dy > 0` scrolls down, `dy < 0` up, `dx > 0`
    /// right, `dx < 0` left, one step per unit.
    Scroll {
        x: u32,
        y: u32,
        #[serde(default, skip_serializing_if = "is_zero")]
        dx: i32,
        #[serde(default, skip_serializing_if = "is_zero")]
        dy: i32,
    },
    /// Type text as key presses; `\n` is Return and `\t` is Tab.
    Type { text: String },
    /// Press and release a key chord such as `cmd+space` or `enter`.
    Key { key: String },
}

fn one() -> u32 {
    1
}

fn is_one(v: &u32) -> bool {
    *v == 1
}

fn is_zero(v: &i32) -> bool {
    *v == 0
}

impl InputAction {
    /// Refuse what no screen can do: too many clicks or wheel steps, empty or
    /// oversized text, or a chord that does not parse. Coordinates are not
    /// checked here; the runtime knows the screen size.
    pub fn validate(&self) -> Result<()> {
        match self {
            InputAction::Click { count, .. } => {
                if *count == 0 || *count > MAX_CLICK_COUNT {
                    return Err(Error::invalid(format!(
                        "click count must be between 1 and {MAX_CLICK_COUNT}"
                    )));
                }
            }
            InputAction::Move { .. } => {}
            InputAction::Scroll { dx, dy, .. } => {
                if dx.abs() > MAX_SCROLL_STEPS || dy.abs() > MAX_SCROLL_STEPS {
                    return Err(Error::invalid(format!(
                        "scroll dx and dy must be between -{MAX_SCROLL_STEPS} and {MAX_SCROLL_STEPS}"
                    )));
                }
                if *dx == 0 && *dy == 0 {
                    return Err(Error::invalid("scroll needs a non-zero dx or dy"));
                }
            }
            InputAction::Type { text } => {
                if text.is_empty() {
                    return Err(Error::invalid("type text must not be empty"));
                }
                if text.chars().count() > MAX_TEXT_CHARS {
                    return Err(Error::invalid(format!(
                        "type text is longer than {MAX_TEXT_CHARS} characters"
                    )));
                }
            }
            InputAction::Key { key } => {
                parse_chord(key)?;
            }
        }
        Ok(())
    }
}

/// Body of `POST /v1/instances/{id}/input`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct InputRequest {
    pub actions: Vec<InputAction>,
}

impl InputRequest {
    pub fn validate(&self) -> Result<()> {
        if self.actions.is_empty() {
            return Err(Error::invalid("actions must contain at least one action"));
        }
        if self.actions.len() > MAX_ACTIONS {
            return Err(Error::invalid(format!(
                "at most {MAX_ACTIONS} actions per request"
            )));
        }
        self.actions.iter().try_for_each(InputAction::validate)
    }
}

/// A capture of the screen, PNG encoded.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Screenshot {
    pub width: u32,
    pub height: u32,
    pub png: Vec<u8>,
}

/// `GET /v1/instances/{id}/screenshot?format=json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenshotJson {
    pub width: u32,
    pub height: u32,
    /// Always `png`.
    pub format: String,
    /// The image, standard base64 with padding.
    pub data: String,
}

/// Width and height from a PNG's IHDR chunk, `None` when the bytes are not
/// a PNG.
pub fn png_dimensions(png: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if png.len() < 24 || png[..8] != SIGNATURE || &png[12..16] != b"IHDR" {
        return None;
    }
    let be = |i: usize| u32::from_be_bytes([png[i], png[i + 1], png[i + 2], png[i + 3]]);
    Some((be(16), be(20)))
}

/// A modifier key of a chord.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Modifier {
    Shift,
    Control,
    /// Option on a Mac keyboard, Alt elsewhere.
    Option,
    /// Command on a Mac keyboard, Super or Windows elsewhere.
    Command,
}

impl Modifier {
    fn parse(word: &str) -> Option<Modifier> {
        Some(match word {
            "shift" => Modifier::Shift,
            "ctrl" | "control" => Modifier::Control,
            "alt" | "option" | "opt" => Modifier::Option,
            "cmd" | "command" | "super" | "meta" | "win" => Modifier::Command,
            _ => return None,
        })
    }
}

/// A parsed chord: the modifiers to hold, in the order given, and the key's
/// X11 keysym.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord {
    pub modifiers: Vec<Modifier>,
    pub keysym: u32,
    /// The key as written, for messages.
    pub key: String,
}

/// Parse `cmd+shift+a`, `enter`, `+` (the plus key itself) or `ctrl++`.
pub fn parse_chord(s: &str) -> Result<Chord> {
    let text = s.trim();
    if text.is_empty() {
        return Err(Error::invalid("key must not be empty"));
    }
    if text.chars().count() > MAX_CHORD_CHARS {
        return Err(Error::invalid(format!(
            "key is longer than {MAX_CHORD_CHARS} characters"
        )));
    }
    // The last `+` that is not the final character splits modifiers from the
    // key, so `ctrl++` means Control and the plus key.
    let (mods, key) =
        match text[..text.len() - text.chars().last().map_or(0, char::len_utf8)].rfind('+') {
            Some(i) => (&text[..i], &text[i + 1..]),
            None => ("", text),
        };
    let mut modifiers = Vec::new();
    for word in mods.split('+').filter(|w| !w.is_empty()) {
        let m = Modifier::parse(&word.to_ascii_lowercase())
            .ok_or_else(|| Error::invalid(format!("unknown modifier {word:?} in key {s:?}")))?;
        if !modifiers.contains(&m) {
            modifiers.push(m);
        }
    }
    if key.is_empty() {
        return Err(Error::invalid(format!("key {s:?} names no key")));
    }
    let keysym = match key.chars().count() {
        1 => {
            let (keysym, shifted) = key_press_for_char(key.chars().next().unwrap_or(' '));
            if shifted && !modifiers.contains(&Modifier::Shift) {
                modifiers.push(Modifier::Shift);
            }
            keysym
        }
        _ => keysym_for_name(&key.to_ascii_lowercase()).ok_or_else(|| {
            Error::invalid(format!(
                "unknown key {key:?}; use a single character or a name such as enter, tab, escape, space, backspace, delete, up, down, left, right, home, end, pageup, pagedown, f1 to f12"
            ))
        })?,
    };
    Ok(Chord {
        modifiers,
        keysym,
        key: key.to_string(),
    })
}

/// X11 keysym names the chord parser accepts (lower case), after
/// `<X11/keysymdef.h>`.
pub fn keysym_for_name(name: &str) -> Option<u32> {
    Some(match name {
        "enter" | "return" => 0xff0d,
        "tab" => 0xff09,
        "space" | "spacebar" => 0x20,
        "esc" | "escape" => 0xff1b,
        "backspace" => 0xff08,
        "delete" | "del" => 0xffff,
        "insert" => 0xff63,
        "home" => 0xff50,
        "end" => 0xff57,
        "pageup" | "pgup" => 0xff55,
        "pagedown" | "pgdn" | "pgdown" => 0xff56,
        "left" => 0xff51,
        "up" => 0xff52,
        "right" => 0xff53,
        "down" => 0xff54,
        "capslock" => 0xffe5,
        "plus" => 0x2b,
        "minus" => 0x2d,
        "comma" => 0x2c,
        "period" | "dot" => 0x2e,
        "slash" => 0x2f,
        "backslash" => 0x5c,
        _ => {
            let n: u32 = name.strip_prefix('f')?.parse().ok()?;
            if (1..=24).contains(&n) {
                0xffbe + n - 1
            } else {
                return None;
            }
        }
    })
}

/// The key press that produces `c` on a US keyboard: the keysym to send and
/// whether Shift must be held. Apple's VNC server (the one behind a Lume
/// VM) maps keysyms to key codes and does not shift by itself, so `A` is
/// Shift plus the `a` key and `!` is Shift plus the `1` key; everything
/// else is [`keysym_for_char`] without Shift.
pub fn key_press_for_char(c: char) -> (u32, bool) {
    if c.is_ascii_uppercase() {
        return (keysym_for_char(c.to_ascii_lowercase()), true);
    }
    let base = match c {
        '~' => '`',
        '!' => '1',
        '@' => '2',
        '#' => '3',
        '$' => '4',
        '%' => '5',
        '^' => '6',
        '&' => '7',
        '*' => '8',
        '(' => '9',
        ')' => '0',
        '_' => '-',
        '+' => '=',
        '{' => '[',
        '}' => ']',
        '|' => '\\',
        ':' => ';',
        '"' => '\'',
        '<' => ',',
        '>' => '.',
        '?' => '/',
        _ => return (keysym_for_char(c), false),
    };
    (keysym_for_char(base), true)
}

/// The X11 keysym for a typed character: Latin-1 keysyms equal the code
/// point, newline and tab become Return and Tab, anything else is the
/// Unicode keysym (`0x01000000 + code point`).
pub fn keysym_for_char(c: char) -> u32 {
    match c {
        '\n' | '\r' => 0xff0d,
        '\t' => 0xff09,
        '\u{8}' => 0xff08,
        '\u{1b}' => 0xff1b,
        '\u{7f}' => 0xffff,
        ' '..='~' | '\u{a0}'..='\u{ff}' => c as u32,
        _ => 0x0100_0000 + c as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_round_trip_and_default() {
        let json = r#"{"actions":[{"type":"click","x":10,"y":20},{"type":"click","x":1,"y":2,"button":"right","count":2},{"type":"move","x":5,"y":6},{"type":"scroll","x":1,"y":1,"dy":-3},{"type":"type","text":"hi\n"},{"type":"key","key":"cmd+space"}]}"#;
        let req: InputRequest = serde_json::from_str(json).unwrap();
        assert_eq!(
            req.actions[0],
            InputAction::Click {
                x: 10,
                y: 20,
                button: MouseButton::Left,
                count: 1
            }
        );
        assert_eq!(
            req.actions[1],
            InputAction::Click {
                x: 1,
                y: 2,
                button: MouseButton::Right,
                count: 2
            }
        );
        assert_eq!(
            req.actions[3],
            InputAction::Scroll {
                x: 1,
                y: 1,
                dx: 0,
                dy: -3
            }
        );
        req.validate().unwrap();
        let back: InputRequest =
            serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(back, req);
        // Serialization leaves the defaults out.
        let text = serde_json::to_string(&req.actions[0]).unwrap();
        assert_eq!(text, r#"{"type":"click","x":10,"y":20,"button":"left"}"#);
        assert!(serde_json::from_str::<InputAction>(r#"{"type":"tap","x":1,"y":2}"#).is_err());
        assert!(
            serde_json::from_str::<InputAction>(r#"{"type":"click","x":1,"y":2,"z":3}"#).is_err()
        );
    }

    #[test]
    fn validation_refuses_the_absurd() {
        let bad = [
            InputAction::Click {
                x: 0,
                y: 0,
                button: MouseButton::Left,
                count: 0,
            },
            InputAction::Click {
                x: 0,
                y: 0,
                button: MouseButton::Left,
                count: 4,
            },
            InputAction::Scroll {
                x: 0,
                y: 0,
                dx: 0,
                dy: 0,
            },
            InputAction::Scroll {
                x: 0,
                y: 0,
                dx: 0,
                dy: 51,
            },
            InputAction::Type {
                text: String::new(),
            },
            InputAction::Type {
                text: "x".repeat(MAX_TEXT_CHARS + 1),
            },
            InputAction::Key {
                key: "hyper+a".into(),
            },
            InputAction::Key { key: "cmd+".into() },
            InputAction::Key {
                key: "nosuchkey".into(),
            },
        ];
        for a in bad {
            assert!(a.validate().is_err(), "{a:?} should be invalid");
        }
        assert!(InputRequest::default().validate().is_err());
        let many = InputRequest {
            actions: vec![InputAction::Move { x: 1, y: 1 }; MAX_ACTIONS + 1],
        };
        assert!(many.validate().is_err());
    }

    #[test]
    fn chords_parse() {
        let c = parse_chord("cmd+space").unwrap();
        assert_eq!(c.modifiers, vec![Modifier::Command]);
        assert_eq!(c.keysym, 0x20);
        let c = parse_chord("Ctrl+Alt+Delete").unwrap();
        assert_eq!(c.modifiers, vec![Modifier::Control, Modifier::Option]);
        assert_eq!(c.keysym, 0xffff);
        // Uppercase and shifted symbols are Shift plus the base key, once.
        let c = parse_chord("shift+A").unwrap();
        assert_eq!(c.modifiers, vec![Modifier::Shift]);
        assert_eq!(c.keysym, 0x61);
        let c = parse_chord("cmd+$").unwrap();
        assert_eq!(c.modifiers, vec![Modifier::Command, Modifier::Shift]);
        assert_eq!(c.keysym, 0x34);
        assert_eq!(parse_chord("enter").unwrap().keysym, 0xff0d);
        assert_eq!(parse_chord("F5").unwrap().keysym, 0xffc2);
        assert_eq!(parse_chord("down").unwrap().keysym, 0xff54);
        // The plus key itself (Shift plus `=`), alone and with a modifier.
        let c = parse_chord("+").unwrap();
        assert_eq!(
            (c.modifiers.as_slice(), c.keysym),
            (&[Modifier::Shift][..], 0x3d)
        );
        let c = parse_chord("ctrl++").unwrap();
        assert_eq!(
            (c.modifiers.as_slice(), c.keysym),
            (&[Modifier::Control, Modifier::Shift][..], 0x3d)
        );
        assert_eq!(parse_chord("ctrl+-").unwrap().keysym, 0x2d);
        // Repeated modifiers collapse; unknown ones fail.
        assert_eq!(
            parse_chord("cmd+command+q").unwrap().modifiers,
            vec![Modifier::Command]
        );
        assert!(parse_chord("").is_err());
        assert!(parse_chord("fn+a").is_err());
        assert!(parse_chord("f25").is_err());
    }

    #[test]
    fn key_presses_shift_what_needs_it() {
        assert_eq!(key_press_for_char('a'), (0x61, false));
        assert_eq!(key_press_for_char('A'), (0x61, true));
        assert_eq!(key_press_for_char('1'), (0x31, false));
        assert_eq!(key_press_for_char('!'), (0x31, true));
        assert_eq!(key_press_for_char('_'), (0x2d, true));
        assert_eq!(key_press_for_char('"'), (0x27, true));
        assert_eq!(key_press_for_char('|'), (0x5c, true));
        assert_eq!(key_press_for_char(' '), (0x20, false));
        assert_eq!(key_press_for_char('\n'), (0xff0d, false));
        assert_eq!(key_press_for_char('-'), (0x2d, false));
    }

    #[test]
    fn keysyms_for_characters() {
        assert_eq!(keysym_for_char('a'), 0x61);
        assert_eq!(keysym_for_char('A'), 0x41);
        assert_eq!(keysym_for_char(' '), 0x20);
        assert_eq!(keysym_for_char('\n'), 0xff0d);
        assert_eq!(keysym_for_char('\t'), 0xff09);
        assert_eq!(keysym_for_char('é'), 0xe9);
        assert_eq!(keysym_for_char('€'), 0x0100_20ac);
    }

    #[test]
    fn png_header_is_read() {
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&1024u32.to_be_bytes());
        png.extend_from_slice(&768u32.to_be_bytes());
        png.extend_from_slice(&[8, 2, 0, 0, 0]);
        assert_eq!(png_dimensions(&png), Some((1024, 768)));
        assert_eq!(png_dimensions(b"not a png"), None);
    }
}
