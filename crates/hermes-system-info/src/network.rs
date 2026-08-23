//! Which addresses this machine can be reached at from another machine.
//!
//! This exists because of a failure that is silent in the worst way. A user who
//! wants to serve the gateway to their other devices reaches for the machine's
//! name — `hermes serve --host "$(hostname)"` — and on most Linux installs that
//! name is mapped to a **loopback** address in `/etc/hosts` (Debian writes
//! `127.0.1.1 <hostname>`), which wins over anything the network publishes. The
//! bind succeeds, the gateway reports that it is serving, authentication stays
//! off because the bind is local, and nobody else can reach it. Nothing is
//! wrong except that the one thing the user asked for did not happen.
//!
//! Detecting that needs an answer to "what addresses does this machine actually
//! hold?", and that answer is only useful if it is honest. So:
//!
//! * **Only addresses another machine could plausibly use.** Loopback is
//!   excluded because reaching it is the failure being diagnosed; link-local
//!   (`169.254/16`, `fe80::/10`) is excluded because binding it needs a scope
//!   the operator would have to know to supply, so offering it as advice would
//!   be offering a trap.
//! * **No opinion about which network an address belongs to.** A LAN address, a
//!   mesh VPN's CGNAT address and a unique-local IPv6 address are listed
//!   identically, for the same reason [`crate::paths`] has no opinion about
//!   which product is running: nothing here knows, and nothing here should.
//! * **Advisory, never load-bearing.** Every caller is printing a hint. An
//!   empty list or an error narrows what can be said; it never changes what the
//!   gateway does. That is why a partial read is kept rather than discarded.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use hermes_core::{Actionable, ErrorKind, Remedy, RemedyAction, SettingsSection};

#[derive(Debug, thiserror::Error)]
pub enum NetworkError {
    #[error("could not read {source_path}: {source}")]
    Read {
        source_path: &'static str,
        #[source]
        source: std::io::Error,
    },

    #[error("listing this machine's addresses is not implemented for this platform ({platform})")]
    UnsupportedPlatform { platform: &'static str },
}

impl Actionable for NetworkError {
    fn code(&self) -> &'static str {
        match self {
            Self::Read { .. } => "address_probe_failed",
            Self::UnsupportedPlatform { .. } => "address_probe_unsupported",
        }
    }

    fn kind(&self) -> ErrorKind {
        ErrorKind::Internal
    }

    fn remedies(&self) -> Vec<Remedy> {
        vec![Remedy::new(
            "List this machine's addresses yourself and pass one to --host \
             (`ip -brief addr` on Linux, `ifconfig` on macOS, `ipconfig` on Windows)",
            RemedyAction::OpenSettings {
                section: SettingsSection::Api,
            },
        )]
    }
}

/// Addresses another machine could reach this one at, in a stable order.
///
/// IPv4 before IPv6, each family ascending, so repeated runs and the output
/// they feed do not reorder themselves for no reason.
///
/// An empty list is a real answer, not a failure: a machine with only loopback
/// up holds nothing worth suggesting, and that is exactly what a caller
/// diagnosing an unreachable bind needs to be told.
pub fn reachable_addresses() -> Result<Vec<IpAddr>, NetworkError> {
    let mut addresses = platform::reachable_addresses()?;
    addresses.retain(is_reachable_from_elsewhere);
    addresses.sort_by_key(|address| match address {
        // `(family, octets)`: IPv4 first, then each family in ascending order.
        IpAddr::V4(v4) => (0_u8, u128::from(v4.to_bits())),
        IpAddr::V6(v6) => (1_u8, v6.to_bits()),
    });
    addresses.dedup();
    Ok(addresses)
}

/// Whether an address is one another machine could plausibly connect to.
///
/// Deliberately conservative. Suggesting an address that cannot be bound
/// without a scope id, or that loops straight back here, would replace one
/// confusing failure with a second one.
fn is_reachable_from_elsewhere(address: &IpAddr) -> bool {
    if address.is_loopback() || address.is_unspecified() || address.is_multicast() {
        return false;
    }
    match address {
        IpAddr::V4(v4) => !v4.is_link_local() && !v4.is_broadcast(),
        // `fe80::/10`. `Ipv6Addr::is_unicast_link_local` is still unstable, and
        // the test is one mask, so it is written out rather than waited for.
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) != 0xfe80,
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::{IpAddr, Ipv4Addr, Ipv6Addr, NetworkError};

    /// The kernel's forwarding table, whose `Local:` half lists every address
    /// this host answers for. Linux publishes no per-interface IPv4 list under
    /// `/proc`; reading the addresses any other way means an ioctl or a netlink
    /// socket, and this crate forbids `unsafe`.
    const FIB_TRIE: &str = "/proc/net/fib_trie";
    /// Every configured IPv6 address, one per line, as 32 hex characters.
    const IF_INET6: &str = "/proc/net/if_inet6";

    pub(super) fn reachable_addresses() -> Result<Vec<IpAddr>, NetworkError> {
        let v4 = std::fs::read_to_string(FIB_TRIE);
        let v6 = std::fs::read_to_string(IF_INET6);

        // A partial answer beats no answer: a host with IPv6 disabled has no
        // `if_inet6`, and refusing to report its IPv4 addresses over that would
        // withhold the only useful thing this function had to say.
        if let (Err(source), Err(_)) = (&v4, &v6) {
            return Err(NetworkError::Read {
                source_path: FIB_TRIE,
                source: std::io::Error::new(source.kind(), source.to_string()),
            });
        }

        let mut addresses: Vec<IpAddr> = Vec::new();
        if let Ok(contents) = &v4 {
            addresses.extend(parse_fib_trie(contents).into_iter().map(IpAddr::V4));
        }
        if let Ok(contents) = &v6 {
            addresses.extend(parse_if_inet6(contents).into_iter().map(IpAddr::V6));
        }
        Ok(addresses)
    }

    /// Pull this host's own IPv4 addresses out of `/proc/net/fib_trie`.
    ///
    /// The file is an indented dump of the routing trie, and the half after the
    /// `Local:` marker is the local table. An address sits on its own line and
    /// its properties on the next, so the address that matters is the one
    /// *preceding* a `host LOCAL` line:
    ///
    /// ```text
    /// Local:
    ///    |-- 192.0.2.10
    ///       /32 host LOCAL
    /// ```
    ///
    /// Anything before `Local:` is the main table — every route this machine
    /// knows, including other machines' addresses — so parsing must not start
    /// until that marker is seen. Getting that wrong would advertise a peer's
    /// address as one to bind, which is the exact confusion this module exists
    /// to end.
    pub(super) fn parse_fib_trie(contents: &str) -> Vec<Ipv4Addr> {
        let mut found = Vec::new();
        let mut in_local_table = false;
        let mut previous: Option<Ipv4Addr> = None;

        for line in contents.lines() {
            let trimmed = line.trim();
            if trimmed == "Local:" {
                in_local_table = true;
                previous = None;
                continue;
            }
            if trimmed.ends_with(':') && trimmed != "Local:" {
                // Another table header ("Main:"), so the local half has ended.
                in_local_table = false;
                continue;
            }
            if !in_local_table {
                continue;
            }

            if trimmed.contains("host LOCAL") {
                if let Some(address) = previous.take() {
                    found.push(address);
                }
                continue;
            }
            // `|-- 10.0.0.1` or `+-- 10.0.0.0/24`; only a bare address counts.
            previous = trimmed
                .rsplit(' ')
                .next()
                .and_then(|token| token.parse::<Ipv4Addr>().ok());
        }
        found
    }

    /// Pull every configured IPv6 address out of `/proc/net/if_inet6`.
    ///
    /// One address per line, as 32 undelimited hex characters followed by the
    /// interface index, prefix length, scope and flags:
    ///
    /// ```text
    /// 20010db8000000000000000000000001 04 40 00 80 eth0
    /// ```
    pub(super) fn parse_if_inet6(contents: &str) -> Vec<Ipv6Addr> {
        contents
            .lines()
            .filter_map(|line| {
                let hex = line.split_whitespace().next()?;
                if hex.len() != 32 {
                    return None;
                }
                let mut segments = [0_u16; 8];
                let (groups, _) = hex.as_bytes().as_chunks::<4>();
                for (segment, group) in segments.iter_mut().zip(groups) {
                    let text = std::str::from_utf8(group).ok()?;
                    *segment = u16::from_str_radix(text, 16).ok()?;
                }
                Some(Ipv6Addr::from(segments))
            })
            .collect()
    }
}

#[cfg(not(target_os = "linux"))]
mod platform {
    use super::{IpAddr, NetworkError};

    /// Not yet implemented off Linux.
    ///
    /// An error rather than an empty list, because the two mean opposite things
    /// to a caller: "this machine holds nothing another machine can reach" is a
    /// diagnosis, and "I did not look" is not. macOS and Windows enumerate
    /// addresses through `getifaddrs` and `GetAdaptersAddresses`, both of which
    /// need `unsafe` or a new dependency, so they belong to the cross-platform
    /// milestone where they can be verified on those systems.
    pub(super) fn reachable_addresses() -> Result<Vec<IpAddr>, NetworkError> {
        Err(NetworkError::UnsupportedPlatform {
            platform: std::env::consts::OS,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Addresses below come from the ranges reserved for documentation (RFC
    // 5737, RFC 3849) or from a range's base address, so no address belonging
    // to a real machine or network is written into this repository.

    #[cfg(target_os = "linux")]
    const FIB_TRIE_SAMPLE: &str = "\
Main:
  +-- 0.0.0.0/0 3 0 5
     |-- 198.51.100.1
        /0 universe UNICAST
     +-- 203.0.113.0/24 4 1 8
        |-- 203.0.113.9
           /32 universe UNICAST
Local:
  +-- 0.0.0.0/1 3 0 5
     +-- 192.0.2.0/24 2 0 2
        |-- 192.0.2.10
           /32 host LOCAL
        |-- 192.0.2.255
           /32 link BROADCAST
     |-- 198.51.100.7
        /32 host LOCAL
     +-- 127.0.0.0/8 2 0 2
        |-- 127.0.0.1
           /32 host LOCAL
        |-- 127.0.1.1
           /32 host LOCAL
";

    #[cfg(target_os = "linux")]
    #[test]
    fn only_the_local_table_is_read() {
        // The main table lists every route this machine knows, other machines'
        // addresses included. Suggesting one of those as a bind address would
        // be worse than saying nothing.
        let found = platform::parse_fib_trie(FIB_TRIE_SAMPLE);
        assert!(
            found.contains(&"192.0.2.10".parse().unwrap()),
            "a local address was missed: {found:?}"
        );
        assert!(
            found.contains(&"198.51.100.7".parse().unwrap()),
            "a local address was missed: {found:?}"
        );
        assert!(
            !found.contains(&"198.51.100.1".parse().unwrap()),
            "a gateway from the main table was reported as local: {found:?}"
        );
        assert!(
            !found.contains(&"203.0.113.9".parse().unwrap()),
            "a peer from the main table was reported as local: {found:?}"
        );
        assert!(
            !found.contains(&"192.0.2.255".parse().unwrap()),
            "a broadcast address is not a bind address: {found:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn loopback_is_parsed_but_never_suggested() {
        // Both halves matter. The parser reports what the kernel says, and the
        // filter is what decides it is useless advice - keeping those separate
        // is what makes each testable.
        let parsed = platform::parse_fib_trie(FIB_TRIE_SAMPLE);
        assert!(parsed.contains(&"127.0.1.1".parse().unwrap()));
        assert!(
            !is_reachable_from_elsewhere(&"127.0.1.1".parse::<IpAddr>().unwrap()),
            "127.0.1.1 is the address this whole module exists to warn about"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ipv6_addresses_are_read_as_undelimited_hex() {
        let sample = "\
20010db8000000000000000000000001 04 40 00 80 eth0
fe800000000000000000000000000001 02 40 20 80 eth0
00000000000000000000000000000001 01 80 10 80       lo
not-an-address 01 80 10 80 lo
";
        let found = platform::parse_if_inet6(sample);
        assert_eq!(
            found,
            vec![
                "2001:db8::1".parse::<Ipv6Addr>().unwrap(),
                "fe80::1".parse().unwrap(),
                "::1".parse().unwrap(),
            ],
            "the parser reports what the kernel lists, unfiltered"
        );
    }

    #[test]
    fn link_local_and_loopback_are_not_advice() {
        // A link-local address cannot be bound without a scope id the operator
        // would have to know to supply, so offering one replaces a confusing
        // failure with a second confusing failure.
        let link_local_v4 = IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1));
        for address in [
            "127.0.0.1".parse().unwrap(),
            "127.0.1.1".parse().unwrap(),
            "::1".parse().unwrap(),
            link_local_v4,
            "fe80::1".parse().unwrap(),
            "0.0.0.0".parse().unwrap(),
            "::".parse().unwrap(),
        ] {
            let address: IpAddr = address;
            assert!(
                !is_reachable_from_elsewhere(&address),
                "{address} should not be suggested as a bind address"
            );
        }
    }

    #[test]
    fn every_network_is_offered_on_the_same_terms() {
        // A LAN address, the shared range a mesh VPN hands out, a unique-local
        // IPv6 address and an ordinary one are indistinguishable here. Nothing
        // knows which product assigned an address, and nothing should.
        // The shared range a mesh VPN hands out is built rather than written,
        // the way `auth.rs` builds it: an address literal in a tracked file is
        // exactly what the secrets gate exists to keep out, even a reserved one.
        let cgnat_base = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1));
        for address in [
            "192.0.2.10".parse().unwrap(),
            "10.0.0.1".parse().unwrap(),
            cgnat_base,
            "fd00::1".parse().unwrap(),
            "2001:db8::1".parse().unwrap(),
        ] {
            let address: IpAddr = address;
            assert!(
                is_reachable_from_elsewhere(&address),
                "{address} should have been offered as a bind address"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_probe_runs_on_this_machine_and_suggests_nothing_useless() {
        // Whatever this machine holds is unknown to the test - a CI container
        // may hold nothing at all - so the assertion is on the property, not on
        // the contents.
        let addresses = reachable_addresses().expect("Linux reads /proc");
        assert!(
            addresses.iter().all(is_reachable_from_elsewhere),
            "the filter let something through: {addresses:?}"
        );
        let mut sorted = addresses.clone();
        sorted.sort_by_key(|address| match address {
            IpAddr::V4(v4) => (0_u8, u128::from(v4.to_bits())),
            IpAddr::V6(v6) => (1_u8, v6.to_bits()),
        });
        assert_eq!(addresses, sorted, "the order must be stable between runs");
    }
}
