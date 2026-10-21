//! The result's files (the full output, images) in the run's output directory, created once and recorded in the run
//! document before any file is written, so a rerun after a crash writes the same files again.

use futures::future::BoxFuture;
use pi_chord::Context;
use tokio::sync::Mutex;

use super::docs::CODEMODE_RUN_DOC;
use super::output::OutputFile;
use crate::env::ExecutionEnv;
use crate::harness::ToolExecutionApi;
use crate::session::Tx;

/// Writes the result's files under fixed names into the run's output directory.
pub(super) struct OutputFiles {
    api: ToolExecutionApi,
    context: Context,
    /// The directory, or why it could not be made; looked up once.
    directory: Mutex<Option<Result<String, String>>>,
}

impl OutputFiles {
    pub fn new(api: ToolExecutionApi, context: Context) -> Self {
        Self { api, context, directory: Mutex::new(None) }
    }

    /// The run's output directory: the recorded one, else a new temp directory, recorded before it is used.
    async fn output_dir(&self, env: &dyn ExecutionEnv) -> Result<String, String> {
        let task = self.api.task_id;
        let read = move |tx: Tx| async move { Ok(tx.doc(&CODEMODE_RUN_DOC, task).await?.get()?.output_dir) };
        if let Some(recorded) = self.api.commit(read, &self.context).await.map_err(|error| error.to_string())? {
            return Ok(recorded);
        }
        let created = env.create_temp_dir(Some("pi-codemode-"), &self.context).await.map_err(|error| error.message)?;
        let record = move |tx: Tx| async move {
            let run = tx.doc(&CODEMODE_RUN_DOC, task).await?;
            run.edit(|state| {
                state.output_dir.get_or_insert_with(|| created.clone());
            })?;
            Ok(run.get()?.output_dir)
        };
        let recorded = self.api.commit(record, &self.context).await.map_err(|error| error.to_string())?;
        recorded.ok_or_else(|| "the output directory was not recorded".to_owned())
    }
}

impl OutputFile for OutputFiles {
    fn write<'a>(&'a self, name: &'a str, data: Vec<u8>) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let Some(env) = self.api.env.clone() else { return Err("no execution environment".to_owned()) };
            let directory = {
                let mut directory = self.directory.lock().await;
                if directory.is_none() {
                    *directory = Some(self.output_dir(&*env).await);
                }
                directory.clone().unwrap_or_else(|| Err("no output directory".to_owned()))?
            };
            let path = format!("{directory}/{name}");
            env.write_file(&path, &data, &self.context).await.map_err(|error| error.message)?;
            Ok(path)
        })
    }
}
