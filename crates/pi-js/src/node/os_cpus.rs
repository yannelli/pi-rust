//! Per-CPU times for `os.cpus()` in milliseconds, read as libuv reads them: /proc/stat on Linux,
//! host_processor_info on macOS, and NtQuerySystemInformation on Windows.
#![allow(unsafe_code)]

/// `[user, nice, sys, idle, irq]` in milliseconds.
pub(crate) type Times = [f64; 5];

/// One entry per CPU, or `None` where the platform gives no times.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn cpu_times() -> Option<Vec<Times>> {
    // libuv reads clock ticks and takes USER_HZ as 100, so a tick is 10 ms.
    const MS_PER_TICK: f64 = 10.0;
    let text = std::fs::read_to_string("/proc/stat").ok()?;
    let mut list = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("cpu").filter(|rest| rest.starts_with(|c: char| c.is_ascii_digit())) else {
            continue;
        };
        let fields: Vec<f64> = rest.split_whitespace().skip(1).map(|field| field.parse().unwrap_or(0.0)).collect();
        let get = |index: usize| fields.get(index).copied().unwrap_or(0.0) * MS_PER_TICK;
        // user, nice, system, idle, iowait, irq: libuv skips iowait.
        list.push([get(0), get(1), get(2), get(3), get(5)]);
    }
    (!list.is_empty()).then_some(list)
}

#[cfg(target_os = "macos")]
#[allow(deprecated)]
pub(crate) fn cpu_times() -> Option<Vec<Times>> {
    let mut count: libc::natural_t = 0;
    let mut info: libc::processor_info_array_t = std::ptr::null_mut();
    let mut length: libc::mach_msg_type_number_t = 0;
    // SAFETY: the kernel fills `info` with `count` load records, which are freed below.
    unsafe {
        let ticks = libc::sysconf(libc::_SC_CLK_TCK);
        let multiplier = if ticks > 0 { 1000.0 / ticks as f64 } else { 10.0 };
        let flavor = libc::PROCESSOR_CPU_LOAD_INFO;
        let status = libc::host_processor_info(libc::mach_host_self(), flavor, &mut count, &mut info, &mut length);
        if status != libc::KERN_SUCCESS || info.is_null() {
            return None;
        }
        let loads = std::slice::from_raw_parts(info.cast::<libc::processor_cpu_load_info>(), count as usize);
        // cpu_ticks holds user, system, idle, and nice; macOS reports no interrupt time.
        let list = loads
            .iter()
            .map(|load| {
                let tick = |index: usize| f64::from(load.cpu_ticks[index]) * multiplier;
                [tick(0), tick(3), tick(1), tick(2), 0.0]
            })
            .collect();
        let bytes = length as usize * std::mem::size_of::<libc::integer_t>();
        libc::vm_deallocate(libc::mach_task_self(), info as libc::vm_address_t, bytes as libc::vm_size_t);
        Some(list)
    }
}

#[cfg(windows)]
pub(crate) fn cpu_times() -> Option<Vec<Times>> {
    use windows_sys::Wdk::System::SystemInformation::{
        NtQuerySystemInformation, SystemProcessorPerformanceInformation,
    };

    /// SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION; times are in 100 ns units.
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Performance {
        idle: i64,
        kernel: i64,
        user: i64,
        dpc: i64,
        interrupt: i64,
        interrupt_count: u32,
    }
    let count = std::thread::available_parallelism().map_or(1, |count| count.get()).max(64);
    let mut entries = vec![Performance::default(); count];
    let size = std::mem::size_of_val(entries.as_slice()) as u32;
    let mut written = 0u32;
    // SAFETY: the buffer holds `count` records and the call writes at most `size` bytes.
    let status = unsafe {
        NtQuerySystemInformation(SystemProcessorPerformanceInformation, entries.as_mut_ptr().cast(), size, &mut written)
    };
    if status < 0 {
        return None;
    }
    entries.truncate(written as usize / std::mem::size_of::<Performance>());
    // libuv: system time is kernel time without idle time.
    let ms = |value: i64| (value / 10_000) as f64;
    let list = entries
        .iter()
        .map(|entry| [ms(entry.user), 0.0, ms(entry.kernel - entry.idle), ms(entry.idle), ms(entry.interrupt)])
        .collect::<Vec<_>>();
    (!list.is_empty()).then_some(list)
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos", windows)))]
pub(crate) fn cpu_times() -> Option<Vec<Times>> {
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn reports_cpu_times() {
        if cfg!(any(target_os = "linux", target_os = "macos", windows)) {
            let times = super::cpu_times().expect("cpu times");
            assert!(!times.is_empty());
            assert!(times.iter().all(|cpu| cpu.iter().all(|value| *value >= 0.0 && value.fract() == 0.0)));
            assert!(times.iter().any(|cpu| cpu[0] + cpu[2] + cpu[3] > 0.0));
        }
    }
}
