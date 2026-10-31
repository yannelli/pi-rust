//! `/usage`: the subscription limits of every logged-in provider, fetched again on each call.

use super::mode::InteractiveMode;
use crate::theme::{ThemeColor, theme};
use crate::usage_cli::{Tone, report_text};

impl InteractiveMode {
    pub(super) fn handle_usage_command(&self, provider: Option<String>) {
        self.show_status("Fetching subscription usage...");
        let runtime = self.session().model_runtime().clone();
        self.spawn(move |mode| async move {
            let service = runtime.usage();
            let usages = match provider {
                Some(provider) => vec![service.get(&provider, true).await],
                None => service.all(true).await,
            };
            let render = move || {
                let t = theme();
                report_text(&usages, pi_ai::now_ms() / 1000, &|tone, text| match tone {
                    Tone::Title => t.bold(text),
                    Tone::Label => t.fg(ThemeColor::Dim, text),
                    Tone::Ok => t.fg(ThemeColor::Success, text),
                    Tone::Warn => t.fg(ThemeColor::Warning, text),
                    Tone::High | Tone::Error => t.fg(ThemeColor::Error, text),
                })
            };
            mode.add_spacer();
            mode.add_text(render, 1, 0);
            mode.request_render();
        });
    }
}
