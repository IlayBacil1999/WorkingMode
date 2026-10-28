//! Small Win32 helpers: UTF-16 strings and GUID text.

use windows_sys::core::GUID;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Copies `s` into a fixed UTF-16 buffer, truncating and null-terminating.
pub fn fill(dst: &mut [u16], s: &str) {
    let n = dst.len() - 1;
    let mut i = 0;
    for c in s.encode_utf16().take(n) {
        dst[i] = c;
        i += 1;
    }
    dst[i] = 0;
}

/// windows-sys GUIDs don't implement PartialEq.
pub fn guid_eq(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

pub fn opt_guid_eq(a: Option<GUID>, b: Option<GUID>) -> bool {
    matches!((a, b), (Some(x), Some(y)) if guid_eq(&x, &y))
}

pub fn guid_to_string(g: &GUID) -> String {
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        g.data1, g.data2, g.data3, g.data4[0], g.data4[1], g.data4[2], g.data4[3], g.data4[4], g.data4[5], g.data4[6], g.data4[7]
    )
}

pub fn guid_from_str(s: &str) -> Option<GUID> {
    let hex: String = s.trim_matches(|c| c == '{' || c == '}').chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return None;
    }
    u128::from_str_radix(&hex, 16).ok().map(GUID::from_u128)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_round_trip() {
        let s = "381b4222-f694-41f0-9685-ff5bb260df2e";
        assert_eq!(guid_to_string(&guid_from_str(s).unwrap()), s);
        assert!(guid_from_str("nope").is_none());
    }

    #[test]
    fn fill_truncates() {
        let mut b = [1u16; 4];
        fill(&mut b, "abcdef");
        assert_eq!(from_wide(&b), "abc");
    }
}
