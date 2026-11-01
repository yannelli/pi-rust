//! Extension components expose live viewport capture and the bundled list's page selection.

use pi_tui::Component;
use serde_json::json;

use crate::harness::{Setup, harness};

const SOURCE: &str = r#"
import { SelectList } from "@earendil-works/pi-tui";
export default function (pi) {
    pi.registerMessageRenderer("page-list", () => {
        const identity = text => text;
        return new SelectList(
            Array.from({ length: 12 }, (_, i) => ({ value: `item-${i}`, label: `item-${i}` })),
            5,
            { selectedPrefix: identity, selectedText: identity, description: identity,
              scrollInfo: identity, noMatch: identity },
        );
    });
    pi.registerMessageRenderer("dynamic-capture", () => {
        let capture = false;
        return {
            get capturesViewportInput() { return capture; },
            handleInput(data) { capture = data === "capture"; },
            render() { return [capture ? "captured" : "released"]; },
        };
    });
    pi.registerMessageRenderer("no-capture", () => ({ render() { return ["plain"]; } }));
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn bundled_select_lists_page_and_capture_the_viewport() {
    let h = harness(Setup { files: vec![("capture.ts", SOURCE.into())], load: vec!["capture.ts"], ..Setup::default() })
        .await;
    tokio::task::block_in_place(|| {
        let message = json!({"role": "custom", "customType": "page-list", "content": "", "display": true});
        let mut list = h.runner.render_message("page-list", &message, false, 0).unwrap();
        assert!(list.captures_viewport_input());
        for (key, selected) in [("\x1b[6~", 5), ("\x1b[6~", 10), ("\x1b[6~", 11), ("\x1b[5~", 6)] {
            list.handle_input(key);
            let lines = list.render(40);
            let expected = format!("→ item-{selected}");
            assert!(lines.iter().any(|line| line.starts_with(&expected)), "{lines:?}");
        }
    });
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}

#[tokio::test(flavor = "multi_thread")]
async fn component_viewport_capture_is_queried_after_each_input() {
    let h = harness(Setup { files: vec![("capture.ts", SOURCE.into())], load: vec!["capture.ts"], ..Setup::default() })
        .await;
    tokio::task::block_in_place(|| {
        let message = json!({"role": "custom", "customType": "dynamic-capture", "content": "", "display": true});
        let mut component = h.runner.render_message("dynamic-capture", &message, false, 0).unwrap();
        assert!(!component.captures_viewport_input());
        component.handle_input("capture");
        assert!(component.captures_viewport_input());
        assert_eq!(component.render(40), ["captured"]);
        component.handle_input("release");
        assert!(!component.captures_viewport_input());
        assert_eq!(component.render(40), ["released"]);
        let plain = h.runner.render_message("no-capture", &message, false, 0).unwrap();
        assert!(!plain.captures_viewport_input());
    });
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}
