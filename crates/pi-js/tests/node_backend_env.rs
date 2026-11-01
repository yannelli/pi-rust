//! Native Node queries and children follow environment changes within one isolated backend process.

mod common;
mod node_common;

use common::*;
use serde_json::json;

#[tokio::test]
async fn native_environment_queries_follow_extension_environment_changes() {
    let (_dir, root) = tempdir();
    let initial = root.join("initial");
    let changed = root.join("changed");
    std::fs::create_dir_all(&initial).unwrap();
    std::fs::create_dir_all(&changed).unwrap();
    let capture = std::sync::Arc::new(Capture::default());
    let mut options = options(&root, &capture);
    let mut env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    env.insert("HOME".into(), initial.display().to_string());
    env.insert("USERPROFILE".into(), initial.display().to_string());
    env.insert("PI_NODE_TEST_ENV".into(), "provided".into());
    options.env = Some(env);
    let parent_home = std::env::var_os("HOME");
    let Some(runtime) = node_common::node_runtime_with(options) else { return };
    assert!(runtime.is_node());
    let script = format!(
        r#"
const {{ homedir }} = await import("node:os");
const {{ spawnSync }} = await import("node:child_process");
const key = process.platform === "win32" ? "USERPROFILE" : "HOME";
const configured = homedir() === process.env[key];
process.env[key] = {};
const updated = homedir() === process.env[key];
const child = spawnSync(process.execPath, ["-e",
  "const os = require('node:os'); const key = process.platform === 'win32' ? 'USERPROFILE' : 'HOME'; " +
  "process.stdout.write(JSON.stringify(os.homedir() === process.env[key] && process.env.PI_NODE_TEST_ENV === 'provided'));"
], {{ encoding: "utf8" }});
return {{ configured, updated, provided: process.env.PI_NODE_TEST_ENV === "provided",
  childInherited: child.status === 0 && JSON.parse(child.stdout) }};
"#,
        json!(changed.display().to_string())
    );
    let actual = run_cjs_within(&runtime, &root, &script, 20).await;
    assert_eq!(actual, json!({ "configured": true, "updated": true, "provided": true, "childInherited": true }));
    assert!(std::env::var_os("HOME") == parent_home, "a backend must not change the Rust host environment");
}
