//! Addresses assigned to this machine's interfaces.
//!
//! Everything here is unfiltered: loopback, link-local and the rest come back
//! as the platform reports them. Deciding which of those another machine could
//! reach is `hermes-system-info`'s job, and it already does it in one place for
//! every platform.

use crate::ProbeError;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[cfg(target_os = "macos")]
mod platform {
    use super::{IpAddr, Ipv4Addr, Ipv6Addr, ProbeError};

    /// Read one address out of a `sockaddr`, if it is one we understand.
    ///
    /// `read_unaligned` throughout: the pointers in an `ifaddrs` chain point
    /// into a kernel-packed buffer and carry no alignment guarantee for the
    /// larger structs they are cast to.
    fn address_of(sockaddr: *const libc::sockaddr) -> Option<IpAddr> {
        if sockaddr.is_null() {
            return None;
        }
        // SAFETY: the pointer is non-null and came from `getifaddrs`, which
        // guarantees at least a `sockaddr` header; `sa_family` is in that header.
        #[allow(unsafe_code)]
        let family = i32::from(unsafe { std::ptr::read_unaligned(sockaddr).sa_family });

        if family == libc::AF_INET {
            // SAFETY: a family of `AF_INET` is the kernel's own statement that
            // this points at a `sockaddr_in`.
            #[allow(unsafe_code)]
            let v4 = unsafe { std::ptr::read_unaligned(sockaddr.cast::<libc::sockaddr_in>()) };
            // `s_addr` is network byte order; `from_bits` wants host order.
            return Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(v4.sin_addr.s_addr))));
        }
        if family == libc::AF_INET6 {
            // SAFETY: as above, for `AF_INET6` and `sockaddr_in6`.
            #[allow(unsafe_code)]
            let v6 = unsafe { std::ptr::read_unaligned(sockaddr.cast::<libc::sockaddr_in6>()) };
            return Some(IpAddr::V6(Ipv6Addr::from(v6.sin6_addr.s6_addr)));
        }
        None
    }

    pub(super) fn read() -> Result<Vec<IpAddr>, ProbeError> {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        // SAFETY: `head` is a live pointer variable for the call to fill in.
        #[allow(unsafe_code)]
        let rc = unsafe { libc::getifaddrs(&raw mut head) };
        if rc != 0 {
            return Err(ProbeError::last("getifaddrs"));
        }
        if head.is_null() {
            // No interfaces at all is a real answer, and an empty list says so.
            return Ok(Vec::new());
        }

        let mut addresses = Vec::new();
        let mut cursor = head;
        while !cursor.is_null() {
            // SAFETY: `cursor` is non-null and points at a live `ifaddrs` in the
            // list `getifaddrs` allocated, which stays valid until `freeifaddrs`
            // below.
            #[allow(unsafe_code)]
            let entry = unsafe { &*cursor };
            if let Some(address) = address_of(entry.ifa_addr) {
                addresses.push(address);
            }
            cursor = entry.ifa_next;
        }

        // SAFETY: `head` is exactly what `getifaddrs` returned and has not been
        // freed; every borrow of the list ended above.
        #[allow(unsafe_code)]
        unsafe {
            libc::freeifaddrs(head);
        }
        Ok(addresses)
    }
}

#[cfg(windows)]
mod platform {
    use super::{IpAddr, Ipv4Addr, Ipv6Addr, ProbeError};
    use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
        GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::Networking::WinSock::{
        AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6,
    };

    fn address_of(sockaddr: *const SOCKADDR) -> Option<IpAddr> {
        if sockaddr.is_null() {
            return None;
        }
        // SAFETY: non-null, and supplied by the API as at least a `SOCKADDR`.
        #[allow(unsafe_code)]
        let family = unsafe { std::ptr::read_unaligned(sockaddr).sa_family };

        if family == AF_INET {
            // SAFETY: the family is the API's own statement of the real type.
            #[allow(unsafe_code)]
            let v4 = unsafe { std::ptr::read_unaligned(sockaddr.cast::<SOCKADDR_IN>()) };
            // SAFETY: the union's `S_addr` member is always readable; every
            // representation of four bytes is a valid `u32`.
            #[allow(unsafe_code)]
            let bits = unsafe { v4.sin_addr.S_un.S_addr };
            return Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(bits))));
        }
        if family == AF_INET6 {
            // SAFETY: as above, for `AF_INET6`.
            #[allow(unsafe_code)]
            let v6 = unsafe { std::ptr::read_unaligned(sockaddr.cast::<SOCKADDR_IN6>()) };
            // SAFETY: the union's byte member is always readable.
            #[allow(unsafe_code)]
            let octets = unsafe { v6.sin6_addr.u.Byte };
            return Some(IpAddr::V6(Ipv6Addr::from(octets)));
        }
        None
    }

    pub(super) fn read() -> Result<Vec<IpAddr>, ProbeError> {
        // Unicast addresses are the only ones that answer the question; asking
        // the API to skip the rest keeps the buffer small and the walk short.
        let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;

        let mut bytes: u32 = 0;
        // SAFETY: a null buffer with a live length asks the API for its
        // required size; it writes only to `bytes`.
        #[allow(unsafe_code)]
        let rc = unsafe {
            GetAdaptersAddresses(
                u32::from(AF_UNSPEC),
                flags,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut bytes,
            )
        };
        if rc == ERROR_SUCCESS || bytes == 0 {
            // Nothing to report, which is a real answer rather than a failure.
            return Ok(Vec::new());
        }
        if rc != ERROR_BUFFER_OVERFLOW {
            return Err(ProbeError::from_win32("GetAdaptersAddresses", rc));
        }

        let len = usize::try_from(bytes).unwrap_or(0);
        // A `u64` buffer rather than `u8`: these records are a pointer-linked
        // chain, and this is the cheapest way to guarantee their alignment.
        let mut buffer = vec![0u64; len.div_ceil(std::mem::size_of::<u64>()).max(1)];
        // SAFETY: `buffer` holds at least `bytes` bytes and `bytes` says so.
        #[allow(unsafe_code)]
        let rc = unsafe {
            GetAdaptersAddresses(
                u32::from(AF_UNSPEC),
                flags,
                std::ptr::null_mut(),
                buffer.as_mut_ptr().cast(),
                &raw mut bytes,
            )
        };
        if rc != ERROR_SUCCESS {
            return Err(ProbeError::from_win32("GetAdaptersAddresses", rc));
        }

        let mut addresses = Vec::new();
        let mut adapter: *const IP_ADAPTER_ADDRESSES_LH = buffer.as_ptr().cast();
        while !adapter.is_null() {
            // SAFETY: `adapter` is non-null and points into the buffer the call
            // above filled, which outlives this walk.
            #[allow(unsafe_code)]
            let entry = unsafe { &*adapter };

            let mut unicast = entry.FirstUnicastAddress;
            while !unicast.is_null() {
                // SAFETY: non-null, and part of the same filled buffer.
                #[allow(unsafe_code)]
                let address = unsafe { &*unicast };
                if let Some(ip) = address_of(address.Address.lpSockaddr) {
                    addresses.push(ip);
                }
                unicast = address.Next;
            }
            adapter = entry.Next;
        }
        Ok(addresses)
    }
}

/// Every address currently assigned to an interface on this machine.
pub fn read() -> Result<Vec<IpAddr>, ProbeError> {
    platform::read()
}
