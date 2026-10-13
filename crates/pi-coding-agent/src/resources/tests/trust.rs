//! Port of upstream `test/trust-manager.test.ts`, plus the `resolveProjectTrusted` decision order.

use std::path::Path;

use super::write;
use crate::resources::paths::canonicalize;
use crate::resources::trust::{
    DefaultProjectTrust, ExtensionTrustDecision, ProjectTrustRequest, ProjectTrustResolution, ProjectTrustStore,
    apply_project_trust_choice, format_saved_decision, has_trust_requiring_project_resources, project_trust_options,
    resolve_project_trust, saved_option_index,
};

#[test]
fn stores_decisions_and_inherits_from_parents() {
    let temp = tempfile::tempdir().unwrap();
    let store = ProjectTrustStore::new(&temp.path().join("agent"));
    let parent = temp.path().join("trusted-parent");
    let child = parent.join("project");
    std::fs::create_dir_all(&child).unwrap();

    assert_eq!(store.get(&child).unwrap(), None);
    store.set(&parent, Some(true)).unwrap();
    assert_eq!(store.get(&child).unwrap(), Some(true));
    store.set(&child, Some(false)).unwrap();
    assert_eq!(store.get(&child).unwrap(), Some(false));
    store.set(&child, None).unwrap();
    assert_eq!(store.get(&child).unwrap(), Some(true));
    let entry = store.get_entry(&child).unwrap().unwrap();
    assert_eq!(entry.path, canonicalize(&parent));

    let text = std::fs::read_to_string(store.path()).unwrap();
    let key = serde_json::to_string(&canonicalize(&parent).display().to_string()).unwrap();
    let expected = format!("{{\n  {key}: true\n}}\n");
    assert_eq!(text, expected);
    assert!(!store.path().with_file_name("trust.json.lock").exists());
}

#[test]
fn rejects_invalid_trust_files() {
    let temp = tempfile::tempdir().unwrap();
    let store = ProjectTrustStore::new(temp.path());
    write(store.path(), "[]");
    assert!(store.get(temp.path()).unwrap_err().to_string().contains("expected an object"));
    write(store.path(), "{\"/x\": 1}");
    assert!(store.get(temp.path()).unwrap_err().to_string().contains("must be true, false, or null"));
    write(store.path(), "{\"/x\": null}");
    assert_eq!(store.get(Path::new("/x")).unwrap(), None);
}

#[cfg(unix)]
#[test]
fn stale_locks_are_replaced_and_held_locks_fail() {
    let temp = tempfile::tempdir().unwrap();
    let store = ProjectTrustStore::new(temp.path());
    let lock = temp.path().join("trust.json.lock");
    std::fs::create_dir_all(&lock).unwrap();
    assert!(store.get(temp.path()).unwrap_err().to_string().contains("held by another process"));
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    std::fs::File::open(&lock).unwrap().set_modified(old).unwrap();
    assert_eq!(store.get(temp.path()).unwrap(), None);
    assert!(!lock.exists());
}

#[test]
fn detects_trust_requiring_project_resources() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let cwd = home.join("project");
    std::fs::create_dir_all(home.join(".pi/agent")).unwrap();
    std::fs::create_dir_all(home.join(".agents/skills")).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    assert!(!has_trust_requiring_project_resources(home, home));
    assert!(!has_trust_requiring_project_resources(&cwd, home));

    write(&home.join(".pi/settings.json"), "{}");
    assert!(has_trust_requiring_project_resources(home, home));
    std::fs::remove_file(home.join(".pi/settings.json")).unwrap();

    std::fs::create_dir_all(cwd.join(".pi")).unwrap();
    assert!(!has_trust_requiring_project_resources(&cwd, home), "a bare .pi needs no trust");
    for entry in ["mcp.json", "SYSTEM.md", "APPEND_SYSTEM.md"] {
        write(&cwd.join(".pi").join(entry), "x");
        assert!(has_trust_requiring_project_resources(&cwd, home), "{entry}");
        std::fs::remove_file(cwd.join(".pi").join(entry)).unwrap();
    }
    std::fs::create_dir_all(cwd.join(".pi/prompts")).unwrap();
    assert!(has_trust_requiring_project_resources(&cwd, home));
    std::fs::remove_dir_all(cwd.join(".pi")).unwrap();

    std::fs::create_dir_all(cwd.join(".agents/skills")).unwrap();
    assert!(has_trust_requiring_project_resources(&cwd, home));
    assert!(has_trust_requiring_project_resources(&cwd.join("nested"), home));
}

#[test]
fn options_match_the_upstream_prompt() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = canonicalize(temp.path());
    let parent = cwd.parent().unwrap().to_path_buf();
    let labels: Vec<String> = project_trust_options(&cwd, true).into_iter().map(|option| option.label).collect();
    let parent_label = format!("Trust parent folder ({})", parent.display());
    assert_eq!(
        labels,
        [
            "Trust",
            parent_label.as_str(),
            "Trust (this session only)",
            "Do not trust",
            "Do not trust (this session only)"
        ]
    );
    let options = project_trust_options(&cwd, false);
    assert_eq!(options.len(), 3);
    assert_eq!(options[1].updates[1].decision, None);

    let store = ProjectTrustStore::new(&temp.path().join("agent"));
    assert_eq!(format_saved_decision(Some(&cwd), None), "none");
    assert!(apply_project_trust_choice(&store, Some(&options[1])).unwrap());
    let saved = store.get_entry(&cwd).unwrap();
    assert_eq!(saved_option_index(&options, saved.as_ref()), Some(1));
    let inherited = format!("trusted (inherited from {})", parent.display());
    assert_eq!(format_saved_decision(options[0].saved_path.as_deref(), saved.as_ref()), inherited);
    assert!(!apply_project_trust_choice(&store, Some(&options[2])).unwrap());
    assert_eq!(store.get(&cwd).unwrap(), Some(false));
    assert!(!apply_project_trust_choice(&store, None).unwrap());
}

#[test]
fn resolves_trust_in_upstream_order() {
    let temp = tempfile::tempdir().unwrap();
    let (home, cwd) = (temp.path().join("home"), temp.path().join("project"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(cwd.join(".pi/skills")).unwrap();
    let store = ProjectTrustStore::new(&temp.path().join("agent"));
    let request = |default, has_ui| ProjectTrustRequest {
        cwd: &cwd,
        home: &home,
        trust_override: None,
        default_project_trust: default,
        extension_decision: None,
        has_ui,
    };
    let decided = |resolution: ProjectTrustResolution| match resolution {
        ProjectTrustResolution::Decided(trusted) => Some(trusted),
        ProjectTrustResolution::Prompt(_) => None,
    };
    let resolve = |request: &ProjectTrustRequest<'_>| decided(resolve_project_trust(&store, request).unwrap());

    assert_eq!(
        resolve(&ProjectTrustRequest { trust_override: Some(false), ..request(DefaultProjectTrust::Always, true) }),
        Some(false)
    );
    assert_eq!(resolve(&request(DefaultProjectTrust::Always, false)), Some(true));
    assert_eq!(resolve(&request(DefaultProjectTrust::Never, true)), Some(false));
    assert_eq!(resolve(&request(DefaultProjectTrust::Ask, false)), Some(false));
    let ProjectTrustResolution::Prompt(prompt) =
        resolve_project_trust(&store, &request(DefaultProjectTrust::Ask, true)).unwrap()
    else {
        panic!("expected a prompt");
    };
    assert!(prompt.message.starts_with("Trust project folder?\n"));
    assert_eq!(prompt.options.len(), 5);

    let extension = ExtensionTrustDecision { trusted: true, remember: true };
    let with_extension =
        ProjectTrustRequest { extension_decision: Some(extension), ..request(DefaultProjectTrust::Never, false) };
    assert_eq!(resolve(&with_extension), Some(true));
    assert_eq!(store.get(&cwd).unwrap(), Some(true), "remembered extension decisions are saved");
    assert_eq!(resolve(&request(DefaultProjectTrust::Never, false)), Some(true), "saved decisions beat the default");

    std::fs::remove_dir_all(cwd.join(".pi")).unwrap();
    store.set(&cwd, Some(false)).unwrap();
    assert_eq!(resolve(&request(DefaultProjectTrust::Never, false)), Some(true), "nothing to protect");
    assert_eq!(DefaultProjectTrust::parse("always"), DefaultProjectTrust::Always);
    assert_eq!(DefaultProjectTrust::parse("bogus"), DefaultProjectTrust::Ask);
}
