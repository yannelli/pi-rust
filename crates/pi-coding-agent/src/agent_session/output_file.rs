//! Output files: full text of truncated command output, written to the temp directory and
//! readable only by the user (upstream `utils/output-files.ts`).

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

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
