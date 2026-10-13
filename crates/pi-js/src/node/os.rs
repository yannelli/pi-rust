//! Natives for `node:os`: host facts read from `/proc` on Linux and from system tools elsewhere.

use std::process::Command;

use rquickjs::{Ctx, Object};

use super::core::platform;
use super::set_fn;

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

/// `[model, speedMHz]` per logical CPU.
fn cpus() -> Vec<(String, f64)> {
    let count = std::thread::available_parallelism().map_or(1, |count| count.get());
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
            (model, speed.floor())
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
    set_fn(&object, "cpus", |ctx: Ctx<'js>| {
        let list = rquickjs::Array::new(ctx.clone())?;
        for (index, (model, speed)) in cpus().into_iter().enumerate() {
            let cpu = Object::new(ctx.clone())?;
            cpu.set("model", model)?;
            cpu.set("speed", speed)?;
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
