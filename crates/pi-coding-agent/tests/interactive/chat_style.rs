//! `ctx.ui.setChatStyle` editor fields: prompt layouts and the border color override.

use std::time::Duration;

use pi_ai::providers::faux::FauxResponse;
use pi_coding_agent::theme::{Theme, ThemeColor, theme};
use pi_tui::colors::{foreground_ansi, parse_color};

use crate::interactive::harness::{Options, Ui, local, start};

const STYLE: &str = r#"export default function (pi) {
	pi.registerCommand("chat-style", {
		description: "Set the chat style",
		handler: async (args, ctx) => ctx.ui.setChatStyle(args ? JSON.parse(args) : undefined),
	});
}
"#;

async fn set_style(ui: &Ui, style: &str) {
    ui.submit(&format!("/chat-style {style}")).await;
    ui.settle(100).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_layouts_and_border_colors_follow_the_chat_style() {
    local(async {
        let ui = start(Options {
            files: vec![(".pi/extensions/style.ts", STYLE)],
            responses: vec![FauxResponse::text("streamed reply text")],
            chunk_delay: Duration::from_millis(150),
            ..Options::default()
        })
        .await;

        set_style(&ui, r#"{"promptLayout":"caret"}"#).await;
        ui.wait_for("│ › Ask anything · / for commands · @ for files").await;
        assert!(ui.screen().contains("╭"), "{}", ui.screen());

        ui.submit("hello").await;
        ui.wait_until("running placeholder", |ui| ui.writes().contains("ueue a message · esc to stop the agent")).await;
        ui.wait_for("streamed reply text").await;
        ui.wait_until("idle placeholder", |ui| ui.screen().contains("Ask anything")).await;

        set_style(&ui, r#"{"promptLayout":"harbor","editorBorderColor":"success"}"#).await;
        let success = format!("{}╭", theme().get_fg_ansi(ThemeColor::Success));
        ui.wait_until("success border", |ui| ui.writes().contains(&success)).await;

        set_style(&ui, r##"{"editorBorderColor":"#ff8800"}"##).await;
        let color = parse_color("#ff8800").unwrap();
        let hex = format!("{}───", foreground_ansi(&color, theme().get_color_mode()));
        ui.wait_until("hex border", |ui| ui.writes().contains(&hex)).await;
        let screen = ui.screen();
        assert!(!screen.contains("Ask anything") && !screen.contains("╭"), "{screen}");

        let before = ui.writes().len();
        set_style(&ui, "").await;
        let thinking = theme().get_fg_ansi(Theme::thinking_border_token(ui.session().thinking_level()));
        let thinking = format!("{thinking}───");
        ui.wait_until("thinking border", |ui| ui.writes()[before..].contains(&thinking)).await;
    })
    .await;
}
