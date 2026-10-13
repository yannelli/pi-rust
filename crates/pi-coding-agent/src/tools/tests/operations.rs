//! Custom file-tool backends (upstream `*Operations`), ported from tools.test.ts and the
//! issue #6104 find regression test.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::anyhow;
use async_trait::async_trait;
use pi_agent::SharedTool;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{noop, text};
use crate::tools::{
    EditOperations, EditToolOptions, FindGlobOptions, FindOperations, FindToolOptions, GrepOperations, GrepToolOptions,
    LsOperations, LsToolOptions, ReadOperations, ReadToolOptions, ToolsOptions, WriteOperations, WriteToolOptions,
    create_tool_with_options, relativize_find_result_path,
};

/// An in-memory file system keyed by absolute path.
#[derive(Default)]
struct MemoryFs {
    files: Mutex<BTreeMap<PathBuf, String>>,
    dirs: Vec<PathBuf>,
    made_dirs: Mutex<Vec<PathBuf>>,
}

impl MemoryFs {
    fn with(files: &[(&str, &str)], dirs: &[&str]) -> Arc<Self> {
        let files = files.iter().map(|(path, text)| (PathBuf::from(path), text.to_string())).collect();
        Arc::new(Self { files: Mutex::new(files), dirs: dirs.iter().map(PathBuf::from).collect(), ..Self::default() })
    }

    fn get(&self, path: &Path) -> anyhow::Result<String> {
        self.files.lock().unwrap().get(path).cloned().ok_or_else(|| anyhow!("no such file {}", path.display()))
    }

    fn put(&self, path: &Path, content: &str) {
        self.files.lock().unwrap().insert(path.to_path_buf(), content.to_string());
    }
}

#[async_trait]
impl ReadOperations for MemoryFs {
    async fn read_file(&self, path: &Path) -> anyhow::Result<Vec<u8>> {
        Ok(self.get(path)?.into_bytes())
    }
    async fn access(&self, path: &Path) -> anyhow::Result<()> {
        self.get(path).map(|_| ())
    }
}

#[async_trait]
impl WriteOperations for MemoryFs {
    async fn write_file(&self, path: &Path, content: &str) -> anyhow::Result<()> {
        self.put(path, content);
        Ok(())
    }
    async fn mkdir(&self, dir: &Path) -> anyhow::Result<()> {
        self.made_dirs.lock().unwrap().push(dir.to_path_buf());
        Ok(())
    }
}

#[async_trait]
impl EditOperations for MemoryFs {
    async fn read_file(&self, path: &Path) -> anyhow::Result<Vec<u8>> {
        Ok(self.get(path)?.into_bytes())
    }
    async fn write_file(&self, path: &Path, content: &str) -> anyhow::Result<()> {
        self.put(path, content);
        Ok(())
    }
    async fn access(&self, path: &Path) -> anyhow::Result<()> {
        self.get(path).map(|_| ())
    }
}

#[async_trait]
impl LsOperations for MemoryFs {
    async fn exists(&self, path: &Path) -> anyhow::Result<bool> {
        Ok(self.dirs.iter().any(|dir| dir == path) || self.get(path).is_ok())
    }
    async fn is_directory(&self, path: &Path) -> anyhow::Result<bool> {
        if self.dirs.iter().any(|dir| dir == path) {
            return Ok(true);
        }
        self.get(path).map(|_| false)
    }
    async fn read_dir(&self, path: &Path) -> anyhow::Result<Vec<String>> {
        let files = self.files.lock().unwrap();
        let children = files.keys().chain(&self.dirs).filter(|child| child.parent() == Some(path));
        Ok(children.map(|child| child.file_name().unwrap().to_string_lossy().into_owned()).collect())
    }
}

async fn exec(tool: &SharedTool, args: Value) -> anyhow::Result<pi_agent::AgentToolResult> {
    tool.execute("call-1", args, CancellationToken::new(), noop()).await
}

fn tool(name: &str, cwd: &str, options: ToolsOptions) -> SharedTool {
    create_tool_with_options(name, Path::new(cwd), &options).unwrap()
}

#[tokio::test]
async fn read_write_and_edit_use_custom_operations() {
    let fs = MemoryFs::with(&[("/remote/a.txt", "hello\nworld")], &["/remote"]);
    let options = ToolsOptions {
        read: ReadToolOptions { operations: Some(fs.clone()), ..ReadToolOptions::default() },
        write: WriteToolOptions::with_operations(fs.clone()),
        edit: EditToolOptions::with_operations(fs.clone()),
        ..ToolsOptions::default()
    };
    let read = tool("read", "/remote", options.clone());
    assert_eq!(text(&exec(&read, json!({ "path": "a.txt" })).await.unwrap()), "hello\nworld");
    assert!(exec(&read, json!({ "path": "missing.txt" })).await.is_err());

    let write = tool("write", "/remote", options.clone());
    exec(&write, json!({ "path": "sub/b.txt", "content": "new" })).await.unwrap();
    assert_eq!(fs.get(Path::new("/remote/sub/b.txt")).unwrap(), "new");
    assert_eq!(*fs.made_dirs.lock().unwrap(), vec![PathBuf::from("/remote/sub")]);

    let edit = tool("edit", "/remote", options);
    let args = json!({ "path": "a.txt", "edits": [{ "oldText": "world", "newText": "there" }] });
    exec(&edit, args).await.unwrap();
    assert_eq!(fs.get(Path::new("/remote/a.txt")).unwrap(), "hello\nthere");
}

struct OfflineEdit;

#[async_trait]
impl EditOperations for OfflineEdit {
    async fn read_file(&self, _: &Path) -> anyhow::Result<Vec<u8>> {
        Ok(b"hello\n".to_vec())
    }
    async fn write_file(&self, _: &Path, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn access(&self, _: &Path) -> anyhow::Result<()> {
        Err(anyhow!("disk offline"))
    }
}

#[tokio::test]
async fn edit_reports_the_original_message_of_unknown_access_errors() {
    let options = ToolsOptions { edit: EditToolOptions::with_operations(Arc::new(OfflineEdit)), ..Default::default() };
    let edit = tool("edit", "/tmp", options);
    let args = json!({ "path": "broken.txt", "edits": [{ "oldText": "hello", "newText": "world" }] });
    let err = exec(&edit, args).await.unwrap_err().to_string();
    assert_eq!(err, "Could not edit file: broken.txt. Error: disk offline.");
}

#[cfg(unix)]
#[tokio::test]
async fn edit_reports_eacces_for_read_only_files() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("edit-readonly.txt");
    std::fs::write(&file, "hello\n").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o444)).unwrap();
    let edit = tool("edit", dir.path().to_str().unwrap(), ToolsOptions::default());
    let args = json!({ "path": file, "edits": [{ "oldText": "hello", "newText": "world" }] });
    let err = exec(&edit, args).await.unwrap_err().to_string();
    assert_eq!(err, format!("Could not edit file: {}. Error code: EACCES.", file.display()));
}

#[tokio::test]
async fn ls_uses_custom_operations() {
    let fs = MemoryFs::with(&[("/r/b.txt", ""), ("/r/A.md", "")], &["/r", "/r/src"]);
    let ls = tool("ls", "/r", ToolsOptions { ls: LsToolOptions::with_operations(fs.clone()), ..Default::default() });
    assert_eq!(text(&exec(&ls, json!({})).await.unwrap()), "A.md\nb.txt\nsrc/");
    let err = exec(&ls, json!({ "path": "b.txt" })).await.unwrap_err().to_string();
    assert_eq!(err, "Not a directory: /r/b.txt");
    let err = exec(&ls, json!({ "path": "nope" })).await.unwrap_err().to_string();
    assert_eq!(err, "Path not found: /r/nope");
}

struct FixedGlob(Vec<&'static str>, Mutex<Option<FindGlobOptions>>);

#[async_trait]
impl FindOperations for FixedGlob {
    async fn exists(&self, _: &Path) -> anyhow::Result<bool> {
        Ok(true)
    }
    async fn glob(&self, _: &str, _: &Path, options: FindGlobOptions) -> anyhow::Result<Vec<String>> {
        *self.1.lock().unwrap() = Some(options);
        Ok(self.0.iter().map(|path| path.to_string()).collect())
    }
}

#[cfg(unix)]
#[tokio::test]
async fn find_relativizes_custom_glob_results_against_a_root_search_path() {
    let ops = Arc::new(FixedGlob(vec!["/home/user/project/", "/home/user/project/file.txt"], Mutex::default()));
    let options = ToolsOptions { find: FindToolOptions::with_operations(ops.clone()), ..Default::default() };
    let find = tool("find", "/", options);
    let result = exec(&find, json!({ "pattern": "**" })).await.unwrap();
    assert_eq!(text(&result), "home/user/project/\nhome/user/project/file.txt");
    let seen = ops.1.lock().unwrap().clone().unwrap();
    assert_eq!(seen.ignore, vec!["**/node_modules/**", "**/.git/**"]);
    assert_eq!(seen.limit, 1000);

    let limited = exec(&find, json!({ "pattern": "**", "limit": 2 })).await.unwrap();
    assert!(text(&limited).ends_with("[2 results limit reached]"), "{}", text(&limited));
    assert_eq!(limited.details.unwrap()["resultLimitReached"], 2);
}

#[cfg(unix)]
#[test]
fn relativize_find_result_path_on_posix() {
    let rel = |path: &str, base: &str| relativize_find_result_path(path, Path::new(base));
    assert_eq!(rel("/home/user/file.txt", "/"), "home/user/file.txt");
    assert_eq!(rel("/home/user/project/", "/"), "home/user/project/");
    assert_eq!(rel("/home/user/file\\", "/home/user"), "file\\");
    assert_eq!(rel("/tmp/results/file.txt", "/workspace/project"), "../../tmp/results/file.txt");
    assert_eq!(rel("/tmp/results/dir/", "/workspace/project"), "../../tmp/results/dir/");
    assert_eq!(rel("src/lib.rs", "/workspace"), "src/lib.rs");
}

#[cfg(windows)]
#[test]
fn relativize_find_result_path_on_windows() {
    let rel = |path: &str, base: &str| relativize_find_result_path(path, Path::new(base));
    assert_eq!(rel("I:\\AI\\Models\\TextGen\\gemma4\\", "I:\\"), "AI/Models/TextGen/gemma4/");
    assert_eq!(rel("I:\\AI\\Models\\", "I:\\AI"), "Models/");
    assert_eq!(rel("I:\\AI\\Models2\\file.txt", "I:\\AI\\Models"), "../Models2/file.txt");
    assert_eq!(rel("AI\\Models\\TextGen\\gemma4\\", "I:\\"), "AI/Models/TextGen/gemma4/");
}

/// Grep searches the local tree but reads context lines through the backend.
struct ContextFromBackend;

#[async_trait]
impl GrepOperations for ContextFromBackend {
    async fn is_directory(&self, path: &Path) -> anyhow::Result<bool> {
        Ok(std::fs::metadata(path)?.is_dir())
    }
    async fn read_file(&self, _: &Path) -> anyhow::Result<String> {
        Ok("before\nneedle\nafter\n".into())
    }
}

#[tokio::test]
async fn grep_reads_context_lines_through_operations() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "one\nneedle\nthree\n").unwrap();
    let cwd = dir.path().to_str().unwrap();
    let options =
        ToolsOptions { grep: GrepToolOptions::with_operations(Arc::new(ContextFromBackend)), ..Default::default() };
    let grep = tool("grep", cwd, options);
    let plain = exec(&grep, json!({ "pattern": "needle" })).await.unwrap();
    assert_eq!(text(&plain), "a.txt:2: needle");
    let with_context = exec(&grep, json!({ "pattern": "needle", "context": 1 })).await.unwrap();
    assert_eq!(text(&with_context), "a.txt-1- before\na.txt:2: needle\na.txt-3- after");
    let err = exec(&grep, json!({ "pattern": "x", "path": "missing" })).await.unwrap_err().to_string();
    assert!(err.starts_with("Path not found: "), "{err}");
}
