//! The standalone interactive components of `@earendil-works/pi-coding-agent` are upstream code:
//! extensions build, render, resize, and drive them as upstream does under Node.

#[path = "extensions/harness.rs"]
#[allow(dead_code)]
mod harness;
#[path = "offline_guard/mod.rs"]
mod offline_guard;

use harness::{Setup, harness};
use serde_json::Value;

/// Upstream output of the scenario, written by `node scripts/component-scenario.mjs <upstream>`.
const EXPECTED: &str = include_str!("fixtures/extensions/component_scenario.json");

const EXTENSION: &str = r#"
import * as ca from '@earendil-works/pi-coding-agent';
import * as legacy from '@mariozechner/pi-coding-agent';
import { scenario } from './component_scenario.mjs';

const NAMES = ['ArminComponent', 'AssistantMessageComponent', 'BashExecutionComponent', 'BranchSummaryMessageComponent',
  'CompactionSummaryMessageComponent', 'CustomMessageComponent', 'ExtensionInputComponent', 'ExtensionSelectorComponent',
  'OAuthSelectorComponent', 'ShowImagesSelectorComponent', 'SkillInvocationMessageComponent', 'ThinkingSelectorComponent',
  'UserMessageComponent', 'UserMessageSelectorComponent'];

export default function (pi) {
  pi.registerCommand('components', { handler: async () => {
    const shared = NAMES.filter((name) => typeof ca[name] !== 'function' || ca[name] !== legacy[name]);
    pi.appendEntry('components', { scenario: scenario(ca), shared });
  }});
}
"#;

/// The `components` entry an extension in `files` appends for `/components`.
async fn components_entry(files: Vec<(&'static str, String)>) -> Value {
    let h = harness(Setup { files, load: vec!["components.ts"], ..Setup::default() }).await;
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let prompt = h.session.prompt("/components", Vec::new());
    tokio::time::timeout(std::time::Duration::from_secs(20), prompt).await.unwrap().unwrap();
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let entry = h.entries().into_iter().find(|entry| entry["customType"] == "components").expect("components entry");
    entry["data"].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_components_render_and_handle_input_as_upstream() {
    let files = vec![
        ("components.ts", EXTENSION.to_string()),
        ("component_scenario.mjs", include_str!("fixtures/extensions/component_scenario.mjs").to_string()),
    ];
    let data = &components_entry(files).await;
    assert_eq!(data["shared"], Value::Array(Vec::new()), "{data}");
    let expected: Value = serde_json::from_str(EXPECTED).unwrap();
    assert_eq!(data["scenario"]["events"], expected["events"]);
    assert_eq!(data["scenario"]["armin"], expected["armin"]);
    for (name, rendered) in expected["rendered"].as_object().unwrap() {
        assert_eq!(&data["scenario"]["rendered"][name], rendered, "{name}");
    }
}

const EXTENSION_B: &str = r#"
import * as ca from '@earendil-works/pi-coding-agent';
import { getKeybindings } from '@earendil-works/pi-tui';
import { scenario } from './component_scenario_b.mjs';

export default function (pi) {
  pi.registerCommand('components', { handler: async () => {
    pi.appendEntry('components', { scenario: await scenario({ ...ca, getKeybindings }), shared: [] });
  }});
}
"#;

/// pi-rust ships more built-in themes than upstream (`docs/parity/allowlist.json`), so the theme
/// selector lists the Rust registry and its Down key from `dark` picks the next name there.
fn with_rust_themes(expected: &mut Value) {
    let names = pi_coding_agent::theme::get_available_themes();
    let next = names[names.iter().position(|name| name == "dark").unwrap() + 1].clone();
    for event in expected["events"].as_array_mut().unwrap() {
        if event[0] == "preview" || event[0] == "theme" {
            event[1] = Value::String(next.clone());
        }
    }
    // SelectList pads names to `clamp(longest + 2, 12, 32)` and shows "(current)" above 40 columns.
    let column = (names.iter().map(|name| name.chars().count()).max().unwrap_or(0) + 2).clamp(12, 32);
    for (width, current) in [("narrow", false), ("wide", true)] {
        let row = |name: &String| match (name == "dark", current) {
            (true, true) => format!("→ {name:<column$}(current)"),
            (true, false) => format!("→ {name}"),
            _ => format!("  {name}"),
        };
        let rendered = expected["rendered"]["themes"][width].as_array_mut().unwrap();
        let border = rendered[0].clone();
        let rows = names.iter().map(|name| Value::String(row(name)));
        *rendered = std::iter::once(border.clone()).chain(rows).chain([border]).collect();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn record_driven_selectors_and_the_login_dialog_match_upstream() {
    let files = vec![
        ("components.ts", EXTENSION_B.to_string()),
        ("component_scenario_b.mjs", include_str!("fixtures/extensions/component_scenario_b.mjs").to_string()),
    ];
    let mut expected: Value =
        serde_json::from_str(include_str!("fixtures/extensions/component_scenario_b.json")).unwrap();
    with_rust_themes(&mut expected);
    let data = components_entry(files).await;
    assert_eq!(data["scenario"]["events"], expected["events"]);
    for (name, rendered) in expected["rendered"].as_object().unwrap() {
        assert_eq!(&data["scenario"]["rendered"][name], rendered, "{name}");
    }
}

const EXTENSION_C: &str = r#"
import * as ca from '@earendil-works/pi-coding-agent';
import { getKeybindings, resetCapabilitiesCache, setCapabilities } from '@earendil-works/pi-tui';
import { scenario } from './component_scenario_c.mjs';

const JPEG = '__JPEG__';

/** A JPEG result on a Kitty terminal: the tool output must carry PNG data from the Rust converter. */
function kittyImage() {
  setCapabilities({ images: 'kitty', trueColor: true, hyperlinks: false });
  try {
    const tui = { requestRender() {}, terminal: { rows: 24, columns: 80 } };
    const tool = new ca.ToolExecutionComponent('read', 'c2', { path: 'tiny.jpg' }, {}, undefined, tui, '/work');
    tool.updateResult({ content: [{ type: 'image', data: JPEG, mimeType: 'image/jpeg' }], isError: false });
    const output = tool.render(60).join('\n');
    return [output.includes('iVBORw0KGgo'), output.includes(JPEG.slice(0, 40))];
  } finally {
    resetCapabilitiesCache();
  }
}

export default function (pi) {
  pi.registerCommand('components', { handler: async () => {
    const classes = { ...ca, getKeybindings, resetCapabilitiesCache, setCapabilities };
    pi.appendEntry('components', { scenario: await scenario(classes), image: kittyImage() });
  }});
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_backed_components_match_upstream_and_convert_images_in_rust() {
    let jpeg = include_str!("fixtures/extensions/tiny_jpeg.b64").trim();
    let files = vec![
        ("components.ts", EXTENSION_C.replace("__JPEG__", jpeg)),
        ("component_scenario_c.mjs", include_str!("fixtures/extensions/component_scenario_c.mjs").to_string()),
    ];
    let expected: Value = serde_json::from_str(include_str!("fixtures/extensions/component_scenario_c.json")).unwrap();
    let data = components_entry(files).await;
    assert_eq!(data["scenario"]["events"], expected["events"]);
    for (name, rendered) in expected["rendered"].as_object().unwrap() {
        assert_eq!(&data["scenario"]["rendered"][name], rendered, "{name}");
    }
    assert_eq!(data["image"], serde_json::json!([true, false]), "{data}");
}
