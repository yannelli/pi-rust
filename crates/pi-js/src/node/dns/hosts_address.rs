// Adapted from c-ares 1.34.9 inet_net_pton.c. See LICENSE-c-ares in the dns directory.

//! Hosts file addresses as c-ares 1.34 `ares_inet_pton` reads them. IPv4 accepts the old
//! network shorthand: `127.1` is 127.1.0.0 and `0x7f` is 127.0.0.0, with missing bytes zero.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Appends `byte`, or fails like `EMSGSIZE` when `out` is full.
fn push(out: &mut [u8; 4], length: &mut usize, byte: u8) -> Option<()> {
    *out.get_mut(*length)? = byte;
    *length += 1;
    Some(())
}

/// `ares_inet_net_pton_ipv4` without CIDR: hex nybbles after `0x`, or dotted decimal octets.
fn ipv4(text: &[u8]) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut length = 0;
    let hex = |byte: &u8| byte.is_ascii_hexdigit();
    if let [b'0', b'x' | b'X', first, ..] = text
        && hex(first)
    {
        let digits: Vec<u8> = text[2..].iter().take_while(|byte| hex(byte)).map(|byte| hex_value(*byte)).collect();
        if digits.len() + 2 != text.len() {
            return None;
        }
        for pair in digits.chunks(2) {
            let low = pair.get(1).copied().unwrap_or(0);
            push(&mut out, &mut length, pair[0] << 4 | low)?;
        }
    } else if text.first().is_some_and(u8::is_ascii_digit) {
        for octet in text.split(|byte| *byte == b'.') {
            if octet.is_empty() || !octet.iter().all(u8::is_ascii_digit) {
                return None;
            }
            let value = octet.iter().try_fold(0u32, |value, digit| {
                let value = value * 10 + u32::from(digit - b'0');
                (value <= 255).then_some(value)
            })?;
            push(&mut out, &mut length, u8::try_from(value).ok()?)?;
        }
    } else {
        return None;
    }
    Some(out)
}

fn hex_value(byte: u8) -> u8 {
    char::from(byte).to_digit(16).and_then(|digit| u8::try_from(digit).ok()).unwrap_or(0)
}

/// `ares_inet_pton6` agrees with the standard parser once a dotted IPv4 suffix is rewritten
/// from the shorthand above; a dotted part anywhere else fails both.
fn ipv6(text: &str) -> Option<Ipv6Addr> {
    match text.rsplit_once(':') {
        Some((head, tail)) if tail.contains('.') => {
            let [a, b, c, d] = ipv4(tail.as_bytes())?;
            format!("{head}:{a}.{b}.{c}.{d}").parse().ok()
        }
        _ => text.parse().ok(),
    }
}

/// `ares_normalize_ipaddr`: IPv4 first, then IPv6.
pub(super) fn parse(text: &str) -> Option<IpAddr> {
    ipv4(text.as_bytes()).map(|ip| Ipv4Addr::from(ip).into()).or_else(|| ipv6(text).map(IpAddr::V6))
}

#[cfg(test)]
mod tests {
    use super::parse;

    /// Expected values come from the pinned c-ares 1.34.9 `inet_net_pton.c` built as a fixture.
    #[test]
    fn matches_c_ares_inet_pton() {
        let cases = [
            ("127.0.0.1", Some("127.0.0.1")),
            ("127.1", Some("127.1.0.0")),
            ("10", Some("10.0.0.0")),
            ("192.168.1", Some("192.168.1.0")),
            ("224", Some("224.0.0.0")),
            ("010.0.0.1", Some("10.0.0.1")),
            ("0x7f000001", Some("127.0.0.1")),
            ("0X7F", Some("127.0.0.0")),
            ("0x7f00001", Some("127.0.0.16")),
            ("0x", None),
            ("0x7f0000011", None),
            ("1.2.3.4.5", None),
            ("256.1.1.1", None),
            ("1..2", None),
            ("1.2.", None),
            ("127.0.0.1abc", None),
            ("127.0.0.1/8", None),
            ("0x7f.1", None),
            ("::1", Some("::1")),
            ("::ffff:1.2", Some("::ffff:1.2.0.0")),
            ("1:2:3:4:5:6:7::", Some("1:2:3:4:5:6:7:0")),
            ("::1:2:3:4:5:6:7", Some("0:1:2:3:4:5:6:7")),
            ("1:2:3:4:5:6:1.2.3.4", Some("1:2:3:4:5:6:102:304")),
            ("1::2:", None),
            (":1::", None),
            ("1:2:3:4:5:6:7:8::", None),
            ("1:2:3:4:5:6:7:8:9", None),
            ("12345::", None),
            ("fe80::1%lo0", None),
            ("::1.2.3.4.5", None),
            ("", None),
            ("1:2:3:4:5:6:7::8", None),
            ("1:2:3:4:5:6::1.2.3.4", None),
            ("::1.2", Some("::1.2.0.0")),
            ("::ffff:0x7f.1", None),
            ("::ffff:0x7f000001", None),
            ("::0001", Some("::1")),
            ("::00001", None),
            ("1::2::3", None),
            ("::", Some("::")),
            ("0:0:0:0:0:0:0:0", Some("::")),
            ("::1.2.3", Some("::1.2.3.0")),
            ("a:b:c:d:e:f:1.2.3.4", Some("a:b:c:d:e:f:102:304")),
            ("a:b:c:d:e:f:1.2", Some("a:b:c:d:e:f:102:0")),
            ("A::B", Some("a::b")),
            ("::ffff:256.1.1.1", None),
            ("::ffff:01.2.3.4", Some("::ffff:1.2.3.4")),
            ("1::1.2.3.4", Some("1::102:304")),
            ("::1:1.2.3.4.", None),
            ("::1.2.3.4:5", None),
            ("::ffff:10", Some("::255.255.0.16")),
            ("1:2:3:4:5:6:7:1.2", None),
            (":::1", None),
            ("1:::2", None),
            ("::ffff:1.2.3.4/96", None),
            ("::ffff:a1.2.3.4", None),
            ("fffff::1", None),
            ("1:2:3:4:5:6:7:8:", None),
            ("::FFFF:224", Some("::255.255.2.36")),
        ];
        for (text, expected) in cases {
            assert_eq!(parse(text), expected.map(|ip| ip.parse().unwrap()), "{text}");
        }
    }
}
