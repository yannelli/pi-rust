//! Output files: full text of truncated command output, written to the temp directory and
//! readable only by the user (upstream `utils/output-files.ts`).

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

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

pub(crate) struct OutputFile {
    path: PathBuf,
    file: File,
}

impl OutputFile {
    /// A new file `<temp dir>/<prefix>-<random hex><extension>`; `None` when it cannot be created.
    pub fn create(prefix: &str, extension: &str) -> Option<Self> {
        let name = format!("{prefix}-{}{extension}", crate::session::ids::random_hex(16));
        let path = std::env::temp_dir().join(name);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let file = options.open(&path).ok()?;
        Some(Self { path, file })
    }

    pub fn write(&mut self, text: &str) {
        let _ = self.file.write_all(text.as_bytes());
    }

    /// Flush and return the path.
    pub fn finish(mut self) -> PathBuf {
        let _ = self.file.flush();
        self.path
    }
}
