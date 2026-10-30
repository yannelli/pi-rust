//! pi-rust host additions for extensions: `ctx.ui.writeTerminal`, and the footer data
//! `onBranchChange`, `getSessionStats`, and `getRunState`.

use pi_ai::providers::faux::FauxResponse;

use crate::harness::{Options, local, start};

const EXTENSION: &str = r#"import { Text } from "@earendil-works/pi-tui";
export default function (pi) {
	pi.registerCommand("raw", {
		description: "Write raw bytes",
		handler: async (_args, ctx) => {
			ctx.ui.writeTerminal("\x1b]1337;SetUserVar=plain=eA==\x07");
			ctx.ui.writeTerminal("\x1b]1337;SetUserVar=wrapped=eQ==\x07", { tmuxPassthrough: true });
		},
	});
	pi.registerCommand("stats-footer", {
		description: "Footer with stats",
		handler: async (_args, ctx) => {
			let changes = 0;
			ctx.ui.setFooter((tui, _theme, data) => {
				data.onBranchChange(() => {
					changes += 1;
					tui.requestRender();
				});
				return {
					render: () => {
						const stats = data.getSessionStats();
						return [
							`FOOTER branch=${data.getGitBranch()} changes=${changes} state=${data.getRunState()}`,
							`STATS in=${stats?.input} out=${stats?.output} window=${stats?.contextWindow}`,
						];
					},
					invalidate: () => {},
				};
			});
		},
	});
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_terminal_reaches_the_terminal() {
    local(async {
        let ui = start(Options { files: vec![(".pi/extensions/raw.ts", EXTENSION)], ..Options::default() }).await;
        ui.submit("/raw").await;
        let plain = "\x1b]1337;SetUserVar=plain=eA==\x07";
        let wrapped_raw = "\x1b]1337;SetUserVar=wrapped=eQ==\x07";
        let wrapped = match std::env::var_os("TMUX").filter(|value| !value.is_empty()) {
            Some(_) => format!("\x1bPtmux;{}\x1b\\", wrapped_raw.replace('\x1b', "\x1b\x1b")),
            None => wrapped_raw.to_string(),
        };
        ui.wait_until("raw writes", |ui| ui.writes().contains(plain) && ui.writes().contains(&wrapped)).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn footer_data_reports_stats_run_state_and_branch_changes() {
    local(async {
        let ui = start(Options {
            files: vec![(".pi/extensions/footer.ts", EXTENSION), (".git/HEAD", "ref: refs/heads/main\n")],
            responses: vec![FauxResponse::text("hello from faux")],
            ..Options::default()
        })
        .await;
        ui.submit("/stats-footer").await;
        ui.wait_for("FOOTER branch=main changes=0 state=idle").await;
        ui.wait_for("window=128000").await;

        ui.submit("hi").await;
        ui.wait_for("hello from faux").await;
        ui.wait_until("usage in the footer", |ui| {
            let screen = ui.screen();
            screen.contains("STATS in=") && !screen.contains("STATS in=0 ") && screen.contains("state=done")
        })
        .await;

        std::fs::write(ui.cwd().join(".git/HEAD"), "ref: refs/heads/a-much-longer-branch\n").unwrap();
        ui.wait_for("FOOTER branch=a-much-longer-branch changes=1").await;
    })
    .await;
}
