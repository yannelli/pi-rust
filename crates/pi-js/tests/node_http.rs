mod common;

use common::*;

// Each fixture is an async function body; its `.json` file is what Node 24 returns for it.
// Everything stays on localhost.
async fn check(script: &str, expected: &str) {
    let (_dir, root) = tempdir();
    let (runtime, capture) = runtime(&root);
    let actual = run_cjs(&runtime, &root, script).await;
    println!("{actual}");
    assert_same_keys(&actual, &serde_json::from_str(expected).unwrap());
    let uncaught = capture.uncaught.lock().unwrap();
    assert!(
        uncaught.is_empty(),
        "{:?}",
        uncaught.iter().map(|(_, error)| error.display_with_stack()).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn http_matches_node() {
    check(include_str!("fixtures/node_http/http1.js"), include_str!("fixtures/node_http/http1.json")).await;
}

#[tokio::test]
async fn https_matches_node() {
    let script = include_str!("fixtures/node_http/https.js")
        .replace("__KEY__", &format!("{:?}", include_str!("fixtures/node_http/key.pem")))
        .replace("__CERT__", &format!("{:?}", include_str!("fixtures/node_http/cert.pem")));
    check(&script, include_str!("fixtures/node_http/https.json")).await;
}
