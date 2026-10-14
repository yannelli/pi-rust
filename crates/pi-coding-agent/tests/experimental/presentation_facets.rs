//! Port of upstream `test/experimental-presentation-facets.test.ts`.

use std::path::{Path, PathBuf};

use pi_chord::node::read_facet_bundle_manifest;
use pi_chord::{BACKGROUND_CONTEXT, Json};
use pi_coding_agent::experimental::plugins::bundled::{
    create_presentation_facet_data, create_presentation_facet_loaders,
};
use pi_coding_agent::experimental::plugins::package::{
    create_server_plugin_package, restore_server_plugin_package_profile,
};
use pi_coding_agent::experimental::server_runtime::{StartServerOptions, start_server};
use pi_coding_agent::experimental::services::plugins::{
    PRESENTATION_PLUGINS, PrepareSessionRequest, PresentationPlugins, PresentationPluginsClient,
};
use pi_coding_agent::experimental::services::sessions::{
    SESSION_MANAGEMENT, SessionCreateOptions, SessionManagement, SessionManagementClient,
};

use super::remote_support::{connect, temp_dir};
use super::service_binding::{Errors, create_server_service_binding};

const TUI: &str = r#"import { defineFacet } from "@earendil-works/chord"; import { SlashCommands } from "@earendil-works/pi-coding-agent/experimental/plugin"; export default defineFacet({ id: "ID", setup(env) { env.use(SlashCommands); } });
"#;

async fn loaded_ids(data: &Json) -> Vec<String> {
    let mut ids = Vec::new();
    for loader in create_presentation_facet_loaders(data).unwrap() {
        let loaded = loader.load().await.unwrap();
        ids.extend(loaded.facets.iter().map(|facet| facet.id().to_string()));
        loaded.dispose().await.unwrap();
    }
    ids
}

fn write_package(path: &Path, name: &str, peers: &str, entry: &str, source: &str) {
    std::fs::create_dir_all(path.join("src")).unwrap();
    let package = format!("{{\"name\":\"{name}\",\"version\":\"1.0.0\",\"peerDependencies\":{{{peers}}}}}\n");
    std::fs::write(path.join("package.json"), package).unwrap();
    std::fs::write(path.join("src").join(entry), source).unwrap();
}

// "rejects local plugin paths for Radius servers" checks `openClientRuntime` (step 3).

#[tokio::test]
async fn restores_plugin_package_selections_for_later_server_generations() {
    let directory = temp_dir("pi-presentation-profile-");
    let server_id = pi_server::random_uuid();
    let paths: Vec<String> =
        ["first-plugin", "second-plugin"].map(|name| directory.path().join(name).display().to_string()).into();
    let directory = directory.path();
    assert_eq!(restore_server_plugin_package_profile(directory, &server_id, Some(&paths)).await.unwrap(), paths);
    assert_eq!(restore_server_plugin_package_profile(directory, &server_id, None).await.unwrap(), paths);
    assert!(restore_server_plugin_package_profile(directory, &server_id, Some(&[])).await.unwrap().is_empty());
    assert!(restore_server_plugin_package_profile(directory, &server_id, None).await.unwrap().is_empty());
}

#[tokio::test]
async fn builds_conventional_plugin_entries_into_the_server_owned_plugin_cache() {
    super::faux_worker::install();
    let directory = temp_dir("pi-presentation-package-");
    let server_id = pi_server::random_uuid();
    let package_path = directory.path().join("pi-example-plugin");
    let peers = r#""@earendil-works/chord":"^0.84.4","@earendil-works/pi-coding-agent":"^0.84.4""#;
    write_package(&package_path, "@earendil-works/test-plugin", peers, "tui.ts", &TUI.replace("ID", "built-a"));
    let plugin = create_server_plugin_package(directory.path(), &server_id, &package_path.display().to_string());

    let first = plugin.build().await.unwrap();
    assert_eq!(first.len(), 1);
    let pattern = format!("/plugin-builds/{server_id}/pi-example-plugin-[a-f0-9]{{12}}/chord-facets\\.json$");
    assert!(regex::Regex::new(&pattern).unwrap().is_match(&plugin.manifest_path.display().to_string()));
    assert_eq!(
        (first[0].plugin.id.as_str(), first[0].plugin.version.as_deref().unwrap()),
        ("@earendil-works/test-plugin", "1.0.0")
    );
    assert_eq!(loaded_ids(&create_presentation_facet_data(&first)).await, ["built-a"]);

    let source_path = package_path.join("src").join("tui.ts");
    std::fs::write(&source_path, TUI.replace("ID", "built-b")).unwrap();
    let second = plugin.build().await.unwrap();
    assert_ne!(second[0].source, first[0].source);
    assert_eq!(loaded_ids(&create_presentation_facet_data(&second)).await, ["built-b"]);

    let second_package = directory.path().join("second-plugin");
    let second_source = r#"import { defineFacet } from "@earendil-works/chord"; export default defineFacet({ id: "second-built", setup() {} });
"#;
    write_package(
        &second_package,
        "@earendil-works/second-test-plugin",
        r#""@earendil-works/chord":"^0.84.4""#,
        "tui.ts",
        second_source,
    );
    let server_directory = directory.path().join("server");
    let running = start_server(StartServerOptions {
        directory: Some(server_directory.display().to_string()),
        session_dir: Some(directory.path().join("sessions").display().to_string()),
        ..StartServerOptions::default()
    })
    .await
    .unwrap();
    // Upstream drives this through `openClientRuntime` (step 3); the same server services are bound here.
    let client = connect(&running.server_id, &running.socket_path).await;
    let errors = Errors::default();
    let services = create_server_service_binding(&client, &[&SESSION_MANAGEMENT, &PRESENTATION_PLUGINS], &errors);
    let management = SessionManagementClient(services.use_service(&SESSION_MANAGEMENT));
    let plugins = PresentationPluginsClient(services.use_service(&PRESENTATION_PLUGINS));
    services.ready(&BACKGROUND_CONTEXT).await.unwrap();
    let session_id = pi_server::random_uuid();
    management.create(SessionCreateOptions { id: Some(session_id.clone()) }, &BACKGROUND_CONTEXT).await.unwrap();
    let package_paths = vec![package_path.display().to_string(), second_package.display().to_string()];
    let request = PrepareSessionRequest { session_id, package_paths: Some(package_paths) };
    let data = plugins.prepare_session(request, &BACKGROUND_CONTEXT).await.unwrap();
    assert!(
        restore_server_plugin_package_profile(&server_directory, &running.server_id, None).await.unwrap().is_empty()
    );
    assert_eq!(loaded_ids(&data).await, ["built-b", "second-built"]);

    std::fs::write(&source_path, TUI.replace("ID", "built-c")).unwrap();
    let reloaded = plugins.reload(&BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(loaded_ids(&reloaded).await, ["built-c", "second-built"]);
    services.dispose(&BACKGROUND_CONTEXT).await.unwrap();
    client.dispose();
    running.close().await.unwrap();
}

#[tokio::test]
async fn builds_the_example_plugin_package_without_a_package_owned_build_script() {
    let directory = temp_dir("pi-example-plugin-");
    let server_id = pi_server::random_uuid();
    let package_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/plugins/pi-example-plugin");
    let plugin = create_server_plugin_package(directory.path(), &server_id, &package_path.display().to_string());
    let artifacts = plugin.build().await.unwrap();
    let manifest = read_facet_bundle_manifest(&plugin.manifest_path).await.unwrap();
    assert_eq!(
        (manifest.plugin.id.as_str(), manifest.plugin.version.as_deref().unwrap()),
        ("@earendil-works/pi-example-plugin", "1.0.0")
    );
    assert_eq!(manifest.entries.keys().collect::<Vec<_>>(), ["session", "tui"]);
    let ids = loaded_ids(&create_presentation_facet_data(&artifacts)).await;
    assert_eq!(ids, ["@earendil-works/pi-example-plugin/tui"]);
}
