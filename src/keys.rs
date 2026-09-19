//! CDP の Input.dispatchKeyEvent に渡すキー定義。

pub struct KeyDef {
    pub key: String,
    pub code: String,
    pub vk: u32,
    pub text: String,
}

fn def(key: &str, code: &str, vk: u32, text: &str) -> KeyDef {
    KeyDef { key: key.into(), code: code.into(), vk, text: text.into() }
}

/// 名前付きキー(Playwright の keyboard.press と同じ名前)。
pub fn named(name: &str) -> Option<KeyDef> {
    Some(match name {
        "Enter" => def("Enter", "Enter", 13, "\r"),
        "Space" => def(" ", "Space", 32, " "),
        "Backspace" => def("Backspace", "Backspace", 8, ""),
        "Tab" => def("Tab", "Tab", 9, ""),
        "Escape" => def("Escape", "Escape", 27, ""),
        "ArrowUp" => def("ArrowUp", "ArrowUp", 38, ""),
        "ArrowDown" => def("ArrowDown", "ArrowDown", 40, ""),
        "ArrowLeft" => def("ArrowLeft", "ArrowLeft", 37, ""),
        "ArrowRight" => def("ArrowRight", "ArrowRight", 39, ""),
        _ => return None,
    })
}

/// US 配列で打てる ASCII 文字のキー定義。それ以外は None(テキスト挿入で送る)。
pub fn for_char(c: char) -> Option<KeyDef> {
    let s = c.to_string();
    let (code, vk) = match c {
        'a'..='z' => (format!("Key{}", c.to_ascii_uppercase()), c.to_ascii_uppercase() as u32),
        'A'..='Z' => (format!("Key{c}"), c as u32),
        '0'..='9' => (format!("Digit{c}"), c as u32),
        ' ' => ("Space".into(), 32),
        '\t' => return Some(def("Tab", "Tab", 9, "")),
        '-' => ("Minus".into(), 189),
        '=' => ("Equal".into(), 187),
        '[' => ("BracketLeft".into(), 219),
        ']' => ("BracketRight".into(), 221),
        '\\' => ("Backslash".into(), 220),
        ';' => ("Semicolon".into(), 186),
        '\'' => ("Quote".into(), 222),
        ',' => ("Comma".into(), 188),
        '.' => ("Period".into(), 190),
        '/' => ("Slash".into(), 191),
        '`' => ("Backquote".into(), 192),
        '!'..='~' => (String::new(), 0),
        _ => return None,
    };
    Some(KeyDef { key: s.clone(), code, vk, text: s })
}
