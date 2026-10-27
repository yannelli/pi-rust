//! Port of upstream `test/harness-registry.test.ts`; `harness_registry/wraps.rs` holds the wrapper, section, and
//! hook cases.

#[path = "harness_registry/wraps.rs"]
mod wraps;

use std::sync::{Arc, Mutex};

use pi_chord::Json;
use pi_durable::DurableError;
use pi_durable::harness::agent::resolve_agent;
use pi_durable::harness::*;
use serde_json::json;

fn tool(name: &str) -> ToolRegistration {
    define_tool(name, format!("{name} tool"), json!({ "type": "object" }), |_, _, _| async {
        Ok(ToolExecutionResult { output: Some(Vec::new()), ..ToolExecutionResult::default() })
    })
}

fn task(name: &str, version: u32) -> Task {
    define_task(
        TaskDefinition::new(name, version, |_| Ok(Json::from(json!({ "phase": "run" }))))
            .phase("run", |_, _, _| async { Ok(()) })
            .abort(|_, _, _| async { Ok(()) }),
    )
}

fn names(extensions: &[Arc<Extension>]) -> Vec<String> {
    extensions.iter().map(|extension| extension.name.clone()).collect()
}

fn text_section(key: &str, text: &str) -> Arc<PromptSection> {
    static_section(key, text)
}

fn resolve(state: Option<&AgentState>, snapshot: &RegistrySnapshot, settings: &HarnessSettings) -> Agent {
    let reports = Mutex::new(Vec::<DurableError>::new());
    resolve_agent(state, snapshot, &resolve_settings(Some(settings)), &|error, _| {
        reports.lock().unwrap().push(error.clone())
    })
}

#[test]
fn installs_replaces_in_place_and_uninstalls_extensions_by_name() {
    let registry = create_registry();
    let published = Arc::new(Mutex::new(Vec::new()));
    let (sink, reader) = (published.clone(), registry.clone());
    let _unsubscribe = registry.subscribe(Arc::new(move || {
        sink.lock().unwrap().push(names(reader.snapshot().installed()).join(","));
    }));
    let a = Extension::new("a").tool(tool("read")).into_arc();
    let b = Extension::new("b").tool(tool("read")).tool(tool("bash")).into_arc();
    registry.install(a.clone()).unwrap();
    registry.install(b).unwrap();
    let before = registry.snapshot();
    // A new object with an installed name replaces it at its position.
    let a2 = Extension::new("a").tool(tool("grep")).into_arc();
    registry.install(a2.clone()).unwrap();
    assert_eq!(names(registry.snapshot().installed()), ["a", "b"]);
    assert!(Arc::ptr_eq(&registry.snapshot().extension("a").unwrap(), &a2));
    let tools: Vec<String> = registry
        .snapshot()
        .tools()
        .iter()
        .map(|(extension, tool)| format!("{}@{}", tool.name(), extension.name))
        .collect();
    assert_eq!(tools, ["grep@a", "read@b", "bash@b"]);
    // Old snapshots stay as they were.
    assert!(Arc::ptr_eq(&before.extension("a").unwrap(), &a));
    // Uninstall matches the name; a later install appends.
    registry.uninstall("a").unwrap();
    registry.uninstall("a").unwrap();
    assert_eq!(names(registry.snapshot().installed()), ["b"]);
    registry.install(a).unwrap();
    assert_eq!(names(registry.snapshot().installed()), ["b", "a"]);
    assert_eq!(*published.lock().unwrap(), ["a", "a,b", "a,b", "b", "b,a"]);
}

#[test]
fn validates_the_registry_as_it_would_be_after_an_install_and_publishes_nothing_when_invalid() {
    let registry = create_registry();
    registry.install(Extension::new("tasks").task(task("app.index", 1))).unwrap();
    let published = Arc::new(Mutex::new(0));
    let sink = published.clone();
    let _unsubscribe = registry.subscribe(Arc::new(move || *sink.lock().unwrap() += 1));
    let before = registry.snapshot();
    let invalid = [
        (Extension::new("x").tool(tool("read")).tool(tool("read")), "two tools named read"),
        (Extension::new("x").section(text_section("a", "1")).section(text_section("a", "2")), "two sections"),
        (Extension::new("x").section(text_section("Bad Key", "")), "must match"),
        (Extension::new("x").section(text_section("instructions", "")), "reserved"),
        (Extension::new("x").task(task("pi.generation", 1)), "already installed"),
        (Extension::new("x").task(task("app.index", 1)), "already installed"),
    ];
    for (extension, message) in invalid {
        let error = registry.install(extension).err().unwrap().to_string();
        assert!(error.contains(message), "{error} lacks {message}");
    }
    assert!(registry.snapshot().ptr_eq(&before));
    assert_eq!(*published.lock().unwrap(), 0);
    // Replacing the extension that holds a task name is valid: the check runs on the state after replacement.
    registry.install(Extension::new("tasks").task(task("app.index", 2))).unwrap();
    assert_eq!(registry.snapshot().task("app.index").unwrap().version, 2);
}

#[test]
fn always_holds_the_built_in_tasks_which_are_not_an_extension() {
    let registry = create_registry();
    assert!(registry.snapshot().installed().is_empty());
    let task_names = |registry: &Registry| -> Vec<String> {
        registry.snapshot().tasks().iter().map(|task| task.name.clone()).collect()
    };
    assert_eq!(task_names(&registry), ["pi.generation", "pi.tool", "pi.compaction"]);
    let custom = task("app.custom", 1);
    registry.install(Extension::new("custom").task(custom.clone())).unwrap();
    assert_eq!(task_names(&registry), ["pi.generation", "pi.tool", "pi.compaction", "app.custom"]);
    assert!(Arc::ptr_eq(&registry.snapshot().task("app.custom").unwrap(), &custom));
    registry.uninstall("custom").unwrap();
    assert!(registry.snapshot().task("app.custom").is_none());
}

#[test]
fn keeps_the_default_of_a_progress_interval_given_as_none() {
    let settings = HarnessSettings {
        progress: Some(ProgressOverrides { output_interval_ms: Some(250), ..Default::default() }),
        ..HarnessSettings::default()
    };
    let progress = resolve_settings(Some(&settings)).progress;
    assert_eq!((progress.partial_interval_ms, progress.output_interval_ms), (100, 250));
}

fn coding_registry() -> (Registry, [Arc<Extension>; 3]) {
    let coding = Extension::new("coding")
        .tool(tool("read"))
        .tool(tool("bash"))
        .tool(tool("edit"))
        .section(section("preamble", |_, _| async { Ok(Some("You code.".to_owned())) }, Some(false)))
        .section(text_section("cwd", "/repo"))
        .into_arc();
    let skills = Extension::new("skills").section(text_section("skills", "S")).into_arc();
    let reviewer = Extension::new("reviewer").section(text_section("role", "Review.")).into_arc();
    let registry = create_registry();
    for extension in [&coding, &skills, &reviewer] {
        registry.install(extension.clone()).unwrap();
    }
    (registry, [coding, skills, reviewer])
}

#[test]
fn selects_the_default_an_array_or_the_default_edited_by_add_and_remove() {
    let (registry, [coding, skills, _]) = coding_registry();
    let snapshot = registry.snapshot();
    let none = HarnessSettings::default();
    assert_eq!(names(&resolve(None, &snapshot, &none).extensions), ["coding", "skills", "reviewer"]);
    let settings =
        HarnessSettings { extensions: Some(vec![coding.clone(), skills.clone()]), ..HarnessSettings::default() };
    assert_eq!(names(&resolve(Some(&AgentState::default()), &snapshot, &settings).extensions), ["coding", "skills"]);
    let list = AgentState {
        extensions: Some(ExtensionSelection::List(vec!["reviewer".into(), "coding".into()])),
        ..AgentState::default()
    };
    assert_eq!(names(&resolve(Some(&list), &snapshot, &settings).extensions), ["reviewer", "coding"]);
    // Add appends, remove drops, duplicates keep their first position, uninstalled names are skipped.
    let edited = AgentState {
        extensions: Some(ExtensionSelection::Edit {
            add: Some(vec!["reviewer".into(), "coding".into(), "gone".into()]),
            remove: Some(vec!["skills".into()]),
        }),
        ..AgentState::default()
    };
    assert_eq!(names(&resolve(Some(&edited), &snapshot, &settings).extensions), ["coding", "reviewer"]);
    // An old object stands for its name: the installed extension is selected.
    let stale = HarnessSettings {
        extensions: Some(vec![Extension::new("skills").into_arc(), skills.clone()]),
        ..HarnessSettings::default()
    };
    let selected = resolve(Some(&AgentState::default()), &snapshot, &stale).extensions;
    assert_eq!(selected.len(), 1);
    assert!(Arc::ptr_eq(&selected[0], &skills));
}

#[test]
fn skips_uninstalled_names_and_resolves_them_again_once_they_are_installed() {
    let (_, [coding, skills, _]) = coding_registry();
    let local = create_registry();
    local.install(coding).unwrap();
    let state = AgentState {
        extensions: Some(ExtensionSelection::List(vec!["coding".into(), "skills".into()])),
        ..AgentState::default()
    };
    let none = HarnessSettings::default();
    assert_eq!(names(&resolve(Some(&state), &local.snapshot(), &none).extensions), ["coding"]);
    local.install(skills).unwrap();
    assert_eq!(names(&resolve(Some(&state), &local.snapshot(), &none).extensions), ["coding", "skills"]);
}
