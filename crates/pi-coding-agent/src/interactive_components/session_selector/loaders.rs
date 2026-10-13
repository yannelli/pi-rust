//! Ready-made session loaders (upstream `SessionManager.list` and `SessionManager.listAll` with
//! progress). Files are read on a blocking thread; progress reaches the UI through a channel.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use tokio_util::sync::CancellationToken;

use super::{SessionListProgress, SessionsFuture};
use crate::session::ids::resolve_path;
use crate::session::listing::{build_session_info, sort_session_infos};
use crate::session::storage::cwd_matches;
use crate::session::{SessionInfo, session_dir};

const CURRENT_PUBLISH_INTERVAL: usize = 10;
const ALL_PUBLISH_INTERVAL: usize = 100;

type Report<'a> = &'a dyn Fn(usize, usize, Option<Vec<SessionInfo>>);

fn jsonl_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(read_dir) = std::fs::read_dir(dir) else { return Vec::new() };
    read_dir
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "jsonl"))
        .collect()
}

fn sorted(mut sessions: Vec<SessionInfo>) -> Vec<SessionInfo> {
    sort_session_infos(&mut sessions);
    sessions
}

/// Reads `files` in order, reporting each one and publishing the partial list on the first
/// file, every `interval` files, and the last file.
fn load_infos(
    files: &[PathBuf],
    interval: usize,
    include: &dyn Fn(&SessionInfo) -> bool,
    report: Report,
    cancel: &CancellationToken,
) -> Vec<SessionInfo> {
    let total = files.len();
    let mut sessions = Vec::new();
    for (index, path) in files.iter().enumerate() {
        if cancel.is_cancelled() {
            break;
        }
        sessions.extend(build_session_info(path));
        let loaded = index + 1;
        let publish = loaded == 1 || loaded % interval == 0 || loaded == total;
        let partial = publish.then(|| sorted(sessions.iter().filter(|session| include(session)).cloned().collect()));
        report(loaded, total, partial);
    }
    sorted(sessions.into_iter().filter(|session| include(session)).collect())
}

fn sessions_in_dir(
    dir: &Path,
    include: &dyn Fn(&SessionInfo) -> bool,
    report: Report,
    cancel: &CancellationToken,
) -> Vec<SessionInfo> {
    let mut files = jsonl_files(dir);
    files.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    load_infos(&files, CURRENT_PUBLISH_INTERVAL, include, report, cancel)
}

/// Runs `job` on a blocking thread and forwards its progress to `progress` on this thread.
fn spawn_listing(
    job: impl FnOnce(Report, &CancellationToken) -> Vec<SessionInfo> + Send + 'static,
    progress: SessionListProgress,
    cancel: CancellationToken,
) -> SessionsFuture {
    Box::pin(async move {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let token = cancel.clone();
        let handle = tokio::task::spawn_blocking(move || {
            let report = move |loaded, total, partial| {
                let _ = sender.send((loaded, total, partial));
            };
            job(&report, &token)
        });
        while let Some((loaded, total, partial)) = receiver.recv().await {
            progress(loaded, total, partial.as_deref());
        }
        let sessions = handle.await?;
        if cancel.is_cancelled() {
            anyhow::bail!("Session listing was cancelled");
        }
        Ok(sessions)
    })
}

/// Loader for one project's sessions: `session_dir` (filtered by cwd when it is not the
/// project's default directory) or the default directory under `agent_dir`.
pub fn project_sessions_loader(
    cwd: PathBuf,
    session_dir_override: Option<PathBuf>,
    agent_dir: PathBuf,
) -> impl FnMut(SessionListProgress, CancellationToken) -> SessionsFuture + 'static {
    move |progress, cancel| {
        let (cwd, override_dir, agent_dir) = (cwd.clone(), session_dir_override.clone(), agent_dir.clone());
        let job = move |report: Report, cancel: &CancellationToken| {
            let default_dir = session_dir(&agent_dir, &cwd);
            let dir = override_dir.as_deref().map(resolve_path).unwrap_or_else(|| default_dir.clone());
            let (filter, resolved_cwd) = (dir != default_dir, resolve_path(&cwd));
            let include = |session: &SessionInfo| !filter || cwd_matches(&session.cwd, &resolved_cwd);
            sessions_in_dir(&dir, &include, report, cancel)
        };
        spawn_listing(job, progress, cancel)
    }
}

fn all_session_files(root: &Path) -> Vec<PathBuf> {
    let Ok(read_dir) = std::fs::read_dir(root) else { return Vec::new() };
    let mut files: Vec<(PathBuf, Option<SystemTime>)> = read_dir
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .flat_map(|dir| jsonl_files(&dir))
        .map(|path| {
            let time = std::fs::metadata(&path).and_then(|metadata| metadata.modified()).ok();
            (path, time)
        })
        .collect();
    files.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.file_name().cmp(&a.0.file_name())));
    files.into_iter().map(|(path, _)| path).collect()
}

/// Loader for every project's sessions under `<agent_dir>/sessions`, or every session in
/// `custom_dir` when the session directory is not the default one.
pub fn all_sessions_loader(
    custom_dir: Option<PathBuf>,
    agent_dir: PathBuf,
) -> impl FnMut(SessionListProgress, CancellationToken) -> SessionsFuture + 'static {
    move |progress, cancel| {
        let (custom_dir, agent_dir) = (custom_dir.clone(), agent_dir.clone());
        let job = move |report: Report, cancel: &CancellationToken| match custom_dir {
            Some(dir) => sessions_in_dir(&resolve_path(&dir), &|_| true, report, cancel),
            None => {
                let files = all_session_files(&resolve_path(&agent_dir).join("sessions"));
                load_infos(&files, ALL_PUBLISH_INTERVAL, &|_| true, report, cancel)
            }
        };
        spawn_listing(job, progress, cancel)
    }
}
