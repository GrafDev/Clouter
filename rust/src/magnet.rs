//! Разбор и проверка magnet-ссылок.
use anyhow::{bail, Result};

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Magnet {
    pub info_hash: String,
    pub name: Option<String>,
    pub trackers: Vec<String>,
}

fn unpct(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => { out.push(b' '); i += 1; }
            b'%' => {
                match s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => { out.push(v); i += 3; }
                    None => { out.push(b'%'); i += 1; }
                }
            }
            c => { out.push(c); i += 1; }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Принимает `magnet:?xt=urn:btih:<40 hex | 32 base32>` (в т.ч. с пробелами/переводами строк вокруг).
pub fn parse(input: &str) -> Result<Magnet> {
    let s = input.trim();
    let Some(q) = s.strip_prefix("magnet:?").or_else(|| s.strip_prefix("MAGNET:?")) else {
        bail!("E_MAGNET_PREFIX");
    };
    let (mut hash, mut name, mut trackers) = (None, None, vec![]);
    for kv in q.split('&') {
        let Some((k, v)) = kv.split_once('=') else { continue };
        let v = unpct(v);
        match k {
            "xt" => if let Some(h) = v.strip_prefix("urn:btih:") { hash = Some(h.to_string()); },
            "dn" => name = Some(v),
            "tr" => trackers.push(v),
            _ => {}
        }
    }
    let Some(h) = hash else { bail!("E_MAGNET_NOHASH") };
    let ok_hex = h.len() == 40 && h.chars().all(|c| c.is_ascii_hexdigit());
    let ok_b32 = h.len() == 32 && h.chars().all(|c| matches!(c.to_ascii_uppercase(), 'A'..='Z' | '2'..='7'));
    if !(ok_hex || ok_b32) { bail!("E_MAGNET_BADHASH"); }
    Ok(Magnet { info_hash: h.to_lowercase(), name, trackers })
}

/// Находит magnet-ссылку в произвольном тексте (из буфера обмена).
pub fn find_in_text(text: &str) -> Option<String> {
    let i = text.find("magnet:?")?;
    let rest = &text[i..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ok_hex() {
        let m = parse("magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10&dn=Sintel%20X&tr=udp%3A%2F%2Fa.b%3A1").unwrap();
        assert_eq!(m.name.as_deref(), Some("Sintel X"));
        assert_eq!(m.trackers, vec!["udp://a.b:1"]);
    }
    #[test]
    fn ok_b32() { assert!(parse("magnet:?xt=urn:btih:ABCDEFGHIJKLMNOPQRSTUVWXYZ234567").is_ok()); }
    #[test]
    fn bad() {
        assert!(parse("http://x").is_err());
        assert!(parse("magnet:?dn=x").is_err());
        assert!(parse("magnet:?xt=urn:btih:zzz").is_err());
    }
    #[test]
    fn find() { assert_eq!(find_in_text("скачай magnet:?xt=urn:btih:aa тут").unwrap(), "magnet:?xt=urn:btih:aa"); }
}
