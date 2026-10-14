//! Natives for `node:os`: host facts read from `/proc` on Linux and from system tools elsewhere.

use std::process::Command;

use rquickjs::{Ctx, Object};

use super::core::platform;
use super::set_fn;
use crate::error::JsError;
use crate::runtime::convert::throw;

/// Node's `SystemError` for a failed libuv call: code `ERR_SYSTEM_ERROR` with an `info` object.
pub(crate) fn system_error(error: &std::io::Error, syscall: &str) -> JsError {
    let (code, description) = crate::error::io_code(error);
    let errno = error.raw_os_error().map_or(0, |errno| -errno);
    let info = serde_json::json!({ "errno": errno, "code": code, "message": description, "syscall": syscall });
    JsError::with_name("SystemError", format!("A system error occurred: {syscall} returned {code} ({description})"))
        .with_code("ERR_SYSTEM_ERROR")
        .with_data("errno", errno)
        .with_data("syscall", syscall)
        .with_data("info", info)
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn proc_file(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|text| text.trim().to_string())
}

fn hostname() -> String {
    proc_file("/proc/sys/kernel/hostname")
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| command_output("hostname", &[]))
        .unwrap_or_else(|| "localhost".into())
}

fn homedir() -> String {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var(key).unwrap_or_default()
}

fn tmpdir() -> String {
    let dir = std::env::temp_dir().display().to_string();
    let trimmed = dir.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() || trimmed.ends_with(':') { dir } else { trimmed.to_string() }
}

/// A `/proc/meminfo` field in bytes.
fn meminfo(field: &str) -> Option<f64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = text.lines().find(|line| line.starts_with(field))?;
    let kb: f64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024.0)
}

fn total_memory() -> f64 {
    meminfo("MemTotal:")
        .or_else(|| command_output("sysctl", &["-n", "hw.memsize"]).and_then(|text| text.parse().ok()))
        .unwrap_or(0.0)
}

fn free_memory() -> f64 {
    if let Some(bytes) = meminfo("MemAvailable:") {
        return bytes;
    }
    // macOS: free plus inactive pages from vm_stat.
    let Some(text) = command_output("vm_stat", &[]) else { return 0.0 };
    let page = text
        .lines()
        .next()
        .and_then(|line| line.split("page size of ").nth(1))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|size| size.parse::<f64>().ok())
        .unwrap_or(4096.0);
    let pages = |name: &str| {
        text.lines()
            .find(|line| line.starts_with(name))
            .and_then(|line| line.rsplit(':').next())
            .and_then(|value| value.trim().trim_end_matches('.').parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    (pages("Pages free") + pages("Pages inactive")) * page
}

fn uptime() -> f64 {
    if let Some(seconds) = proc_file("/proc/uptime").and_then(|text| text.split_whitespace().next()?.parse().ok()) {
        return seconds;
    }
    let boot = command_output("sysctl", &["-n", "kern.boottime"])
        .and_then(|text| text.split("sec = ").nth(1)?.split(',').next()?.trim().parse::<f64>().ok());
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |time| time.as_secs_f64());
    boot.map_or(0.0, |boot| (now - boot).max(0.0))
}

fn loadavg() -> Vec<f64> {
    let text = proc_file("/proc/loadavg").or_else(|| {
        command_output("sysctl", &["-n", "vm.loadavg"]).map(|text| text.trim_matches(['{', '}', ' ']).to_string())
    });
    let values: Vec<f64> = text
        .map(|text| text.split_whitespace().take(3).filter_map(|value| value.parse().ok()).collect())
        .unwrap_or_default();
    if values.len() == 3 { values } else { vec![0.0; 3] }
}

/// `[model, speedMHz, times]` per CPU. The count follows the CPU times, as in libuv, which lists
/// every online CPU rather than those this process may run on.
fn cpus() -> Vec<(String, f64, super::os_cpus::Times)> {
    let times = super::os_cpus::cpu_times().unwrap_or_default();
    let parallelism = std::thread::available_parallelism().map_or(1, |count| count.get());
    let count = if times.is_empty() { parallelism } else { times.len() };
    let info = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let field = |name: &str| -> Vec<String> {
        info.lines()
            .filter(|line| line.starts_with(name))
            .filter_map(|line| line.split(':').nth(1).map(|value| value.trim().to_string()))
            .collect()
    };
    let models = field("model name");
    let speeds = field("cpu MHz");
    let fallback_model = command_output("sysctl", &["-n", "machdep.cpu.brand_string"]).unwrap_or_default();
    (0..count)
        .map(|index| {
            let model = models.get(index).cloned().unwrap_or_else(|| fallback_model.clone());
            let speed = speeds.get(index).and_then(|speed| speed.parse::<f64>().ok()).unwrap_or(0.0);
            (model, speed.floor(), times.get(index).copied().unwrap_or_default())
        })
        .collect()
}

fn os_type() -> &'static str {
    match platform() {
        "linux" => "Linux",
        "darwin" => "Darwin",
        "win32" => "Windows_NT",
        "freebsd" => "FreeBSD",
        "openbsd" => "OpenBSD",
        _ => "Unknown",
    }
}

fn release() -> String {
    proc_file("/proc/sys/kernel/osrelease").or_else(|| command_output("uname", &["-r"])).unwrap_or_default()
}

fn version() -> String {
    proc_file("/proc/sys/kernel/version").or_else(|| command_output("uname", &["-v"])).unwrap_or_default()
}

fn machine() -> String {
    command_output("uname", &["-m"]).unwrap_or_else(|| std::env::consts::ARCH.to_string())
}

fn username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .or_else(|| command_output("id", &["-un"]))
        .unwrap_or_default()
}

pub(crate) fn natives<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let object = Object::new(ctx.clone())?;
    set_fn(&object, "hostname", hostname)?;
    set_fn(&object, "homedir", homedir)?;
    set_fn(&object, "tmpdir", tmpdir)?;
    set_fn(&object, "totalmem", total_memory)?;
    set_fn(&object, "freemem", free_memory)?;
    set_fn(&object, "uptime", uptime)?;
    set_fn(&object, "loadavg", loadavg)?;
    set_fn(&object, "type", os_type)?;
    set_fn(&object, "release", release)?;
    set_fn(&object, "version", version)?;
    set_fn(&object, "machine", machine)?;
    set_fn(&object, "username", username)?;
    set_fn(&object, "shell", || std::env::var("SHELL").ok())?;
    set_fn(&object, "parallelism", || std::thread::available_parallelism().map_or(1, |count| count.get()))?;
    set_fn(&object, "getPriority", |ctx: Ctx<'js>, pid: i32| {
        super::sys::get_priority(pid).map_err(|error| throw(&ctx, &system_error(&error, "uv_os_getpriority")))
    })?;
    set_fn(&object, "setPriority", |ctx: Ctx<'js>, pid: i32, priority: i32| {
        super::sys::set_priority(pid, priority).map_err(|error| throw(&ctx, &system_error(&error, "uv_os_setpriority")))
    })?;
    set_fn(&object, "resourceUsage", |ctx: Ctx<'js>| {
        super::sys::resource_usage()
            .map(|usage| usage.to_vec())
            .map_err(|error| throw(&ctx, &system_error(&error, "uv_getrusage")))
    })?;
    set_fn(&object, "networkInterfaces", |ctx: Ctx<'js>| {
        let entries = super::sys::network_interfaces()
            .map_err(|error| throw(&ctx, &system_error(&error, "uv_interface_addresses")))?;
        let list = rquickjs::Array::new(ctx.clone())?;
        for (index, entry) in entries.into_iter().enumerate() {
            let item = Object::new(ctx.clone())?;
            item.set("name", entry.name)?;
            item.set("address", entry.address.to_string())?;
            item.set("netmask", entry.netmask.to_string())?;
            item.set("family", if entry.address.is_ipv6() { "IPv6" } else { "IPv4" })?;
            let mac = entry.mac.iter().map(|byte| format!("{byte:02x}")).collect::<Vec<_>>().join(":");
            item.set("mac", mac)?;
            item.set("internal", entry.internal)?;
            if entry.address.is_ipv6() {
                item.set("scopeid", entry.scope_id)?;
            }
            list.set(index, item)?;
        }
        Ok::<_, rquickjs::Error>(list)
    })?;
    set_fn(&object, "cpus", |ctx: Ctx<'js>| {
        let list = rquickjs::Array::new(ctx.clone())?;
        for (index, (model, speed, times)) in cpus().into_iter().enumerate() {
            let cpu = Object::new(ctx.clone())?;
            cpu.set("model", model)?;
            cpu.set("speed", speed)?;
            let fields = Object::new(ctx.clone())?;
            for (name, value) in ["user", "nice", "sys", "idle", "irq"].into_iter().zip(times) {
                fields.set(name, value)?;
            }
            cpu.set("times", fields)?;
            list.set(index, cpu)?;
        }
        Ok::<_, rquickjs::Error>(list)
    })?;
    Ok(object)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_host_facts() {
        assert!(!hostname().is_empty());
        assert!(!tmpdir().ends_with('/') || tmpdir() == "/");
        assert_eq!(loadavg().len(), 3);
        assert!(!cpus().is_empty());
        if cfg!(target_os = "linux") {
            assert!(total_memory() > 0.0);
            assert!(free_memory() > 0.0);
            assert!(uptime() > 0.0);
            assert_eq!(os_type(), "Linux");
        }
    }
}
