//! SDK `DefaultPackageManager` over the Rust package manager. The expected values come from
//! upstream pi 1.6.0 running the same script under Node 22.

use crate::extensions::harness;

use harness::{Setup, harness};
use serde_json::json;

const SCRIPT: &str = r#"
import { mkdirSync, writeFileSync } from 'node:fs';
import { DefaultPackageManager, SettingsManager } from '@earendil-works/pi-coding-agent';
export default pi => pi.registerCommand('pm', { handler: async (_args, ctx) => {
  const cwd = ctx.cwd;
  const agentDir = `${cwd}/pm-agent`; const pkg = `${cwd}/local-pkg`;
  mkdirSync(`${pkg}/extensions`, { recursive: true }); mkdirSync(agentDir, { recursive: true });
  writeFileSync(`${pkg}/package.json`, JSON.stringify({ name: 'local-pkg', pi: { extensions: ['./extensions'] } }));
  writeFileSync(`${pkg}/extensions/hello.ts`, 'export default () => {};');
  writeFileSync(`${agentDir}/settings.json`, JSON.stringify({ packages: [pkg] }));
  const settingsManager = SettingsManager.create(cwd, agentDir);
  const manager = new DefaultPackageManager({ cwd, agentDir, settingsManager });
  // Native separators on Windows, as in Node; the expected values use `/`.
  const rel = (p) => p.replaceAll('\\', '/').replace(cwd.replaceAll('\\', '/'), '<cwd>');
  const listed = manager.listConfiguredPackages().map((p) => ({ ...p, source: rel(p.source), installedPath: p.installedPath && rel(p.installedPath) }));
  const installed = rel(manager.getInstalledPath(pkg, 'user'));
  const resolved = await manager.resolve();
  const extensions = resolved.extensions.map((r) => ({ path: rel(r.path), enabled: r.enabled, metadata: { ...r.metadata,
    source: rel(r.metadata.source), baseDir: r.metadata.baseDir && rel(r.metadata.baseDir), packageRoot: r.metadata.packageRoot && rel(r.metadata.packageRoot) } }));
  const sources = (await manager.resolveExtensionSources([pkg], { temporary: true })).extensions
    .map((r) => [rel(r.path), r.metadata.scope, r.metadata.origin, r.metadata.source && rel(r.metadata.source)]);
  const parsed = ['npm:@scope/pkg@^1.2.0', 'npm:foo@1.2.3', 'npm:bar', 'git:github.com/user/repo@v1', 'https://github.com/user/repo', './local']
    .map((s) => manager.parseSource(s));
  const added = manager.addSourceToSettings('npm:other-pkg');
  const packagesAfterAdd = settingsManager.getGlobalSettings().packages.map(rel);
  const removed = manager.removeSourceFromSettings('npm:other-pkg');
  await settingsManager.flush();
  const updates = await manager.checkForAvailableUpdates();
  let refused;
  try { new DefaultPackageManager({ cwd, agentDir, settingsManager: {} }); } catch (error) { refused = error.code; }
  pi.appendEntry('result', { listed, installed, extensions, sources, parsed, added, packagesAfterAdd, removed,
    packages: settingsManager.getGlobalSettings().packages.map(rel), updates, refused });
}});
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_package_manager_lists_resolves_and_edits_packages_as_upstream() {
    let h = harness(Setup { files: vec![("pm.ts", SCRIPT.into())], load: vec!["pm.ts"], ..Setup::default() }).await;
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    tokio::time::timeout(std::time::Duration::from_secs(30), h.session.prompt("/pm", Vec::new()))
        .await
        .expect("the command finishes")
        .unwrap();
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let data = h.entries().iter().find(|entry| entry["customType"] == "result").expect("result")["data"].clone();
    let package =
        json!({"source": "<cwd>/local-pkg", "scope": "user", "filtered": false, "installedPath": "<cwd>/local-pkg"});
    assert_eq!(data["listed"], json!([package]));
    assert_eq!(data["installed"], "<cwd>/local-pkg");
    let metadata = json!({"source": "<cwd>/local-pkg", "scope": "user", "origin": "package", "baseDir": "<cwd>/local-pkg",
        "packageRoot": "<cwd>/local-pkg"});
    assert_eq!(
        data["extensions"],
        json!([{"path": "<cwd>/local-pkg/extensions/hello.ts", "enabled": true, "metadata": metadata}])
    );
    assert_eq!(
        data["sources"],
        json!([["<cwd>/local-pkg/extensions/hello.ts", "temporary", "package", "<cwd>/local-pkg"]])
    );
    let parsed = json!([
        {"type": "npm", "spec": "@scope/pkg@^1.2.0", "name": "@scope/pkg", "version": "^1.2.0", "range": ">=1.2.0 <2.0.0-0", "pinned": false},
        {"type": "npm", "spec": "foo@1.2.3", "name": "foo", "version": "1.2.3", "range": "1.2.3", "pinned": true},
        {"type": "npm", "spec": "bar", "name": "bar", "pinned": false},
        {"type": "git", "repo": "https://github.com/user/repo", "host": "github.com", "path": "user/repo", "ref": "v1", "pinned": true},
        {"type": "git", "repo": "https://github.com/user/repo", "host": "github.com", "path": "user/repo", "pinned": false},
        {"type": "local", "path": "./local"},
    ]);
    assert_eq!(data["parsed"], parsed);
    assert_eq!((data["added"].clone(), data["removed"].clone()), (json!(true), json!(true)));
    assert_eq!(data["packagesAfterAdd"], json!(["<cwd>/local-pkg", "npm:other-pkg"]));
    assert_eq!(data["packages"], json!(["<cwd>/local-pkg"]));
    assert_eq!(data["updates"], json!([]));
    assert_eq!(data["refused"], "ERR_PI_UNSUPPORTED");
}
