use crate::common;

use common::*;

// The PEM fixtures in fixtures/node_x509 come from OpenSSL 3.5. x509.json is the output of
// x509.js under Node 22.23 with the same files.

fn fixture_script(body: &str) -> String {
    let pem = |text: &str| format!("{text:?}");
    body.replace("__CA__", &pem(include_str!("fixtures/node_x509/ca.pem")))
        .replace("__LEAF_KEY__", &pem(include_str!("fixtures/node_x509/leaf_key.pem")))
        .replace("__LEAF__", &pem(include_str!("fixtures/node_x509/leaf.pem")))
        .replace("__PSS__", &pem(include_str!("fixtures/node_x509/pss.pem")))
        .replace("__EC_KEY__", &pem(include_str!("fixtures/node_x509/ec_key.pem")))
        .replace("__EC__", &pem(include_str!("fixtures/node_x509/ec.pem")))
        .replace("__ED__", &pem(include_str!("fixtures/node_x509/ed.pem")))
        .replace("__WILD__", &pem(include_str!("fixtures/node_x509/wild.pem")))
}

#[tokio::test]
async fn x509_certificates_match_node() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let actual = run_cjs(&runtime, &root, &fixture_script(include_str!("fixtures/node_x509/x509.js"))).await;
    assert_same_keys(&actual, &serde_json::from_str(include_str!("fixtures/node_x509/x509.json")).unwrap());
}

#[tokio::test]
async fn tls_peer_x509_certificates_match_node() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let actual = run_cjs_within(&runtime, &root, &fixture_script(include_str!("fixtures/node_x509/tls.js")), 60).await;
    assert_same_keys(&actual, &serde_json::from_str(include_str!("fixtures/node_x509/tls.json")).unwrap());
}
