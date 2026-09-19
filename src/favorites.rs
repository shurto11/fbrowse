//! お気に入り(名前 → URL、並び順つき)と、入力文字列から URL への変換。

use indexmap::IndexMap;
use std::path::PathBuf;

pub type Favorites = IndexMap<String, String>;

pub fn path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".config"));
    base.join("fbrowse/favorites.json")
}

pub fn load() -> Favorites {
    match std::fs::read_to_string(path()) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => IndexMap::from([("yahoo".to_string(), "https://www.yahoo.co.jp".to_string())]),
    }
}

pub fn save(f: &Favorites) {
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&p, serde_json::to_string_pretty(f).unwrap_or_default());
}

/// "example.com/path" のようにドメインらしく見えるか。
fn looks_like_domain(s: &str) -> bool {
    if s.chars().any(char::is_whitespace) {
        return false;
    }
    let host = s.split('/').next().unwrap_or("");
    match host.rsplit_once('.') {
        Some((head, tld)) => !head.is_empty() && tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()),
        None => false,
    }
}

/// encodeURIComponent 相当。
fn encode_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// お気に入り名・URL・ドメイン・検索語のいずれかを URL にする。空なら None。
pub fn resolve(input: &str, favs: &Favorites) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    if let Some(url) = favs.get(&input.to_lowercase()) {
        return Some(url.clone());
    }
    let has_scheme = ["http://", "https://", "file://", "about:", "data:", "chrome://", "view-source:"]
        .iter()
        .any(|p| input.starts_with(p));
    if has_scheme {
        return Some(input.to_string());
    }
    if input.starts_with("www.") || looks_like_domain(input) {
        return Some(format!("https://{input}"));
    }
    Some(format!("https://duckduckgo.com/?q={}", encode_component(input)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_inputs() {
        let favs: Favorites = IndexMap::from([("yahoo".to_string(), "https://www.yahoo.co.jp".to_string())]);
        assert_eq!(resolve("Yahoo", &favs).unwrap(), "https://www.yahoo.co.jp");
        assert_eq!(resolve("https://a.b/C", &favs).unwrap(), "https://a.b/C");
        assert_eq!(resolve("file:///tmp/x.html", &favs).unwrap(), "file:///tmp/x.html");
        assert_eq!(resolve("example.com/Path", &favs).unwrap(), "https://example.com/Path");
        assert_eq!(resolve("www.foo", &favs).unwrap(), "https://www.foo");
        assert_eq!(resolve("rust 言語", &favs).unwrap(), "https://duckduckgo.com/?q=rust%20%E8%A8%80%E8%AA%9E");
        assert_eq!(resolve("v1.2", &favs).unwrap(), "https://duckduckgo.com/?q=v1.2");
        assert!(resolve("  ", &favs).is_none());
    }
}
