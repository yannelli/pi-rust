//! Output files: full text of truncated command output, written to the temp directory and
//! readable only by the user (upstream `utils/output-files.ts`).

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Creates `path` as a directory that only the user can enter: the temp directory is shared with
/// other users, and these directories hold session exports and prompts.
pub(crate) fn create_private_dir(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    std::fs::create_dir(path)
}

/// Where streamed output ended up: a complete file, or why it could not be saved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputFileResult {
    Saved(PathBuf),
    Failed(String),
}

impl OutputFileResult {
    pub fn saved_path(&self) -> Option<&Path> {
        match self {
            Self::Saved(path) => Some(path),
            Self::Failed(_) => None,
        }
    }

    pub fn error(&self) -> Option<&str> {
        match self {
            Self::Saved(_) => None,
            Self::Failed(error) => Some(error),
        }
    }
}

/// An output file written while output streams in (upstream `OutputFileStream`). Open and write
/// failures (full disk, quota, unwritable temp dir) never fail the caller: the first one drops
/// the file, and [`OutputFile::close`] reports why.
pub(crate) struct OutputFile {
    path: PathBuf,
    file: Option<File>,
    result: OutputFileResult,
}

impl OutputFile {
    /// A new file `<temp dir>/<prefix>-<random hex><extension>`.
    pub fn create(prefix: &str, extension: &str) -> Self {
        Self::create_in(&std::env::temp_dir(), prefix, extension)
    }

    pub fn create_in(dir: &Path, prefix: &str, extension: &str) -> Self {
        let name = format!("{prefix}-{}{extension}", crate::session::ids::random_hex(16));
        let path = dir.join(name);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        match options.open(&path) {
            Ok(file) => Self { result: OutputFileResult::Saved(path.clone()), path, file: Some(file) },
            // A file already at the path is not ours, so it stays.
            Err(error) => {
                Self { result: OutputFileResult::Failed(node_error(&error, "open", Some(&path))), path, file: None }
            }
        }
    }

    /// `Saved` while writing has not failed; the file is complete only after [`Self::close`].
    pub fn result(&self) -> &OutputFileResult {
        &self.result
    }

    pub fn write(&mut self, data: &[u8]) {
        if let Some(file) = &mut self.file
            && let Err(error) = file.write_all(data)
        {
            self.fail(&error, "write");
        }
    }

    /// Close the file. Writes are unbuffered, so nothing is left to flush.
    pub fn close(&mut self) -> OutputFileResult {
        self.file = None;
        self.result.clone()
    }

    /// Drop the file after the first failure: a partial file would pass for the full output.
    fn fail(&mut self, error: &std::io::Error, syscall: &str) {
        if self.file.take().is_some() {
            let _ = std::fs::remove_file(&self.path);
        }
        if let OutputFileResult::Saved(_) = self.result {
            self.result = OutputFileResult::Failed(node_error(error, syscall, None));
        }
    }
}

/// Node's message for a failed file call, such as `ENOSPC: no space left on device, write`.
fn node_error(error: &std::io::Error, syscall: &str, path: Option<&Path>) -> String {
    let path = path.map(|path| path.display().to_string());
    pi_js::JsError::from_io(error, syscall, path.as_deref()).message
}

#[cfg(test)]
mod tests {
    use super::*;

    // Upstream #10585: a full, over-quota, or unwritable temp dir must not fail the caller.
    #[test]
    fn reports_an_open_failure_from_close() {
        let root = tempfile::tempdir().unwrap();
        let mut file = OutputFile::create_in(&root.path().join("missing"), "pi-test", ".log");
        file.write(b"data");
        let error = file.close().error().map(str::to_string).expect("a failed file");
        assert!(error.starts_with("ENOENT: no such file or directory, open '"), "{error}");
        assert!(!file.path.exists());
    }

    #[test]
    fn drops_a_partially_written_file_after_a_write_failure() {
        let root = tempfile::tempdir().unwrap();
        let mut file = OutputFile::create_in(root.path(), "pi-test", ".log");
        file.write(b"data");
        assert!(file.path.exists());
        file.fail(&std::io::Error::from(std::io::ErrorKind::StorageFull), "write");
        file.write(b"more");
        let expected = OutputFileResult::Failed("ENOSPC: no space left on device, write".into());
        assert_eq!(file.close(), expected);
        assert!(!file.path.exists());
    }

    #[test]
    fn saves_the_full_output_when_writing_succeeds() {
        let root = tempfile::tempdir().unwrap();
        let mut file = OutputFile::create_in(root.path(), "pi-test", ".log");
        file.write(b"data");
        let path = file.path.clone();
        assert_eq!(file.close(), OutputFileResult::Saved(path.clone()));
        assert_eq!(file.result(), &OutputFileResult::Saved(path.clone()));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "data");
    }
}
