//! Pluggable file-system backends of the file tools (upstream `ReadOperations`,
//! `WriteOperations`, `EditOperations`, `GrepOperations`, `FindOperations`, `LsOperations`).
//! Embedders override them to point the tools at a remote system such as SSH.

use std::io::{self, ErrorKind};
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::AsyncReadExt;

use super::file_changes::SharedFileChangeRecorder;
use super::read::detect_supported_image_mime_type;

const IMAGE_TYPE_SNIFF_BYTES: usize = 4100;

/// Backend of the read tool.
#[async_trait]
pub trait ReadOperations: Send + Sync {
    /// File contents.
    async fn read_file(&self, absolute_path: &Path) -> anyhow::Result<Vec<u8>>;
    /// Fail when the file is not readable.
    async fn access(&self, absolute_path: &Path) -> anyhow::Result<()>;
    /// Image MIME type of the file, `None` for other files. Default: no images.
    async fn detect_image_mime_type(&self, _absolute_path: &Path) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
}

/// Backend of the write tool.
#[async_trait]
pub trait WriteOperations: Send + Sync {
    async fn write_file(&self, absolute_path: &Path, content: &str) -> anyhow::Result<()>;
    /// Create the directory and its parents.
    async fn mkdir(&self, dir: &Path) -> anyhow::Result<()>;
}

/// Backend of the edit tool.
#[async_trait]
pub trait EditOperations: Send + Sync {
    async fn read_file(&self, absolute_path: &Path) -> anyhow::Result<Vec<u8>>;
    async fn write_file(&self, absolute_path: &Path, content: &str) -> anyhow::Result<()>;
    /// Fail when the file is not readable and writable.
    async fn access(&self, absolute_path: &Path) -> anyhow::Result<()>;
}

/// Backend of the grep tool. The search itself runs on the local file system, as upstream
/// runs ripgrep locally; these cover the path check and the context lines.
#[async_trait]
pub trait GrepOperations: Send + Sync {
    /// Whether the path is a directory; fails when it does not exist.
    async fn is_directory(&self, absolute_path: &Path) -> anyhow::Result<bool>;
    /// File text, for context lines.
    async fn read_file(&self, absolute_path: &Path) -> anyhow::Result<String>;
}

/// Options of [`FindOperations::glob`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindGlobOptions {
    /// Glob patterns to skip, such as `**/node_modules/**`.
    pub ignore: Vec<String>,
    pub limit: usize,
}

/// Backend of the find tool.
#[async_trait]
pub trait FindOperations: Send + Sync {
    async fn exists(&self, absolute_path: &Path) -> anyhow::Result<bool>;
    /// Paths matching `pattern` under `cwd`, relative or absolute; directories may end in `/`.
    async fn glob(&self, pattern: &str, cwd: &Path, options: FindGlobOptions) -> anyhow::Result<Vec<String>>;
}

/// Backend of the ls tool.
#[async_trait]
pub trait LsOperations: Send + Sync {
    async fn exists(&self, absolute_path: &Path) -> anyhow::Result<bool>;
    /// Whether the path is a directory; fails when it does not exist.
    async fn is_directory(&self, absolute_path: &Path) -> anyhow::Result<bool>;
    /// Entry names of the directory.
    async fn read_dir(&self, absolute_path: &Path) -> anyhow::Result<Vec<String>>;
}

/// The local file system: the default backend of every file tool.
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalFileOperations;

#[async_trait]
impl ReadOperations for LocalFileOperations {
    async fn read_file(&self, absolute_path: &Path) -> anyhow::Result<Vec<u8>> {
        Ok(tokio::fs::read(absolute_path).await?)
    }

    async fn access(&self, absolute_path: &Path) -> anyhow::Result<()> {
        tokio::fs::File::open(absolute_path).await?;
        Ok(())
    }

    async fn detect_image_mime_type(&self, absolute_path: &Path) -> anyhow::Result<Option<String>> {
        let file = tokio::fs::File::open(absolute_path).await?;
        let mut sniff = Vec::with_capacity(IMAGE_TYPE_SNIFF_BYTES);
        file.take(IMAGE_TYPE_SNIFF_BYTES as u64).read_to_end(&mut sniff).await?;
        Ok(detect_supported_image_mime_type(&sniff).map(str::to_string))
    }
}

#[async_trait]
impl WriteOperations for LocalFileOperations {
    async fn write_file(&self, absolute_path: &Path, content: &str) -> anyhow::Result<()> {
        Ok(tokio::fs::write(absolute_path, content.as_bytes()).await?)
    }

    async fn mkdir(&self, dir: &Path) -> anyhow::Result<()> {
        Ok(tokio::fs::create_dir_all(dir).await?)
    }
}

#[async_trait]
impl EditOperations for LocalFileOperations {
    async fn read_file(&self, absolute_path: &Path) -> anyhow::Result<Vec<u8>> {
        Ok(tokio::fs::read(absolute_path).await?)
    }

    async fn write_file(&self, absolute_path: &Path, content: &str) -> anyhow::Result<()> {
        Ok(tokio::fs::write(absolute_path, content.as_bytes()).await?)
    }

    async fn access(&self, absolute_path: &Path) -> anyhow::Result<()> {
        let meta = tokio::fs::metadata(absolute_path).await?;
        if meta.is_dir() {
            return Err(io::Error::from(ErrorKind::IsADirectory).into());
        }
        if meta.permissions().readonly() {
            return Err(io::Error::from(ErrorKind::PermissionDenied).into());
        }
        Ok(())
    }
}

#[async_trait]
impl GrepOperations for LocalFileOperations {
    async fn is_directory(&self, absolute_path: &Path) -> anyhow::Result<bool> {
        Ok(tokio::fs::metadata(absolute_path).await?.is_dir())
    }

    async fn read_file(&self, absolute_path: &Path) -> anyhow::Result<String> {
        let bytes = tokio::fs::read(absolute_path).await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

#[async_trait]
impl FindOperations for LocalFileOperations {
    async fn exists(&self, absolute_path: &Path) -> anyhow::Result<bool> {
        Ok(tokio::fs::try_exists(absolute_path).await.unwrap_or(false))
    }

    /// The native glob search the find tool runs without custom operations.
    async fn glob(&self, pattern: &str, cwd: &Path, options: FindGlobOptions) -> anyhow::Result<Vec<String>> {
        let (pattern, cwd) = (pattern.to_string(), cwd.to_path_buf());
        let cancel = tokio_util::sync::CancellationToken::new();
        tokio::task::spawn_blocking(move || {
            super::find::native_glob(&pattern, &cwd, options.limit, &options.ignore, &cancel)
        })
        .await?
    }
}

#[async_trait]
impl LsOperations for LocalFileOperations {
    async fn exists(&self, absolute_path: &Path) -> anyhow::Result<bool> {
        Ok(tokio::fs::try_exists(absolute_path).await.unwrap_or(false))
    }

    async fn is_directory(&self, absolute_path: &Path) -> anyhow::Result<bool> {
        Ok(tokio::fs::metadata(absolute_path).await?.is_dir())
    }

    async fn read_dir(&self, absolute_path: &Path) -> anyhow::Result<Vec<String>> {
        let mut reader = tokio::fs::read_dir(absolute_path).await?;
        let mut entries = Vec::new();
        while let Some(entry) = reader.next_entry().await? {
            entries.push(entry.file_name().to_string_lossy().into_owned());
        }
        Ok(entries)
    }
}

macro_rules! tool_options {
    ($(#[$doc:meta])* $name:ident, $ops:ident $(, $(#[$field_doc:meta])* $field:ident: $ty:ty)*) => {
        $(#[$doc])*
        #[derive(Clone, Default)]
        pub struct $name {
            /// Custom backend. Default: the local file system.
            pub operations: Option<Arc<dyn $ops>>,
            $($(#[$field_doc])* pub $field: Option<$ty>,)*
        }

        impl $name {
            pub fn with_operations(operations: Arc<dyn $ops>) -> Self {
                Self { operations: Some(operations), $($field: None,)* }
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct(stringify!($name))
                    .field("custom_operations", &self.operations.is_some())
                    $(.field(stringify!($field), &self.$field.is_some()))*
                    .finish()
            }
        }
    };
}

tool_options!(
    /// Options of the write tool (upstream `WriteToolOptions`).
    WriteToolOptions,
    WriteOperations,
    /// Receives each change, for the `/undo` checkpoints (pi-rust extension).
    recorder: SharedFileChangeRecorder
);
tool_options!(
    /// Options of the edit tool (upstream `EditToolOptions`).
    EditToolOptions,
    EditOperations,
    /// Receives each change, for the `/undo` checkpoints (pi-rust extension).
    recorder: SharedFileChangeRecorder
);
tool_options!(
    /// Options of the grep tool (upstream `GrepToolOptions`).
    GrepToolOptions,
    GrepOperations
);
tool_options!(
    /// Options of the find tool (upstream `FindToolOptions`).
    FindToolOptions,
    FindOperations
);
tool_options!(
    /// Options of the ls tool (upstream `LsToolOptions`).
    LsToolOptions,
    LsOperations
);

macro_rules! resolve_local {
    ($($name:ident: $ops:ident),*) => {$(
        impl $name {
            /// The custom backend or the local file system.
            pub(crate) fn resolve(&self) -> Arc<dyn $ops> {
                self.operations.clone().unwrap_or_else(|| Arc::new(LocalFileOperations))
            }
        }
    )*};
}

// The find tool runs its native search without custom operations, so it has no `resolve`.
resolve_local!(WriteToolOptions: WriteOperations, EditToolOptions: EditOperations, GrepToolOptions: GrepOperations, LsToolOptions: LsOperations);

/// Upstream-style error text of a backend failure: `Error code: ENOENT` for I/O errors,
/// `Error: <message>` for others.
pub(crate) fn describe_error(error: &anyhow::Error) -> String {
    match error.downcast_ref::<io::Error>() {
        Some(io) => format!("Error code: {}", io_error_code(io.kind())),
        None => format!("Error: {error}"),
    }
}

pub(crate) fn io_error_code(kind: ErrorKind) -> String {
    match kind {
        ErrorKind::NotFound => "ENOENT".into(),
        ErrorKind::PermissionDenied => "EACCES".into(),
        ErrorKind::IsADirectory => "EISDIR".into(),
        other => format!("{other:?}"),
    }
}
