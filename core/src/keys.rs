//! Remote-control key codes (`KEY_*`) understood by Samsung Tizen TVs.

use std::fmt;
use std::str::FromStr;

/// How a key is sent on the remote channel (`Cmd` field of `SendRemoteKey`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum KeyAction {
    /// Press and release (the normal case).
    #[default]
    Click,
    /// Key down (follow with [`KeyAction::Release`]; used for long presses).
    Press,
    /// Key up.
    Release,
}

impl KeyAction {
    /// Wire value: `Click`, `Press` or `Release`.
    pub fn as_str(self) -> &'static str {
        match self {
            KeyAction::Click => "Click",
            KeyAction::Press => "Press",
            KeyAction::Release => "Release",
        }
    }
}

macro_rules! keys {
    ($( $(#[$m:meta])* $v:ident => $code:literal, $label:literal; )*) => {
        /// A remote key. Use [`Key::Raw`] for any code not listed here.
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub enum Key {
            $( $(#[$m])* $v, )*
            /// Any other key code, sent verbatim (e.g. `"KEY_PICTURE_SIZE"`).
            Raw(String),
        }

        impl Key {
            /// Every named key (excludes [`Key::Raw`]), in a sensible display order.
            pub const ALL: &'static [Key] = &[ $( Key::$v, )* ];

            /// The `KEY_*` code sent to the TV.
            pub fn code(&self) -> &str {
                match self {
                    $( Key::$v => $code, )*
                    Key::Raw(s) => s.as_str(),
                }
            }

            /// Short human label for buttons / help screens.
            pub fn label(&self) -> &str {
                match self {
                    $( Key::$v => $label, )*
                    Key::Raw(s) => s.as_str(),
                }
            }

            /// The enum variant name (`"VolumeUp"`), or the raw code for [`Key::Raw`].
            pub fn name(&self) -> &str {
                match self {
                    $( Key::$v => stringify!($v), )*
                    Key::Raw(s) => s.as_str(),
                }
            }
        }
    };
}

keys! {
    /// Power toggle (turns the TV off when on).
    Power => "KEY_POWER", "Power";
    /// Explicit power off.
    PowerOff => "KEY_POWEROFF", "Power off";
    /// D-pad up.
    Up => "KEY_UP", "Up";
    /// D-pad down.
    Down => "KEY_DOWN", "Down";
    /// D-pad left.
    Left => "KEY_LEFT", "Left";
    /// D-pad right.
    Right => "KEY_RIGHT", "Right";
    /// OK / select.
    Enter => "KEY_ENTER", "OK";
    /// Back.
    Return => "KEY_RETURN", "Back";
    /// Exit the current app/overlay.
    Exit => "KEY_EXIT", "Exit";
    /// Smart Hub home.
    Home => "KEY_HOME", "Home";
    /// Settings menu.
    Menu => "KEY_MENU", "Menu";
    /// Input source.
    Source => "KEY_SOURCE", "Source";
    /// Info banner.
    Info => "KEY_INFO", "Info";
    /// Programme guide.
    Guide => "KEY_GUIDE", "Guide";
    /// Tools / options.
    Tools => "KEY_TOOLS", "Tools";
    /// Volume up.
    VolumeUp => "KEY_VOLUP", "Vol +";
    /// Volume down.
    VolumeDown => "KEY_VOLDOWN", "Vol −";
    /// Mute toggle.
    Mute => "KEY_MUTE", "Mute";
    /// Channel up.
    ChannelUp => "KEY_CHUP", "Ch +";
    /// Channel down.
    ChannelDown => "KEY_CHDOWN", "Ch −";
    /// Channel list.
    ChannelList => "KEY_CH_LIST", "Ch list";
    /// Previous channel.
    PreviousChannel => "KEY_PRECH", "Prev ch";
    /// Digit 0.
    Num0 => "KEY_0", "0";
    /// Digit 1.
    Num1 => "KEY_1", "1";
    /// Digit 2.
    Num2 => "KEY_2", "2";
    /// Digit 3.
    Num3 => "KEY_3", "3";
    /// Digit 4.
    Num4 => "KEY_4", "4";
    /// Digit 5.
    Num5 => "KEY_5", "5";
    /// Digit 6.
    Num6 => "KEY_6", "6";
    /// Digit 7.
    Num7 => "KEY_7", "7";
    /// Digit 8.
    Num8 => "KEY_8", "8";
    /// Digit 9.
    Num9 => "KEY_9", "9";
    /// Red colour key (A).
    Red => "KEY_RED", "Red";
    /// Green colour key (B).
    Green => "KEY_GREEN", "Green";
    /// Yellow colour key (C).
    Yellow => "KEY_YELLOW", "Yellow";
    /// Blue colour key (D; Samsung calls it CYAN).
    Blue => "KEY_CYAN", "Blue";
    /// Media play.
    Play => "KEY_PLAY", "Play";
    /// Media pause.
    Pause => "KEY_PAUSE", "Pause";
    /// Media stop.
    Stop => "KEY_STOP", "Stop";
    /// Fast forward.
    FastForward => "KEY_FF", "FF";
    /// Rewind.
    Rewind => "KEY_REWIND", "Rew";
}

impl Key {
    /// Key for a decimal digit (0-9).
    pub fn digit(d: u8) -> Option<Key> {
        Some(match d {
            0 => Key::Num0,
            1 => Key::Num1,
            2 => Key::Num2,
            3 => Key::Num3,
            4 => Key::Num4,
            5 => Key::Num5,
            6 => Key::Num6,
            7 => Key::Num7,
            8 => Key::Num8,
            9 => Key::Num9,
            _ => return None,
        })
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl FromStr for Key {
    type Err = crate::Error;

    /// Parses a key name, case-insensitively, trying in order:
    ///
    /// 1. the `KEY_*` code (`KEY_VOLUP`),
    /// 2. the bare code without the prefix (`volup`, `cyan`),
    /// 3. the variant name (`VolumeUp`, `num7`; `_`, `-` and spaces are ignored, so
    ///    `volume_up` works too),
    /// 4. the button label (`Vol +`, `OK`, `Back`, `Ch list`; `-` matches `−`).
    ///
    /// An unknown name that starts with `KEY_` becomes [`Key::Raw`] (uppercased), so any
    /// code can still be sent. Anything else unknown is an [`crate::Error::Parse`].
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let t = s.trim();
        if t.is_empty() {
            return Err(crate::Error::parse("empty key name"));
        }
        let up = t.to_ascii_uppercase();
        let code = if up.starts_with("KEY_") { up.clone() } else { format!("KEY_{up}") };
        let compact = |x: &str| -> String {
            x.chars()
                .filter(|c| !matches!(c, '_' | '-' | ' '))
                .flat_map(char::to_lowercase)
                .collect()
        };
        let label_norm = |x: &str| x.replace('−', "-").to_lowercase();
        let (ct, lt) = (compact(t), label_norm(t));
        let found = Key::ALL
            .iter()
            .find(|k| k.code() == code)
            .or_else(|| Key::ALL.iter().find(|k| compact(k.name()) == ct))
            .or_else(|| Key::ALL.iter().find(|k| label_norm(k.label()) == lt));
        if let Some(k) = found {
            return Ok(k.clone());
        }
        let raw_ok = up.len() > 4
            && up.starts_with("KEY_")
            && up.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if raw_ok {
            Ok(Key::Raw(up))
        } else {
            Err(crate::Error::Parse(format!(
                "unknown key '{t}' (use a KEY_* code to send any other key)"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_keys() {
        assert_eq!("KEY_VOLUP".parse::<Key>().unwrap(), Key::VolumeUp);
        assert_eq!("volup".parse::<Key>().unwrap(), Key::VolumeUp);
        assert_eq!("VOLUP".parse::<Key>().unwrap(), Key::VolumeUp);
        assert_eq!("cyan".parse::<Key>().unwrap(), Key::Blue);
        assert_eq!(
            "key_picture_size".parse::<Key>().unwrap(),
            Key::Raw("KEY_PICTURE_SIZE".into())
        );
        assert!("picture_size".parse::<Key>().is_err());
        assert!("nonsense".parse::<Key>().is_err());
        assert!("".parse::<Key>().is_err());
        assert!("KEY_".parse::<Key>().is_err());
        assert!("KEY_A B".parse::<Key>().is_err());
        assert_eq!(Key::digit(7), Some(Key::Num7));
        assert_eq!(Key::VolumeUp.name(), "VolumeUp");
        assert_eq!(KeyAction::Press.as_str(), "Press");
    }

    #[test]
    fn parse_key_names_and_labels() {
        assert_eq!("VolumeUp".parse::<Key>().unwrap(), Key::VolumeUp);
        assert_eq!("volumeup".parse::<Key>().unwrap(), Key::VolumeUp);
        assert_eq!("volume_up".parse::<Key>().unwrap(), Key::VolumeUp);
        assert_eq!("Num7".parse::<Key>().unwrap(), Key::Num7);
        assert_eq!("blue".parse::<Key>().unwrap(), Key::Blue);
        assert_eq!("KEY_CYAN".parse::<Key>().unwrap(), Key::Blue);
        assert_eq!(" key_enter ".parse::<Key>().unwrap(), Key::Enter);
        assert_eq!("Vol +".parse::<Key>().unwrap(), Key::VolumeUp);
        assert_eq!("vol -".parse::<Key>().unwrap(), Key::VolumeDown);
        assert_eq!("ok".parse::<Key>().unwrap(), Key::Enter);
        assert_eq!("Back".parse::<Key>().unwrap(), Key::Return);
        assert_eq!("ch list".parse::<Key>().unwrap(), Key::ChannelList);
        assert_eq!("7".parse::<Key>().unwrap(), Key::Num7);
        for k in Key::ALL {
            assert_eq!(&k.code().parse::<Key>().unwrap(), k);
            assert_eq!(&k.name().parse::<Key>().unwrap(), k);
            assert_eq!(&k.label().parse::<Key>().unwrap(), k, "label {}", k.label());
        }
    }
}
