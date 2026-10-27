//! URL archive packages served by a local HTTP server.

use std::io::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::resources::SourceScope;

/// Serves `body` (with `etag`) for every request and answers matching `If-None-Match` with 304.
#[derive(Clone)]
struct Server {
    base: String,
    content: Arc<Mutex<(Vec<u8>, String, u16)>>,
    requests: Arc<AtomicUsize>,
}

impl Server {
    async fn start(body: Vec<u8>, etag: &str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let content = Arc::new(Mutex::new((body, etag.to_string(), 200)));
        let requests = Arc::new(AtomicUsize::new(0));
        let server = Self { base, content: content.clone(), requests: requests.clone() };
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let (content, requests) = (content.clone(), requests.clone());
                tokio::spawn(async move {
                    let mut buffer = vec![0u8; 8192];
                    let size = stream.read(&mut buffer).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buffer[..size]).to_lowercase();
                    requests.fetch_add(1, Ordering::SeqCst);
                    let (body, etag, status) = content.lock().unwrap().clone();
                    let head = request.starts_with("head ");
                    let not_modified = request.contains(&format!("if-none-match: {}", etag.to_lowercase()));
                    let (status, body) = match (status, not_modified) {
                        (200, true) => (304, Vec::new()),
                        (200, false) => (200, if head { Vec::new() } else { body }),
                        (other, _) => (other, Vec::new()),
                    };
                    let header = format!(
                        "HTTP/1.1 {status} X\r\nETag: {etag}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(header.as_bytes()).await;
                    let _ = stream.write_all(&body).await;
                });
            }
        });
        server
    }

    fn set(&self, body: Vec<u8>, etag: &str, status: u16) {
        *self.content.lock().unwrap() = (body, etag.to_string(), status);
    }
}

fn tar_gz(entries: &[(&str, &str)]) -> Vec<u8> {
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for (path, content) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        let name = header.as_old_mut().name.as_mut();
        name[..path.len()].copy_from_slice(path.as_bytes());
        header.set_cksum();
        builder.append(&header, content.as_bytes()).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

fn zip_file(entries: &[(&str, &str)]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (path, content) in entries {
        writer.start_file(*path, options).unwrap();
        writer.write_all(content.as_bytes()).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn package_json(version: &str) -> String {
    json!({"name": "tools", "version": version, "keywords": ["pi-package"], "pi": {"skills": ["./skills"]}}).to_string()
}

#[tokio::test]
async fn tar_gz_packages_install_resolve_update_and_remove() {
    let fixture = Fixture::new();
    let v1 = tar_gz(&[
        ("package/package.json", &package_json("1.0.0")),
        ("package/skills/review/SKILL.md", &skill("review")),
        ("package/extensions/ignored.ts", EXTENSION),
        ("../escape.txt", "outside"),
    ]);
    let server = Server::start(v1, "\"v1\"").await;
    let source = format!("{}/dl/tools-1.0.0.tgz", server.base);
    let runner = MockRunner::failing();
    let manager = fixture.manager(runner.clone());
    manager.install_and_persist(&source, false).await.unwrap();
    let installed = fixture.agent_dir.join("url/127.0.0.1/dl/tools-1.0.0");
    assert!(installed.join("skills/review/SKILL.md").exists());
    assert!(
        !fixture.agent_dir.join("url/127.0.0.1/dl/escape.txt").exists() && !fixture.root().join("escape.txt").exists()
    );
    assert_eq!(fixture.global_packages(), json!([source]));
    assert!(runner.calls().is_empty(), "packages without dependencies need no npm");

    let resolved = manager.resolve_package_resources(None).await.unwrap();
    assert_eq!(resolved.skills[0].path, installed.join("skills/review/SKILL.md"));
    assert!(resolved.extensions.is_empty(), "the manifest lists skills only");
    let listed = manager.list_configured_packages().await;
    assert_eq!(listed[0].installed_path.as_deref(), Some(installed.as_path()));

    assert!(manager.check_for_available_updates().await.is_empty());
    write(&installed.join("local-marker"), "kept");
    manager.update(None).await.unwrap();
    assert!(installed.join("local-marker").exists(), "304 keeps the installed copy");

    let v2 =
        tar_gz(&[("package/package.json", &package_json("2.0.0")), ("package/skills/lint/SKILL.md", &skill("lint"))]);
    server.set(v2, "\"v2\"", 200);
    let updates = manager.check_for_available_updates().await;
    assert_eq!((updates.len(), updates[0].kind), (1, crate::packages::PackageKind::Url));
    manager.update(Some(&source)).await.unwrap();
    assert!(installed.join("skills/lint/SKILL.md").exists() && !installed.join("local-marker").exists());

    assert!(manager.remove_and_persist(&source, false).await.unwrap());
    assert!(!fixture.agent_dir.join("url/127.0.0.1").exists());
    assert_eq!(fixture.global_packages(), json!([]));
}

#[tokio::test]
async fn zip_packages_without_a_top_directory_install_dependencies() {
    let fixture = Fixture::new();
    let manifest = json!({"name": "z", "version": "1.0.0", "dependencies": {"left-pad": "1"}}).to_string();
    let server = Server::start(
        zip_file(&[("package.json", &manifest), ("prompts/fix.md", "Fix it"), ("themes/t.json", "{}")]),
        "z1",
    )
    .await;
    let source = format!("{}/z.zip", server.base);
    let runner = MockRunner::new(|_| Ok(String::new()));
    let manager = fixture.manager(runner.clone());
    manager.install(&source, true).await.unwrap();
    let installed = fixture.cwd.join(".pi/url/127.0.0.1/z");
    assert!(installed.join("prompts/fix.md").exists() && installed.join("themes/t.json").exists());
    let calls = runner.calls();
    assert_eq!(calls[0].argv(), ["npm", "install", "--omit=dev", "--legacy-peer-deps", "--no-audit", "--no-fund"]);
    assert_eq!(calls[0].spec.cwd.as_deref(), Some(installed.as_path()));
    let resolved = manager.resolve_extension_sources(&[source], true, false).await.unwrap();
    assert_eq!(resolved.prompts.len(), 1);
    assert_eq!(resolved.themes[0].metadata.scope, SourceScope::Project);
}

#[tokio::test]
async fn failed_downloads_leave_nothing_behind() {
    let fixture = Fixture::new();
    let server = Server::start(Vec::new(), "x").await;
    server.set(Vec::new(), "x", 404);
    let source = format!("{}/missing.tar.gz", server.base);
    let manager = fixture.manager(MockRunner::failing());
    let error = manager.install(&source, false).await.unwrap_err();
    assert_eq!(error.to_string(), format!("Could not download {source}: HTTP 404"));
    server.set(b"not an archive".to_vec(), "y", 200);
    let error = manager.install(&source, false).await.unwrap_err();
    assert!(error.to_string().contains("Could not unpack archive"), "{error}");
    assert!(!fixture.agent_dir.join("url/127.0.0.1").exists());
    assert!(server.requests.load(Ordering::SeqCst) >= 2);
}
