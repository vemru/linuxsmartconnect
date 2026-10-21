//! Keyboard mapping: X keysym names → PC/AT set-1 scancodes (svc 16 cmd 3) or text (cmd 7).

pub const E0: i64 = 0x100;
pub const FLAG_NATIVE: i64 = 0x1000;
pub const KEYCODE_HOME: i64 = 3;
pub const KEYCODE_BACK: i64 = 4;
pub const KEYCODE_APP_SWITCH: i64 = 187;

pub fn is_modifier(c: (i64, i64)) -> bool {
    matches!(c, (0x1D | 0x2A | 0x36 | 0x38, 0) | (0x1D | 0x38 | 0x5B | 0x5C, E0))
}

/// X keysym name → PC/AT set-1 scancode and flags (0x100 = E0-extended), US layout.
/// Shifted keysyms ("exclam", "A") map to their base key: Shift is sent as its own key.
pub fn scancode(key: &str) -> Option<(i64, i64)> {
    if let [c] = key.as_bytes() {
        let c = c.to_ascii_lowercase();
        const LETTERS: &[u8] = b"qwertyuiop\0\0\0\0asdfghjkl\0\0\0\0\0zxcvbnm";
        if c.is_ascii_lowercase() {
            let i = LETTERS.iter().position(|&l| l == c)?;
            return Some((0x10 + i as i64, 0));
        }
        if c.is_ascii_digit() {
            return Some((if c == b'0' { 0x0B } else { 0x02 + (c - b'1') as i64 }, 0));
        }
    }
    let (code, flags) = match key {
        "Escape" => (0x01, 0),
        "exclam" => (0x02, 0),
        "at" => (0x03, 0),
        "numbersign" => (0x04, 0),
        "dollar" => (0x05, 0),
        "percent" => (0x06, 0),
        "asciicircum" => (0x07, 0),
        "ampersand" => (0x08, 0),
        "asterisk" => (0x09, 0),
        "parenleft" => (0x0A, 0),
        "parenright" => (0x0B, 0),
        "minus" | "underscore" => (0x0C, 0),
        "equal" | "plus" => (0x0D, 0),
        "BackSpace" => (0x0E, 0),
        "Tab" | "ISO_Left_Tab" => (0x0F, 0),
        "bracketleft" | "braceleft" => (0x1A, 0),
        "bracketright" | "braceright" => (0x1B, 0),
        "Return" => (0x1C, 0),
        "Control_L" => (0x1D, 0),
        "semicolon" | "colon" => (0x27, 0),
        "apostrophe" | "quotedbl" => (0x28, 0),
        "grave" | "asciitilde" => (0x29, 0),
        "Shift_L" => (0x2A, 0),
        "backslash" | "bar" => (0x2B, 0),
        "comma" | "less" => (0x33, 0),
        "period" | "greater" => (0x34, 0),
        "slash" | "question" => (0x35, 0),
        "Shift_R" => (0x36, 0),
        "KP_Multiply" => (0x37, 0),
        "Alt_L" | "Meta_L" => (0x38, 0),
        "space" => (0x39, 0),
        "Caps_Lock" => (0x3A, 0),
        "F1" => (0x3B, 0),
        "F2" => (0x3C, 0),
        "F3" => (0x3D, 0),
        "F4" => (0x3E, 0),
        "F5" => (0x3F, 0),
        "F6" => (0x40, 0),
        "F7" => (0x41, 0),
        "F8" => (0x42, 0),
        "F9" => (0x43, 0),
        "F10" => (0x44, 0),
        "Num_Lock" => (0x45, 0),
        "Scroll_Lock" => (0x46, 0),
        "KP_7" | "KP_Home" => (0x47, 0),
        "KP_8" | "KP_Up" => (0x48, 0),
        "KP_9" | "KP_Prior" | "KP_Page_Up" => (0x49, 0),
        "KP_Subtract" => (0x4A, 0),
        "KP_4" | "KP_Left" => (0x4B, 0),
        "KP_5" | "KP_Begin" => (0x4C, 0),
        "KP_6" | "KP_Right" => (0x4D, 0),
        "KP_Add" => (0x4E, 0),
        "KP_1" | "KP_End" => (0x4F, 0),
        "KP_2" | "KP_Down" => (0x50, 0),
        "KP_3" | "KP_Next" | "KP_Page_Down" => (0x51, 0),
        "KP_0" | "KP_Insert" => (0x52, 0),
        "KP_Decimal" | "KP_Delete" | "KP_Separator" => (0x53, 0),
        "F11" => (0x57, 0),
        "F12" => (0x58, 0),
        "KP_Enter" => (0x1C, E0),
        "Control_R" => (0x1D, E0),
        "KP_Divide" => (0x35, E0),
        "Print" | "Sys_Req" => (0x37, E0),
        "Alt_R" | "ISO_Level3_Shift" | "Meta_R" => (0x38, E0),
        "Home" => (0x47, E0),
        "Up" => (0x48, E0),
        "Page_Up" | "Prior" => (0x49, E0),
        "Left" => (0x4B, E0),
        "Right" => (0x4D, E0),
        "End" => (0x4F, E0),
        "Down" => (0x50, E0),
        "Page_Down" | "Next" => (0x51, E0),
        "Insert" => (0x52, E0),
        "Delete" => (0x53, E0),
        "Super_L" => (0x5B, E0),
        "Super_R" => (0x5C, E0),
        "Menu" => (0x5D, E0),
        _ => return None,
    };
    Some((code, flags))
}

/// Printable keys arrive as keysym names ("a", "A", "space", "comma"…); send them as text.
pub fn key_text(key: &str) -> Option<String> {
    let named = match key {
        "space" => " ",
        "comma" => ",",
        "period" => ".",
        "minus" => "-",
        "plus" => "+",
        "equal" => "=",
        "slash" => "/",
        "backslash" => "\\",
        "semicolon" => ";",
        "colon" => ":",
        "apostrophe" => "'",
        "quotedbl" => "\"",
        "exclam" => "!",
        "question" => "?",
        "at" => "@",
        "numbersign" => "#",
        "dollar" => "$",
        "percent" => "%",
        "ampersand" => "&",
        "asterisk" => "*",
        "parenleft" => "(",
        "parenright" => ")",
        "underscore" => "_",
        _ => "",
    };
    if !named.is_empty() {
        return Some(named.to_owned());
    }
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if !c.is_control() => Some(c.to_string()),
        _ => None,
    }
}
