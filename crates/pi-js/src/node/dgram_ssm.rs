//! Source-specific multicast for `node:dgram`, as libuv does it: IPv4 groups through socket2's
//! IP_ADD_SOURCE_MEMBERSHIP, IPv6 groups through MCAST_JOIN_SOURCE_GROUP on Linux and macOS.
#![allow(unsafe_code)]

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use socket2::SockRef;

pub(super) enum Membership {
    V4 { source: Ipv4Addr, group: Ipv4Addr, interface: Ipv4Addr },
    V6 { source: Ipv6Addr, group: Ipv6Addr, interface: u32 },
}

/// Parse the addresses like libuv: the group picks the family, and `None` means EINVAL.
pub(super) fn parse(source: &str, group: &str, interface: Option<&str>) -> Option<Membership> {
    match group.parse::<IpAddr>().ok()? {
        IpAddr::V4(group) => {
            let interface = interface.map_or(Some(Ipv4Addr::UNSPECIFIED), |text| text.parse().ok())?;
            Some(Membership::V4 { source: source.parse().ok()?, group, interface })
        }
        IpAddr::V6(group) => {
            let interface = interface.map_or(Some(0), scope_id)?;
            Some(Membership::V6 { source: source.parse().ok()?, group, interface })
        }
    }
}

/// The interface index of an IPv6 interface address: its `%zone`, or 0 without one.
fn scope_id(text: &str) -> Option<u32> {
    let (address, zone) = text.split_once('%').unwrap_or((text, ""));
    address.parse::<Ipv6Addr>().ok()?;
    Some(if zone.is_empty() { 0 } else { interface_index(zone) })
}

#[cfg(unix)]
fn interface_index(name: &str) -> u32 {
    let Ok(name) = std::ffi::CString::new(name) else { return 0 };
    // SAFETY: `name` is a valid NUL-terminated string for the duration of the call.
    unsafe { libc::if_nametoindex(name.as_ptr()) }
}

#[cfg(not(unix))]
fn interface_index(name: &str) -> u32 {
    name.parse().unwrap_or(0)
}

/// Join or leave the source-specific group on `socket`.
pub(super) fn apply(socket: &SockRef<'_>, membership: &Membership, join: bool) -> io::Result<()> {
    match membership {
        Membership::V4 { source, group, interface } if join => socket.join_ssm_v4(source, group, interface),
        Membership::V4 { source, group, interface } => socket.leave_ssm_v4(source, group, interface),
        Membership::V6 { source, group, interface } => source_group_v6(socket, source, group, *interface, join),
    }
}

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn source_group_v6(
    socket: &SockRef<'_>,
    source: &Ipv6Addr,
    group: &Ipv6Addr,
    interface: u32,
    join: bool,
) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: group_source_req holds only integers and byte arrays, so all zeroes is valid.
    let mut request: libc::group_source_req = unsafe { std::mem::zeroed() };
    request.gsr_interface = interface;
    request.gsr_group = storage(group);
    request.gsr_source = storage(source);
    let option = if join { libc::MCAST_JOIN_SOURCE_GROUP } else { libc::MCAST_LEAVE_SOURCE_GROUP };
    let length = std::mem::size_of::<libc::group_source_req>() as libc::socklen_t;
    // SAFETY: `request` is a valid group_source_req that lives through the call, and `length` is its size.
    let result = unsafe {
        libc::setsockopt(socket.as_raw_fd(), libc::IPPROTO_IPV6, option, (&raw const request).cast(), length)
    };
    if result < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn source_group_v6(_: &SockRef<'_>, _: &Ipv6Addr, _: &Ipv6Addr, _: u32, _: bool) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

/// `address` as a sockaddr_in6 inside a sockaddr_storage, with port, flow, and scope 0.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn storage(address: &Ipv6Addr) -> libc::sockaddr_storage {
    // SAFETY: both are plain C structs of integers, for which all zeroes is valid.
    let (mut storage, mut socket): (libc::sockaddr_storage, libc::sockaddr_in6) =
        unsafe { (std::mem::zeroed(), std::mem::zeroed()) };
    socket.sin6_family = libc::AF_INET6 as libc::sa_family_t;
    socket.sin6_addr = libc::in6_addr { s6_addr: address.octets() };
    #[cfg(target_vendor = "apple")]
    {
        socket.sin6_len = std::mem::size_of::<libc::sockaddr_in6>() as u8;
    }
    // SAFETY: sockaddr_storage is larger than sockaddr_in6, and the two do not overlap.
    unsafe {
        std::ptr::copy_nonoverlapping(
            (&raw const socket).cast::<u8>(),
            (&raw mut storage).cast::<u8>(),
            std::mem::size_of::<libc::sockaddr_in6>(),
        );
    }
    storage
}
