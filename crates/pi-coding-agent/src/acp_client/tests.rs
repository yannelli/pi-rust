use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::json;

use super::install::{agents_root, install_with, uninstall};
use super::registry::{BinaryTarget, Distribution, PackageTarget, RegistryAgent, fetch_with, platform_key};
use super::{AcpAgentEntry, AcpScope, provider_id, store, with_builtins};
use crate::platform::test_http::{TestReply, TestServer, client};
use crate::settings::SettingsManager;

fn entry(id: &str, command: &str) -> AcpAgentEntry {
    serde_json::from_value(json!({ "id": id, "name": id, "command": command })).unwrap()
}

#[test]
fn provider_id_appends_acp_once() {
    assert_eq!(provider_id("kiro"), "kiro-acp");
    assert_eq!(provider_id("amp-acp"), "amp-acp");
}

#[test]
fn entry_defaults_and_camel_case() {
    let parsed = entry("kiro", "kiro-cli");
    assert!(parsed.enabled);
    assert_eq!(parsed.source, "custom");
    assert!(parsed.args.is_empty() && parsed.env.is_empty());
    let value = serde_json::to_value(AcpAgentEntry { install_dir: Some("/x".into()), ..parsed }).unwrap();
    assert_eq!(value["installDir"], "/x");
}

#[test]
fn store_round_trip_with_project_override() {
    let settings = SettingsManager::in_memory(
        json!({ "acpAgents": [{ "id": "broken" }, { "id": "a", "name": "A", "command": "a", "extra": 1 }] }),
    );
    let listed = store::list(&settings);
    assert_eq!(listed.len(), 1, "unparsed entries are skipped");
    assert_eq!(listed[0].1, AcpScope::Global);

    store::upsert(&settings, &entry("b", "b-cmd"), AcpScope::Global).unwrap();
    store::upsert(&settings, &entry("a", "project-a"), AcpScope::Project).unwrap();
    let listed = store::list(&settings);
    let ids: Vec<_> = listed.iter().map(|(entry, scope)| (entry.id.as_str(), entry.command.as_str(), *scope)).collect();
    assert_eq!(ids, vec![("a", "project-a", AcpScope::Project), ("b", "b-cmd", AcpScope::Global)]);

    store::set_enabled(&settings, "a", AcpScope::Global, false).unwrap();
    let global = settings.global_settings().extra["acpAgents"].clone();
    assert_eq!(global[0], json!({ "id": "broken" }), "unparsed entries are kept");
    assert_eq!(global[1]["enabled"], false);
    assert_eq!(global[1]["extra"], 1);
    assert!(store::set_enabled(&settings, "missing", AcpScope::Global, true).is_err());

    let removed = store::remove(&settings, "a", AcpScope::Project).unwrap();
    assert_eq!(removed.map(|entry| entry.command), Some("project-a".into()));
    let listed = store::list(&settings);
    assert_eq!(listed[0].0.command, "a");
    assert!(!listed[0].0.enabled);
    assert!(settings.project_settings().extra.get("acpAgents").is_none());
}

#[test]
fn project_write_fails_when_untrusted() {
    let settings = SettingsManager::in_memory(json!({}));
    settings.set_project_trusted(false);
    assert!(store::upsert(&settings, &entry("a", "a"), AcpScope::Project).is_err());
}

#[tokio::test]
async fn fetch_reads_agents_and_adds_builtins() {
    let server = TestServer::start(|_| {
        TestReply::json(&json!({ "version": "1", "extensions": [], "agents": [
            { "id": "gemini", "name": "Gemini", "version": "1.0.0", "authors": ["Google"],
              "distribution": { "npx": { "package": "@google/gemini-cli", "args": ["--acp"] } } },
            { "id": "amp-acp", "name": "Amp (registry)", "distribution": { "uvx": { "package": "amp" } } },
            { "name": "no id" },
        ] }))
    })
    .await;
    let agents = fetch_with(&client(), &format!("{}/registry.json", server.url)).await.unwrap();
    assert_eq!(agents.len(), 2);
    assert_eq!(agents[0].distribution.npx.as_ref().unwrap().args, vec!["--acp"]);
    let all = with_builtins(agents);
    let ids: Vec<_> = all.iter().map(|agent| (agent.id.as_str(), agent.builtin)).collect();
    assert_eq!(ids, vec![("gemini", false), ("amp-acp", false), ("kiro", true)]);

    let failing = TestServer::start(|_| TestReply::status(500, "down")).await;
    let error = fetch_with(&client(), &failing.url).await.unwrap_err();
    assert!(error.contains("HTTP 500"), "{error}");
}

#[tokio::test]
async fn install_npx_and_uvx_and_kiro() {
    let dir = tempfile::tempdir().unwrap();
    let amp = with_builtins(Vec::new()).into_iter().find(|agent| agent.id == "amp-acp").unwrap();
    let installed = install_with(&client(), &amp, dir.path(), None).await.unwrap();
    assert_eq!((installed.command.as_str(), installed.args.clone()), ("npx", vec!["-y".to_string(), "amp-acp".into()]));
    assert_eq!(installed.source, "builtin");

    let uvx = RegistryAgent {
        id: "py".into(),
        name: "Py".into(),
        distribution: Distribution {
            uvx: Some(PackageTarget { package: "py-acp".into(), args: vec!["acp".into()], ..Default::default() }),
            ..Default::default()
        },
        ..Default::default()
    };
    let installed = install_with(&client(), &uvx, dir.path(), None).await.unwrap();
    assert_eq!((installed.command.as_str(), installed.args), ("uvx", vec!["py-acp".to_string(), "acp".into()]));
    assert_eq!(installed.source, "registry");

    let kiro = with_builtins(Vec::new()).into_iter().find(|agent| agent.id == "kiro").unwrap();
    let error = install_with(&client(), &kiro, dir.path(), Some(dir.path().as_os_str())).await.unwrap_err();
    assert!(error.contains("curl -fsSL https://cli.kiro.dev/install | bash"), "{error}");
    let program = if cfg!(windows) { "kiro-cli.exe" } else { "kiro-cli" };
    std::fs::write(dir.path().join(program), "").unwrap();
    let installed = install_with(&client(), &kiro, dir.path(), Some(dir.path().as_os_str())).await.unwrap();
    assert_eq!(
        (installed.command.as_str(), installed.args),
        ("kiro-cli", vec!["acp".to_string(), "--agent-engine=v3".into()])
    );
}

fn tar_gz(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default()));
    for (path, content) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, path, *content).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

fn binary_agent(url: String, sha256: Option<String>) -> RegistryAgent {
    let target = BinaryTarget {
        archive: url,
        sha256,
        cmd: "./dist/agent".into(),
        args: vec!["--acp".into()],
        env: BTreeMap::from([("A".into(), "1".into())]),
    };
    RegistryAgent {
        id: "bin".into(),
        name: "Bin".into(),
        version: "2.0.0".into(),
        distribution: Distribution {
            binary: BTreeMap::from([(platform_key().unwrap().to_string(), target)]),
            npx: Some(PackageTarget { package: "unused".into(), ..Default::default() }),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[tokio::test]
async fn install_binary_checks_sha256_and_uninstalls() {
    if platform_key().is_none() {
        return;
    }
    let archive = tar_gz(&[("dist/agent", b"#!/bin/sh\n"), ("README", b"hi")]);
    let served = Arc::new(Mutex::new(archive.clone()));
    let body = served.clone();
    let server = TestServer::start(move |_| TestReply::bytes(200, body.lock().unwrap().clone())).await;
    let url = format!("{}/agent-2.0.0.tar.gz", server.url);
    let dir = tempfile::tempdir().unwrap();
    let version_dir = agents_root(dir.path()).join("bin").join("2.0.0");

    let wrong = binary_agent(url.clone(), Some("00".repeat(32)));
    let error = install_with(&client(), &wrong, dir.path(), None).await.unwrap_err();
    assert!(error.contains("SHA-256"), "{error}");
    assert!(!version_dir.exists());
    assert!(!agents_root(dir.path()).join("bin").exists());

    let sha = ring::digest::digest(&ring::digest::SHA256, &archive);
    let sha: String = sha.as_ref().iter().map(|byte| format!("{byte:02X}")).collect();
    let installed = install_with(&client(), &binary_agent(url, Some(sha)), dir.path(), None).await.unwrap();
    let command = std::path::PathBuf::from(&installed.command);
    assert!(command.is_absolute() && command.ends_with("bin/2.0.0/dist/agent"), "{}", installed.command);
    assert_eq!(std::fs::read(&command).unwrap(), b"#!/bin/sh\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&command).unwrap().permissions().mode() & 0o111, 0o111);
    }
    assert_eq!((installed.args.clone(), installed.env["A"].as_str()), (vec!["--acp".to_string()], "1"));
    assert_eq!(installed.install_dir.as_deref(), Some(version_dir.to_str().unwrap()));

    uninstall(&installed, dir.path()).unwrap();
    assert!(!agents_root(dir.path()).join("bin").exists());
    let outside = AcpAgentEntry { install_dir: Some(dir.path().display().to_string()), ..installed };
    uninstall(&outside, dir.path()).unwrap();
    assert!(dir.path().exists());
}
