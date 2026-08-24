//! The shape of the `static_ip=` seed key.
//!
//! Its own module for the same reason [`wifi`](crate::wifi) is: the value crosses
//! a trust boundary. It is typed at press time, travels as a line of `seed.txt`,
//! and is rendered by the device's first-boot hook into whichever line-based
//! format the image's network stack reads — a NetworkManager keyfile or a
//! `dhcpcd.conf` stanza. The check here holds the grammar those two renderings
//! share, so a bad value is a press-time error naming the flag, not a board that
//! silently never gets an address.
//!
//! The grammar is `address/prefix[,gateway[,dns...]]`, IPv4 throughout:
//! comma-separated fields, the first a dotted quad with a `/1`-`/32` prefix
//! length, then an optional gateway and any number of DNS servers. The check is
//! syntax, not policy — whether the gateway is inside the subnet or the DNS
//! servers answer is the network's business, and refusing a value the operator
//! deliberately chose would be wrong more often than it would help.
//!
//! Pure and host-independent; nothing here touches the filesystem or the network.

use std::net::Ipv4Addr;

/// Check a `static_ip=` value: `address/prefix[,gateway[,dns...]]`.
///
/// Every field must be IPv4 — the seed grammar does not carry IPv6, whose
/// addresses would need an escaping story for `:` against the renderings'
/// own separators; on every image both stacks keep IPv6 autoconfiguration on
/// regardless of this key.
///
/// # Errors
///
/// A terse clause naming the offending property, for the caller to wrap with
/// the flag or key the value was authored under.
pub fn check(value: &str) -> Result<(), &'static str> {
    if value.is_empty() {
        return Err("empty");
    }
    if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(
            "contains whitespace or a control character, which the grammar has no place for",
        );
    }
    let mut fields = value.split(',');
    let address = fields.next().expect("split yields at least one field");
    let Some((addr, prefix)) = address.split_once('/') else {
        return Err("the address has no /prefix (use address/prefix, e.g. 192.168.1.50/24)");
    };
    if addr.parse::<Ipv4Addr>().is_err() {
        return Err("the address is not an IPv4 dotted quad");
    }
    if !prefix.chars().all(|c| c.is_ascii_digit())
        || !prefix.parse::<u8>().is_ok_and(|p| (1..=32).contains(&p))
    {
        return Err("the prefix length is not 1-32");
    }
    for (i, field) in fields.enumerate() {
        if field.is_empty() {
            return Err("has an empty field (a doubled or trailing comma)");
        }
        if field.parse::<Ipv4Addr>().is_err() {
            return Err(if i == 0 {
                "the gateway is not an IPv4 dotted quad"
            } else {
                "a DNS server is not an IPv4 dotted quad"
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_ips_hold_the_grammar() {
        check("192.168.1.50/24").unwrap();
        check("10.0.0.5/32").unwrap();
        check("192.168.1.50/24,192.168.1.1").unwrap();
        check("192.168.1.50/24,192.168.1.1,1.1.1.1").unwrap();
        check("192.168.1.50/24,192.168.1.1,1.1.1.1,9.9.9.9").unwrap();

        assert!(check("").is_err());
        assert!(check("192.168.1.50").is_err(), "the prefix is not optional");
        assert!(check("192.168.1.50/").is_err());
        assert!(check("192.168.1.50/0").is_err(), "0 routes nothing");
        assert!(check("192.168.1.50/33").is_err());
        assert!(check("192.168.1.50/+24").is_err(), "u8's sign is not ours");
        assert!(check("192.168.256.1/24").is_err());
        assert!(check("fe80::1/64").is_err(), "IPv6 is not carried");
        assert!(check("192.168.1.50/24,").is_err(), "trailing comma");
        assert!(check("192.168.1.50/24,,1.1.1.1").is_err(), "empty gateway");
        assert!(check("192.168.1.50/24,gateway").is_err());
        assert!(check("192.168.1.50/24,192.168.1.1,dns").is_err());
        assert!(check("192.168.1.50 /24").is_err(), "whitespace");
        assert!(
            check("192.168.1.50/24\n").is_err(),
            "a seed line cannot carry a newline"
        );
    }
}
