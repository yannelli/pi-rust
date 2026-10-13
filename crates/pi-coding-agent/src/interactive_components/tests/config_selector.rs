//! Config selector (`pi config`) on a virtual terminal.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;

use crate::interactive_components::config_selector::ConfigSelector;
use crate::interactive_components::test_support::{Calls, Harness, keys, setup_with};
use crate::packages::{ConfigEditor, ConfigWriteScope, ScopedResolvedPaths};
use crate::resources::{PathMetadata, ResolvedPaths, ResolvedResource, SourceOrigin, SourceScope};
use crate::settings::{SettingsManager, SettingsOptions};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    cwd: PathBuf,
    agent_dir: PathBuf,
    settings: Arc<SettingsManager>,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::resources::paths::canonicalize(temp.path());
        let (cwd, agent_dir) = (root.join("project"), root.join("agent"));
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();
        let settings = json!({"packages": ["npm:pi-tools"]}).to_string();
        std::fs::write(agent_dir.join("settings.json"), settings).unwrap();
        let settings = Arc::new(SettingsManager::create(&cwd, &agent_dir, SettingsOptions { project_trusted: true }));
        Self { _temp: temp, root, cwd, agent_dir, settings }
    }

    fn resolved(&self, extra_prompts: usize) -> ResolvedPaths {
        let resource = |path: PathBuf, enabled: bool, metadata: &PathMetadata| ResolvedResource {
            path,
            enabled,
            metadata: metadata.clone(),
        };
        let mut package = PathMetadata::new("npm:pi-tools", SourceScope::User, SourceOrigin::Package);
        let package_root = self.root.join("pkg");
        package.base_dir = Some(package_root.clone());
        let user = PathMetadata::new("auto", SourceScope::User, SourceOrigin::TopLevel);
        let extensions =
            ["a.ts", "b.ts"].map(|name| resource(package_root.join("extensions").join(name), true, &package));
        let mut prompts = vec![resource(self.agent_dir.join("prompts/review.md"), false, &user)];
        prompts
            .extend((0..extra_prompts).map(|n| resource(self.agent_dir.join(format!("prompts/p{n}.md")), true, &user)));
        ResolvedPaths {
            extensions: extensions.to_vec(),
            skills: vec![resource(self.agent_dir.join("skills/foo/SKILL.md"), true, &user)],
            prompts,
            themes: Vec::new(),
        }
    }

    fn editor(&self, scope: ConfigWriteScope, extra_prompts: usize) -> ConfigEditor {
        let resolved = self.resolved(extra_prompts);
        let scoped = ScopedResolvedPaths { global: resolved.clone(), project: resolved };
        ConfigEditor::new(&scoped, self.settings.clone(), &self.cwd, &self.agent_dir, &self.root, scope)
    }
}

struct Callbacks {
    closed: Calls<()>,
    exited: Calls<()>,
    renders: Calls<()>,
}

fn mount(
    fixture: &Fixture,
    scope: ConfigWriteScope,
    project_mode: bool,
    rows: usize,
) -> (Harness<ConfigSelector>, Callbacks) {
    let callbacks = Callbacks { closed: Calls::new(), exited: Calls::new(), renders: Calls::new() };
    let editor = fixture.editor(scope, if rows < 20 { 6 } else { 0 });
    let (close, exit, render) = (callbacks.closed.counter(), callbacks.exited.counter(), callbacks.renders.counter());
    let harness =
        Harness::new(100, rows, move |_| ConfigSelector::new(editor, close, exit, render, Some(rows), project_mode));
    (harness, callbacks)
}

fn row<'a>(screen: &'a str, needle: &str) -> &'a str {
    screen.lines().find(|line| line.contains(needle)).unwrap_or_else(|| panic!("{needle} in\n{screen}"))
}

#[test]
fn renders_groups_and_items_in_global_mode() {
    let fixture = Fixture::new();
    let (harness, _) = mount(&fixture, ConfigWriteScope::Global, true, 30);
    let screen = harness.screen();
    let header = row(&screen, "Global Resources");
    assert!(header.ends_with("tab switch mode · space toggle · esc close"), "{header}");
    assert!(screen.contains("~/.pi/agent/settings.json"), "{screen}");
    let lines: Vec<&str> = screen.lines().collect();
    let start = lines.iter().position(|line| *line == "  npm:pi-tools (user)").expect(&screen);
    assert_eq!(
        lines[start..start + 9],
        [
            "  npm:pi-tools (user)",
            "    Extensions",
            ">     [x] a.ts",
            "      [x] b.ts",
            "  User (~/agent/)",
            "    Skills",
            "      [x] foo",
            "    Prompts",
            "      [ ] review.md",
        ]
    );
    assert!(lines[1].starts_with("────"), "{screen}");
    assert!(!screen.contains("inherited global"), "{screen}");
}

#[test]
fn navigation_skips_headers_and_pages() {
    let fixture = Fixture::new();
    let (harness, _) = mount(&fixture, ConfigWriteScope::Global, true, 30);
    harness.send(keys::DOWN);
    assert!(harness.screen().contains(">     [x] b.ts"));
    harness.send(keys::DOWN);
    assert!(harness.screen().contains(">     [x] foo"));
    harness.send(keys::DOWN);
    harness.send(keys::DOWN);
    assert!(harness.screen().contains(">     [ ] review.md"), "{}", harness.screen());
    harness.send(keys::PAGE_UP);
    assert!(harness.screen().contains(">     [x] a.ts"), "{}", harness.screen());
    harness.send(keys::PAGE_DOWN);
    assert!(harness.screen().contains(">     [ ] review.md"), "{}", harness.screen());
    harness.send(keys::UP);
    assert!(harness.screen().contains(">     [x] foo"));
}

#[test]
fn toggles_resources_and_writes_global_settings() {
    let fixture = Fixture::new();
    let (harness, callbacks) = mount(&fixture, ConfigWriteScope::Global, true, 30);
    harness.send(keys::SPACE);
    assert!(harness.screen().contains(">     [ ] a.ts"), "{}", harness.screen());
    let packages = serde_json::to_value(fixture.settings.global_settings().packages.unwrap()).unwrap();
    assert_eq!(packages, json!([{"source": "npm:pi-tools", "extensions": ["-extensions/a.ts"]}]));
    harness.send_all(&[keys::DOWN, keys::DOWN, keys::DOWN, keys::ENTER]);
    assert!(harness.screen().contains(">     [x] review.md"), "{}", harness.screen());
    assert_eq!(fixture.settings.global_settings().prompts, Some(vec!["+prompts/review.md".to_string()]));
    assert_eq!(callbacks.renders.len(), 2);
    assert!(harness.component.borrow().with_editor(|editor| editor.groups()[1].subgroups[1].items[0].enabled));
}

#[test]
fn tab_switches_between_global_and_project_mode() {
    let fixture = Fixture::new();
    let (harness, callbacks) = mount(&fixture, ConfigWriteScope::Global, true, 30);
    harness.send(keys::TAB);
    assert_eq!(harness.component.borrow().write_scope(), ConfigWriteScope::Project);
    let screen = harness.screen();
    let header = row(&screen, "Project Local Resources");
    assert!(header.ends_with("tab switch mode · space cycle inherit/+/- · esc close"), "{header}");
    assert!(screen.contains(".pi/settings.json · inherited global resources are dimmed"), "{screen}");
    assert!(screen.contains("  npm:pi-tools (user) · inherited global"), "{screen}");
    assert!(screen.contains(">     [x] a.ts  inherited global"), "{screen}");

    harness.send(keys::SPACE);
    let screen = harness.screen();
    assert!(screen.contains(">     [-] a.ts  project unload"), "{screen}");
    let project = serde_json::to_value(fixture.settings.project_settings().packages.unwrap()).unwrap();
    assert_eq!(project, json!([{"source": "npm:pi-tools", "autoload": false, "extensions": ["-extensions/a.ts"]}]));
    harness.send(keys::SPACE);
    assert!(harness.screen().contains(">     [+] a.ts  project load"), "{}", harness.screen());

    harness.send(keys::TAB);
    assert_eq!(harness.component.borrow().write_scope(), ConfigWriteScope::Global);
    let screen = harness.screen();
    assert!(screen.contains("Global Resources") && screen.contains(">     [x] a.ts"), "{screen}");
    assert_eq!(callbacks.renders.len(), 4);
}

#[test]
fn tab_does_nothing_without_project_mode() {
    let fixture = Fixture::new();
    let (harness, callbacks) = mount(&fixture, ConfigWriteScope::Global, false, 30);
    let header = row(&harness.screen(), "Global Resources").to_string();
    assert!(header.ends_with("space toggle · esc close") && !header.contains("switch mode"), "{header}");
    harness.send(keys::TAB);
    assert_eq!(harness.component.borrow().write_scope(), ConfigWriteScope::Global);
    assert!(callbacks.renders.is_empty());
    assert!(!harness.screen().contains("Project Local Resources"));
}

#[test]
fn starts_in_project_mode_with_local_flag() {
    let fixture = Fixture::new();
    let (harness, _) = mount(&fixture, ConfigWriteScope::Project, true, 30);
    let screen = harness.screen();
    assert!(screen.contains("Project Local Resources"), "{screen}");
    assert!(screen.contains("      [ ] review.md  inherited global"), "{screen}");
}

#[test]
fn typing_filters_by_name_type_and_path() {
    let fixture = Fixture::new();
    let (harness, _) = mount(&fixture, ConfigWriteScope::Global, true, 30);
    harness.type_text("review");
    let screen = harness.screen();
    assert!(screen.contains(">     [ ] review.md"), "{screen}");
    assert!(screen.contains("  User (~/agent/)") && screen.contains("    Prompts"), "{screen}");
    assert!(!screen.contains("a.ts") && !screen.contains("Skills"), "{screen}");
    harness.send(keys::SPACE);
    assert!(harness.screen().contains(">     [x] review.md"), "{}", harness.screen());
    for _ in 0.."review".len() {
        harness.send(keys::BACKSPACE);
    }
    harness.type_text("skills");
    let screen = harness.screen();
    assert!(screen.contains(">     [x] foo") && !screen.contains("review.md"), "{screen}");
    harness.type_text("zzz");
    assert!(harness.screen().contains("  No resources found"), "{}", harness.screen());
}

#[test]
fn escape_closes_and_unclaimed_ctrl_c_exits() {
    let fixture = Fixture::new();
    let (harness, callbacks) = mount(&fixture, ConfigWriteScope::Global, true, 30);
    harness.send(&keys::ctrl('c'));
    assert_eq!((callbacks.closed.len(), callbacks.exited.len()), (1, 0));
    harness.send(keys::ESCAPE);
    assert_eq!(callbacks.closed.len(), 2);
    drop(harness);

    let _kb = setup_with(&[("tui.select.cancel", "escape")]);
    let (harness, callbacks) = mount(&fixture, ConfigWriteScope::Global, true, 30);
    harness.send(&keys::ctrl('c'));
    assert_eq!((callbacks.closed.len(), callbacks.exited.len()), (0, 1));
}

#[test]
fn scrolls_long_lists_with_an_item_counter() {
    let fixture = Fixture::new();
    let (harness, _) = mount(&fixture, ConfigWriteScope::Global, true, 13);
    assert!(harness.screen().contains("  (1/10)"), "{}", harness.screen());
    harness.send(keys::PAGE_DOWN);
    let screen = harness.screen();
    assert!(screen.contains("  (3/10)") && screen.contains(">     [x] foo"), "{screen}");
    assert!(!screen.contains("npm:pi-tools"), "{screen}");
}
