//! A local marketplace for tests: signed indexes and binary archives on a local HTTP server.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::{Value, json};

use super::marketplace::Marketplace;
use super::marketplace_index::{host_target, is_windows_target, sha256_hex};
use crate::platform::test_http::{TestReply, TestServer, client};

pub(crate) const BINARY: &str = "pi-rust-ext";

pub(crate) struct MockMarketplace {
    pub server: TestServer,
    files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    key: Ed25519KeyPair,
}

impl MockMarketplace {
    pub async fn start() -> Self {
        let files: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::default();
        let served = files.clone();
        let server = TestServer::start(move |request| match served.lock().unwrap().get(request.path()) {
            Some(body) => TestReply::bytes(200, body.clone()),
            None => TestReply::status(404, "missing"),
        })
        .await;
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
        let key = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        Self { server, files, key }
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.key.public_key().as_ref().try_into().unwrap()
    }

    pub fn put(&self, path: &str, body: Vec<u8>) {
        self.files.lock().unwrap().insert(path.to_string(), body);
    }

    pub fn signature(&self, body: &[u8]) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD.encode(self.key.sign(body).as_ref()).into_bytes()
    }

    /// Serve the signed `index` at `/<tag>/index.json`, and also at `/index.json` when `latest`.
    pub fn publish(&self, index: &Value, latest: bool) {
        let body = serde_json::to_vec_pretty(index).unwrap();
        let tag = index["tag"].as_str().unwrap();
        let mut paths = vec![format!("/{tag}/index.json")];
        if latest {
            paths.push("/index.json".to_string());
        }
        for path in paths {
            self.put(&format!("{path}.sig"), self.signature(&body));
            self.put(&path, body.clone());
        }
    }

    pub fn archive_path(tag: &str) -> String {
        let target = host_target().unwrap();
        let extension = if is_windows_target(target) { "zip" } else { "tar.gz" };
        format!("/{tag}/{BINARY}-{target}.{extension}")
    }

    /// An index for the host target whose binary archive holds `content`.
    pub fn release(&self, tag: &str, extensions: &[&str], content: &str) -> Value {
        let target = host_target().unwrap();
        let archive = binary_archive(target, content);
        let path = Self::archive_path(tag);
        let asset = json!({ "url": format!("{}{path}", self.server.url), "sha256": sha256_hex(&archive), "size": archive.len() });
        self.put(&path, archive);
        let entries: Vec<Value> = extensions
            .iter()
            .map(|name| {
                json!({
                    "name": name, "displayName": name, "description": format!("The {name} extension"),
                    "version": tag.trim_start_matches('v'), "binary": BINARY, "args": [name], "tags": ["test"],
                    "platforms": [target],
                })
            })
            .collect();
        json!({
            "schemaVersion": 1, "repository": "yannelli/pi-rust-extensions", "tag": tag, "protocolVersion": 1,
            "minPiVersion": "0.1.0", "binaries": { BINARY: { target: asset } }, "extensions": entries,
        })
    }

    pub fn marketplace(&self, agent_dir: &Path, offline: bool) -> Marketplace {
        Marketplace::new(agent_dir.to_path_buf(), client(), offline)
            .with_public_key(self.public_key())
            .with_base_url(&self.server.url)
    }
}

/// The release archive layout: `<binary>-<target>/<binary>[.exe]`.
pub(crate) fn binary_archive(target: &str, content: &str) -> Vec<u8> {
    let folder = format!("{BINARY}-{target}");
    if is_windows_target(target) {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer.start_file(format!("{folder}/{BINARY}.exe"), zip::write::SimpleFileOptions::default()).unwrap();
        writer.write_all(content.as_bytes()).unwrap();
        return writer.finish().unwrap().into_inner();
    }
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    let mut header = tar::Header::new_gnu();
    header.set_size(content.len() as u64);
    header.set_mode(0o644);
    header.set_entry_type(tar::EntryType::Regular);
    builder.append_data(&mut header, format!("{folder}/{BINARY}"), content.as_bytes()).unwrap();
    builder.into_inner().unwrap().finish().unwrap()
}
