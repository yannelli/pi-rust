use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use pi_tui::ui_loop::{self, TimerId};

struct CancelOnDrop(TimerId);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        ui_loop::clear_timer(self.0);
    }
}

fn timer_owning_another_timer() -> (TimerId, TimerId) {
    let inner = ui_loop::set_interval(Duration::from_secs(60), || {});
    let guard = CancelOnDrop(inner);
    let outer = ui_loop::set_interval(Duration::from_secs(60), move || {
        std::hint::black_box(&guard);
    });
    (inner, outer)
}

#[test]
fn clearing_a_timer_can_cancel_another_timer_owned_by_its_callback() {
    let (inner, outer) = timer_owning_another_timer();
    ui_loop::clear_timer(outer);
    assert!(!ui_loop::timer_active(inner));
    assert!(!ui_loop::timer_active(outer));
}

#[test]
fn timers_can_be_cleared_during_thread_shutdown() {
    const CHILD: &str = "PI_TUI_TIMER_DROP_REGRESSION_CHILD";
    if std::env::var_os(CHILD).is_some() {
        std::thread::spawn(|| {
            timer_owning_another_timer();
        })
        .join()
        .unwrap();
        return;
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "timers_can_be_cleared_during_thread_shutdown", "--nocapture"])
        .env(CHILD, "1")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "timer cleanup child failed: {status}");
            break;
        }
        if started.elapsed() > Duration::from_secs(10) {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("timer cleanup child did not exit within 10 seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
