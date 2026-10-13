//! File workspace with journaled, revision-checked edits (upstream Node `NodeWorkspace`).

use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::fs_util::{atomic_write, ensure_private_dir, file_mode, read_optional, serialized, sha256_hex};
use crate::host::WorkspaceAdapter;
use crate::types::{Reconciliation, SearchMatch, ToolInvocation, ToolResult, WorkspaceDocument};

const RECEIPTS: &str = ".pi-embedded-receipts";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ReceiptPhase {
    Pending,
    Completed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EditReceipt {
    version: u32,
    id: String,
    path: String,
    expected_revision: String,
    document: WorkspaceDocument,
    phase: ReceiptPhase,
}

fn check(cancel: &CancellationToken) -> anyhow::Result<()> {
    if cancel.is_cancelled() {
        anyhow::bail!("Operation cancelled");
    }
    Ok(())
}

fn document_result(document: &WorkspaceDocument) -> ToolResult {
    ToolResult::json(serde_json::to_value(document).unwrap_or_default())
}

/// Reads, searches, and reviewed edits under one root, without shell access. Symlinks are
/// rejected. Edit receipts live in `.pi-embedded-receipts`; keep them with the workspace.
pub struct FsWorkspace {
    root: PathBuf,
    receipts: PathBuf,
}

impl FsWorkspace {
    pub fn new(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        let root = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
        Self { receipts: root.join(RECEIPTS), root }
    }

    async fn canonical_root(&self) -> anyhow::Result<PathBuf> {
        Ok(tokio::fs::canonicalize(&self.root).await?)
    }

    /// Resolve `path` inside the root, rejecting escapes, the receipt directory, and symlinks.
    async fn resolve_file(&self, path: &str) -> anyhow::Result<(PathBuf, String)> {
        let relative = Path::new(path);
        let escapes = relative.is_absolute()
            || path.contains('\0')
            || path.split(['/', '\\']).any(|part| part == "..")
            || relative.components().any(|part| matches!(part, Component::Prefix(_) | Component::RootDir));
        if escapes {
            anyhow::bail!("Workspace path escapes root: {path}");
        }
        let root = self.canonical_root().await?;
        let parts: Vec<_> = relative.components().filter(|part| matches!(part, Component::Normal(_))).collect();
        let first = parts.first().map(|part| part.as_os_str().to_string_lossy().into_owned());
        if parts.is_empty() || first.as_deref() == Some(RECEIPTS) {
            anyhow::bail!("Invalid workspace file: {path}");
        }
        let mut current = root.clone();
        for part in &parts {
            current.push(part);
            if tokio::fs::symlink_metadata(&current).await?.file_type().is_symlink() {
                anyhow::bail!("Workspace symlink rejected: {path}");
            }
        }
        let local: PathBuf = parts.iter().collect();
        Ok((current, local.to_string_lossy().into_owned()))
    }

    async fn receipt_path(&self, invocation_id: &str) -> anyhow::Result<PathBuf> {
        ensure_private_dir(&self.receipts).await?;
        if tokio::fs::symlink_metadata(&self.receipts).await?.file_type().is_symlink() {
            anyhow::bail!("Workspace receipts symlink rejected");
        }
        Ok(self.receipts.join(format!("{}.json", sha256_hex(invocation_id))))
    }

    async fn read_receipt(path: &Path) -> anyhow::Result<Option<EditReceipt>> {
        match read_optional(path).await? {
            Some(content) => Ok(Some(serde_json::from_str(&content)?)),
            None => Ok(None),
        }
    }

    async fn write_receipt(path: &Path, receipt: &EditReceipt) -> anyhow::Result<()> {
        atomic_write(path, &serde_json::to_string(receipt)?, 0o600).await
    }

    async fn search_directory(
        &self,
        directory: &str,
        query: &str,
        cancel: &CancellationToken,
        results: &mut Vec<SearchMatch>,
    ) -> anyhow::Result<()> {
        check(cancel)?;
        let mut entries = tokio::fs::read_dir(self.root.join(directory)).await?;
        let mut names = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            names.push((entry.file_name().to_string_lossy().into_owned(), entry.file_type().await?));
        }
        names.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, kind) in names {
            if kind.is_symlink() || name == RECEIPTS {
                continue;
            }
            let path = if directory.is_empty() {
                name
            } else {
                Path::new(directory).join(name).to_string_lossy().into_owned()
            };
            if kind.is_dir() {
                self.resolve_file(&path).await?;
                Box::pin(self.search_directory(&path, query, cancel, results)).await?;
            } else if kind.is_file() {
                let document = self.read(&path, cancel.clone()).await?;
                if document.text.contains('\0') {
                    continue;
                }
                for (index, line) in document.text.split('\n').enumerate() {
                    let line = line.strip_suffix('\r').unwrap_or(line);
                    if line.contains(query) {
                        results.push(SearchMatch { path: path.clone(), line: index as u64 + 1, text: line.into() });
                    }
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl WorkspaceAdapter for FsWorkspace {
    async fn read(&self, path: &str, cancel: CancellationToken) -> anyhow::Result<WorkspaceDocument> {
        check(&cancel)?;
        let (absolute, local) = self.resolve_file(path).await?;
        let metadata = tokio::fs::symlink_metadata(&absolute).await?;
        if !metadata.is_file() {
            anyhow::bail!("Workspace path is not a file: {path}");
        }
        let text = String::from_utf8_lossy(&tokio::fs::read(&absolute).await?).into_owned();
        check(&cancel)?;
        Ok(WorkspaceDocument { path: local, revision: sha256_hex(&text), text })
    }

    async fn search(&self, query: &str, cancel: CancellationToken) -> anyhow::Result<Vec<SearchMatch>> {
        if query.is_empty() {
            anyhow::bail!("Search query must not be empty");
        }
        let mut results = Vec::new();
        self.search_directory("", query, &cancel, &mut results).await?;
        Ok(results)
    }

    async fn edit(
        &self,
        path: &str,
        expected_revision: &str,
        text: &str,
        invocation_id: &str,
        cancel: CancellationToken,
    ) -> anyhow::Result<WorkspaceDocument> {
        let root = self.canonical_root().await?;
        serialized(&format!("workspace:{}", root.display()), async {
            check(&cancel)?;
            let (absolute, local) = self.resolve_file(path).await?;
            let receipt_path = self.receipt_path(invocation_id).await?;
            if let Some(mut previous) = Self::read_receipt(&receipt_path).await? {
                let same = previous.id == invocation_id
                    && previous.path == local
                    && previous.expected_revision == expected_revision
                    && previous.document.text == text;
                if !same {
                    anyhow::bail!("Invocation ID reused for a different edit: {invocation_id}");
                }
                if previous.phase == ReceiptPhase::Completed {
                    return Ok(previous.document);
                }
                let current = self.read(path, cancel.clone()).await?;
                if current.revision == previous.document.revision {
                    previous.phase = ReceiptPhase::Completed;
                    Self::write_receipt(&receipt_path, &previous).await?;
                    return Ok(previous.document);
                }
                if current.revision != expected_revision {
                    anyhow::bail!("Edit requires reconciliation: {invocation_id}");
                }
            }
            let current = self.read(path, cancel.clone()).await?;
            if current.revision != expected_revision {
                anyhow::bail!("Workspace revision conflict: {path}");
            }
            let document = WorkspaceDocument { path: local.clone(), text: text.into(), revision: sha256_hex(text) };
            let mut receipt = EditReceipt {
                version: 1,
                id: invocation_id.into(),
                path: local,
                expected_revision: expected_revision.into(),
                document: document.clone(),
                phase: ReceiptPhase::Pending,
            };
            Self::write_receipt(&receipt_path, &receipt).await?;
            check(&cancel)?;
            self.resolve_file(path).await?;
            let mode = file_mode(&tokio::fs::symlink_metadata(&absolute).await?);
            atomic_write(&absolute, text, mode).await?;
            receipt.phase = ReceiptPhase::Completed;
            Self::write_receipt(&receipt_path, &receipt).await?;
            Ok(document)
        })
        .await
    }

    async fn reconcile_edit(
        &self,
        invocation: ToolInvocation,
        cancel: CancellationToken,
    ) -> anyhow::Result<Reconciliation> {
        let root = self.canonical_root().await?;
        serialized(&format!("workspace:{}", root.display()), async {
            check(&cancel)?;
            let receipt_path = self.receipt_path(&invocation.id).await?;
            let Some(mut receipt) = Self::read_receipt(&receipt_path).await? else {
                return Ok(Reconciliation::NotApplied);
            };
            if receipt.version != 1 || receipt.id != invocation.id {
                anyhow::bail!("Invalid edit receipt: {}", invocation.id);
            }
            if receipt.phase == ReceiptPhase::Completed {
                return Ok(Reconciliation::Completed { result: document_result(&receipt.document) });
            }
            let current = self.read(&receipt.path, cancel.clone()).await?;
            if current.revision == receipt.document.revision {
                receipt.phase = ReceiptPhase::Completed;
                Self::write_receipt(&receipt_path, &receipt).await?;
                return Ok(Reconciliation::Completed { result: document_result(&receipt.document) });
            }
            if current.revision == receipt.expected_revision {
                tokio::fs::remove_file(&receipt_path).await?;
                return Ok(Reconciliation::NotApplied);
            }
            Ok(Reconciliation::Unknown)
        })
        .await
    }
}
