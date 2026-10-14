//! System calls libuv makes for Node: resource usage, process priority, and network
//! interfaces, through libc on Unix and windows-sys on Windows.
#![allow(unsafe_code)]

use std::io;
use std::net::IpAddr;

/// One address of a network interface, as `os.networkInterfaces()` reports it.
pub(crate) struct InterfaceAddress {
    pub name: String,
    pub address: IpAddr,
    pub netmask: IpAddr,
    pub mac: [u8; 6],
    pub internal: bool,
    pub scope_id: u32,
}

/// `getrusage(RUSAGE_SELF)` fields in libuv's order: user and system CPU time in
/// microseconds, max RSS in KiB, then the counters.
pub(crate) type ResourceUsage = [f64; 16];

#[cfg(unix)]
mod imp {
    use std::io;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use super::{InterfaceAddress, ResourceUsage};

    fn micros(time: libc::timeval) -> f64 {
        time.tv_sec as f64 * 1e6 + time.tv_usec as f64
    }

    pub(crate) fn resource_usage() -> io::Result<ResourceUsage> {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let max_rss = if cfg!(target_os = "macos") { usage.ru_maxrss / 1024 } else { usage.ru_maxrss };
        Ok([
            micros(usage.ru_utime),
            micros(usage.ru_stime),
            max_rss as f64,
            usage.ru_ixrss as f64,
            usage.ru_idrss as f64,
            usage.ru_isrss as f64,
            usage.ru_minflt as f64,
            usage.ru_majflt as f64,
            usage.ru_nswap as f64,
            usage.ru_inblock as f64,
            usage.ru_oublock as f64,
            usage.ru_msgsnd as f64,
            usage.ru_msgrcv as f64,
            usage.ru_nsignals as f64,
            usage.ru_nvcsw as f64,
            usage.ru_nivcsw as f64,
        ])
    }

    pub(crate) fn get_priority(pid: i32) -> io::Result<i32> {
        // getpriority may return -1 on success, so errno tells failures apart.
        reset_errno();
        let value = unsafe { libc::getpriority(libc::PRIO_PROCESS, pid as libc::id_t) };
        let error = io::Error::last_os_error();
        if value == -1 && error.raw_os_error().is_some_and(|code| code != 0) {
            return Err(error);
        }
        Ok(value)
    }

    fn reset_errno() {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        unsafe {
            *libc::__errno_location() = 0;
        }
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
        unsafe {
            *libc::__error() = 0;
        }
    }

    static ORIGINAL_MODE: std::sync::Mutex<Option<libc::termios>> = std::sync::Mutex::new(None);

    /// libuv's raw mode for descriptor 0, restoring the first saved mode when leaving it.
    pub(crate) fn set_raw_mode(raw: bool) -> io::Result<()> {
        let mut saved = ORIGINAL_MODE.lock().unwrap_or_else(|poison| poison.into_inner());
        let mut mode: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(0, &mut mode) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let original = *saved.get_or_insert(mode);
        let mut next = original;
        if raw {
            next.c_iflag &= !(libc::BRKINT | libc::ICRNL | libc::INPCK | libc::ISTRIP | libc::IXON);
            next.c_oflag |= libc::ONLCR;
            next.c_cflag |= libc::CS8;
            next.c_lflag &= !(libc::ECHO | libc::ICANON | libc::IEXTEN | libc::ISIG);
            next.c_cc[libc::VMIN] = 1;
            next.c_cc[libc::VTIME] = 0;
        }
        if unsafe { libc::tcsetattr(0, libc::TCSADRAIN, &next) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(crate) fn set_priority(pid: i32, priority: i32) -> io::Result<()> {
        if unsafe { libc::setpriority(libc::PRIO_PROCESS, pid as libc::id_t, priority) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    unsafe fn ip_of(address: *const libc::sockaddr) -> Option<(IpAddr, u32)> {
        if address.is_null() {
            return None;
        }
        match i32::from(unsafe { (*address).sa_family }) {
            libc::AF_INET => {
                let v4 = unsafe { &*(address as *const libc::sockaddr_in) };
                Some((IpAddr::V4(Ipv4Addr::from(u32::from_be(v4.sin_addr.s_addr))), 0))
            }
            libc::AF_INET6 => {
                let v6 = unsafe { &*(address as *const libc::sockaddr_in6) };
                Some((IpAddr::V6(Ipv6Addr::from(v6.sin6_addr.s6_addr)), v6.sin6_scope_id))
            }
            _ => None,
        }
    }

    /// The hardware address in a link-layer entry (AF_PACKET or AF_LINK).
    unsafe fn mac_of(address: *const libc::sockaddr) -> Option<[u8; 6]> {
        if address.is_null() {
            return None;
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if i32::from(unsafe { (*address).sa_family }) == libc::AF_PACKET {
            let link = unsafe { &*(address as *const libc::sockaddr_ll) };
            let mut mac = [0; 6];
            mac.copy_from_slice(&link.sll_addr[..6]);
            return Some(mac);
        }
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
        if i32::from(unsafe { (*address).sa_family }) == libc::AF_LINK {
            let link = unsafe { &*(address as *const libc::sockaddr_dl) };
            let start = usize::from(link.sdl_nlen);
            let data = unsafe { std::slice::from_raw_parts(link.sdl_data.as_ptr() as *const u8, start + 6) };
            let mut mac = [0; 6];
            mac.copy_from_slice(&data[start..start + 6]);
            return Some(mac);
        }
        None
    }

    pub(crate) fn network_interfaces() -> io::Result<Vec<InterfaceAddress>> {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if unsafe { libc::getifaddrs(&mut head) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut entries = Vec::new();
        let mut macs = Vec::new();
        let mut cursor = head;
        while !cursor.is_null() {
            let entry = unsafe { &*cursor };
            cursor = entry.ifa_next;
            let flags = entry.ifa_flags;
            let up = flags & (libc::IFF_UP as u32) != 0 && flags & (libc::IFF_RUNNING as u32) != 0;
            if !up || entry.ifa_name.is_null() {
                continue;
            }
            let name = unsafe { std::ffi::CStr::from_ptr(entry.ifa_name) }.to_string_lossy().into_owned();
            if let Some(mac) = unsafe { mac_of(entry.ifa_addr) } {
                macs.push((name, mac));
                continue;
            }
            let Some((address, scope_id)) = (unsafe { ip_of(entry.ifa_addr) }) else { continue };
            let netmask = unsafe { ip_of(entry.ifa_netmask) }.map(|(mask, _)| mask).unwrap_or(match address {
                IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            });
            let internal = flags & (libc::IFF_LOOPBACK as u32) != 0;
            entries.push(InterfaceAddress { name, address, netmask, mac: [0; 6], internal, scope_id });
        }
        unsafe { libc::freeifaddrs(head) };
        for entry in &mut entries {
            if let Some((_, mac)) = macs.iter().find(|(name, _)| *name == entry.name) {
                entry.mac = *mac;
            }
        }
        Ok(entries)
    }
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_BUFFER_OVERFLOW, FILETIME, HANDLE};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses,
        IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR_IN, SOCKADDR_IN6};
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::{
        ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, GetPriorityClass,
        GetProcessIoCounters, GetProcessTimes, HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS, IO_COUNTERS,
        NORMAL_PRIORITY_CLASS, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION,
        REALTIME_PRIORITY_CLASS, SetPriorityClass,
    };

    use super::{InterfaceAddress, ResourceUsage};

    fn micros(time: FILETIME) -> f64 {
        ((u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)) as f64 / 10.0
    }

    pub(crate) fn resource_usage() -> io::Result<ResourceUsage> {
        let process = unsafe { GetCurrentProcess() };
        let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
        if unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut memory: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
        memory.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        if unsafe { GetProcessMemoryInfo(process, &mut memory, memory.cb) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut io_counters: IO_COUNTERS = unsafe { std::mem::zeroed() };
        if unsafe { GetProcessIoCounters(process, &mut io_counters) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut usage = [0.0; 16];
        usage[0] = micros(user);
        usage[1] = micros(kernel);
        usage[2] = (memory.PeakWorkingSetSize / 1024) as f64;
        usage[7] = f64::from(memory.PageFaultCount);
        usage[9] = io_counters.ReadOperationCount as f64;
        usage[10] = io_counters.WriteOperationCount as f64;
        Ok(usage)
    }

    fn open(pid: i32, access: u32) -> io::Result<(HANDLE, bool)> {
        if pid == 0 || pid as u32 == std::process::id() {
            return Ok((unsafe { GetCurrentProcess() }, false));
        }
        let handle = unsafe { OpenProcess(access, 0, pid as u32) };
        if handle.is_null() { Err(io::Error::last_os_error()) } else { Ok((handle, true)) }
    }

    pub(crate) fn get_priority(pid: i32) -> io::Result<i32> {
        let (handle, owned) = open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
        let class = unsafe { GetPriorityClass(handle) };
        let error = io::Error::last_os_error();
        if owned {
            unsafe { CloseHandle(handle) };
        }
        Ok(match class {
            0 => return Err(error),
            REALTIME_PRIORITY_CLASS => -20,
            HIGH_PRIORITY_CLASS => -14,
            ABOVE_NORMAL_PRIORITY_CLASS => -7,
            BELOW_NORMAL_PRIORITY_CLASS => 10,
            IDLE_PRIORITY_CLASS => 19,
            _ => 0,
        })
    }

    pub(crate) fn set_priority(pid: i32, priority: i32) -> io::Result<()> {
        let class = match priority {
            ..-14 => REALTIME_PRIORITY_CLASS,
            -14..-7 => HIGH_PRIORITY_CLASS,
            -7..0 => ABOVE_NORMAL_PRIORITY_CLASS,
            0..10 => NORMAL_PRIORITY_CLASS,
            10..19 => BELOW_NORMAL_PRIORITY_CLASS,
            _ => IDLE_PRIORITY_CLASS,
        };
        let (handle, owned) = open(pid, PROCESS_SET_INFORMATION)?;
        let ok = unsafe { SetPriorityClass(handle, class) } != 0;
        let error = io::Error::last_os_error();
        if owned {
            unsafe { CloseHandle(handle) };
        }
        if ok { Ok(()) } else { Err(error) }
    }

    /// libuv's console modes: raw keeps only window input; normal echoes and edits lines.
    pub(crate) fn set_raw_mode(raw: bool) -> io::Result<()> {
        use windows_sys::Win32::System::Console::{
            ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_WINDOW_INPUT, GetStdHandle,
            STD_INPUT_HANDLE, SetConsoleMode,
        };
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        let mode =
            if raw { ENABLE_WINDOW_INPUT } else { ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT };
        if unsafe { SetConsoleMode(handle, mode) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn prefix_mask(v6: bool, bits: u8) -> IpAddr {
        if v6 {
            let mask = if bits == 0 { 0 } else { u128::MAX << (128 - u32::from(bits.min(128))) };
            IpAddr::V6(Ipv6Addr::from(mask))
        } else {
            let mask = if bits == 0 { 0 } else { u32::MAX << (32 - u32::from(bits.min(32))) };
            IpAddr::V4(Ipv4Addr::from(mask))
        }
    }

    pub(crate) fn network_interfaces() -> io::Result<Vec<InterfaceAddress>> {
        let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
        let mut size: u32 = 16 * 1024;
        let mut buffer: Vec<u64>;
        loop {
            buffer = vec![0; (size as usize).div_ceil(8)];
            let list = buffer.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH;
            let result =
                unsafe { GetAdaptersAddresses(u32::from(AF_UNSPEC), flags, std::ptr::null(), list, &mut size) };
            match result {
                0 => break,
                ERROR_BUFFER_OVERFLOW => continue,
                code => return Err(io::Error::from_raw_os_error(code as i32)),
            }
        }
        let mut entries = Vec::new();
        let mut adapter = buffer.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
        while !adapter.is_null() {
            let current = unsafe { &*adapter };
            adapter = current.Next;
            if current.OperStatus != IfOperStatusUp {
                continue;
            }
            let name = unsafe { wide(current.FriendlyName) };
            let mut mac = [0; 6];
            if current.PhysicalAddressLength >= 6 {
                mac.copy_from_slice(&current.PhysicalAddress[..6]);
            }
            let internal = current.IfType == 24;
            let mut unicast = current.FirstUnicastAddress;
            while !unicast.is_null() {
                let address = unsafe { &*unicast };
                unicast = address.Next;
                let socket = address.Address.lpSockaddr;
                if socket.is_null() {
                    continue;
                }
                let bits = address.OnLinkPrefixLength;
                let (ip, scope_id) = match unsafe { (*socket).sa_family } {
                    AF_INET => {
                        let v4 = unsafe { &*(socket as *const SOCKADDR_IN) };
                        (IpAddr::V4(Ipv4Addr::from(u32::from_be(unsafe { v4.sin_addr.S_un.S_addr }))), 0)
                    }
                    AF_INET6 => {
                        let v6 = unsafe { &*(socket as *const SOCKADDR_IN6) };
                        (IpAddr::V6(Ipv6Addr::from(unsafe { v6.sin6_addr.u.Byte })), unsafe {
                            v6.Anonymous.sin6_scope_id
                        })
                    }
                    _ => continue,
                };
                let netmask = prefix_mask(ip.is_ipv6(), bits);
                entries.push(InterfaceAddress { name: name.clone(), address: ip, netmask, mac, internal, scope_id });
            }
        }
        Ok(entries)
    }

    unsafe fn wide(text: *const u16) -> String {
        if text.is_null() {
            return String::new();
        }
        let mut length = 0;
        while unsafe { *text.add(length) } != 0 {
            length += 1;
        }
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length) })
    }
}

pub(crate) fn resource_usage() -> io::Result<ResourceUsage> {
    imp::resource_usage()
}

pub(crate) fn get_priority(pid: i32) -> io::Result<i32> {
    imp::get_priority(pid)
}

pub(crate) fn set_priority(pid: i32, priority: i32) -> io::Result<()> {
    imp::set_priority(pid, priority)
}

pub(crate) fn network_interfaces() -> io::Result<Vec<InterfaceAddress>> {
    imp::network_interfaces()
}

pub(crate) fn set_raw_mode(raw: bool) -> io::Result<()> {
    imp::set_raw_mode(raw)
}
