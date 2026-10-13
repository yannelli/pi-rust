//! Port of upstream `test/session-tables.test.ts`.

#[path = "../support/mod.rs"]
mod support;

mod owners;
mod tasks;

use std::sync::{Arc, LazyLock};

use pi_chord::Json;
use pi_durable::documents::*;
use pi_durable::session::{Session, TransactionScope};
use pi_durable::tasks::{TaskOptions, TaskTemplate};
use pi_durable::*;
use serde::{Deserialize, Serialize};
use support::*;

/// The `test.work` task kind: its first checkpoint is `{ phase: "start" }`.
pub struct WorkTask;

impl TaskTemplate for WorkTask {
    fn name(&self) -> &str {
        "test.work"
    }

    fn version(&self) -> u32 {
        1
    }

    fn initial(&self, _input: &Json) -> DurableResult<Json> {
        Ok(json(r#"{"phase":"start"}"#))
    }
}

pub fn work() -> Arc<dyn TaskTemplate> {
    Arc::new(WorkTask)
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    pub lines: Vec<String>,
}

pub static PROGRESS_DOC: LazyLock<DocToken<Progress, TaskScope>> =
    LazyLock::new(|| define_doc(DocSpec::new("test.progress", 1, TaskScope, Progress::default)).unwrap());

pub static STEP_DOC: LazyLock<DocFamilyToken<Progress, (), TaskScope>> = LazyLock::new(|| {
    define_doc_family(DocFamilySpec::new("test.step", 1, TaskScope, |(): ()| Progress::default())).unwrap()
});

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Notes {
    pub text: String,
}

pub static NOTES_DOC: LazyLock<DocToken<Notes, RewindableConversation>> = LazyLock::new(|| {
    define_doc(DocSpec::new("test.notes", 1, RewindableConversation(DocumentFork::AsOf), Notes::default)).unwrap()
});

/// The same task, terminal with a completed outcome.
pub fn terminal(task: &TaskRecord) -> TaskRecord {
    TaskRecord {
        state: TaskState::Terminal { outcome: TaskOutcome::completed(json(r#"{"ok":true}"#)) },
        owner: None,
        abort_reason: None,
        abandon_on_restart: None,
        memos: None,
        started_at: None,
        ended_at: None,
        ..task.clone()
    }
}

/// A pending `test.work` record.
pub fn pending(id: TaskId, conversation_id: ConversationId, path: &str, abort_requested: bool) -> TaskRecord {
    TaskRecord {
        id,
        conversation_id,
        kind: "test.work".into(),
        version: 1,
        input: json(&format!(r#"{{"path":"{path}"}}"#)),
        owner: None,
        background: false,
        abort_requested,
        abort_reason: None,
        abandon_on_restart: None,
        state: TaskState::Pending { checkpoint: json(r#"{"phase":"start"}"#) },
        memos: None,
        started_at: None,
        ended_at: None,
    }
}

pub async fn create_task(session: &Session, conversation_id: ConversationId, with_document: bool) -> TaskId {
    session
        .commit(
            move |tx| async move {
                let options = TaskOptions::conversation(conversation_id);
                let task_id = tx.create_task(work(), json(r#"{"path":"a"}"#), options).await?;
                if with_document {
                    tx.doc(&PROGRESS_DOC, task_id).await?.edit(|p| p.lines.push("started".into()))?;
                }
                Ok(task_id)
            },
            &context(),
        )
        .await
        .unwrap()
}

/// Replace a task record in an internal commit.
pub async fn replace_task(
    session: &Session,
    task_id: TaskId,
    change: fn(&TaskRecord) -> TaskRecord,
) -> DurableResult<()> {
    session
        .commit_with(
            move |tx| async move {
                let task = tx.task(task_id).await?.unwrap();
                tx.set_task(change(&task))
            },
            &context(),
            TransactionScope::default(),
        )
        .await
}
