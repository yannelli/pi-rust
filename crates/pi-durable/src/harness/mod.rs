//! The durable harness (upstream `harness/`): agents, model turns, tool calls, compaction, and the task scheduler
//! over a [`crate::session::Session`].

pub mod agent;
pub mod compaction;
pub mod context;
mod conversation;
mod core;
pub mod define;
pub mod generation;
mod host;
pub mod inbox;
mod json;
pub mod live;
pub mod output;
mod partial_changes;
pub mod progress;
pub mod prompt;
pub mod registry;
pub mod scheduler;
pub mod settings;
pub mod submissions;
pub mod task;
pub mod task_graph;
pub mod telemetry;
pub mod tool;
pub mod tool_registration;
pub mod transcript;
mod transcript_state;
pub mod types;
pub mod usage;
pub(crate) mod util;
pub mod view;
mod view_window;

pub use agent::{AGENT_DOC, INSTRUCTIONS_KEY, add_tools, configure, create_agent};
pub use conversation::{Conversation, CreateOptions, EntryRange};
pub use core::{
    ConversationCreatedHook, EnvBuilder, EnvTarget, HarnessOptions, NowFn, ReportFn, WakeFn, default_harness_identity,
};
pub use define::*;
pub use host::{ConversationInit, Harness};
pub use registry::{Registry, RegistryReader, RegistrySnapshot, builtin_tasks, create_registry};
pub use scheduler::{ConversationHandle, HookApi, IndexSizes, TaskRuntime};
pub use settings::*;
pub use submissions::Submission;
pub use task::{NextTaskState, Task, TaskDefinition, define_task, handler};
pub use task_graph::{TaskGraph, TaskGraphWatch};
pub use telemetry::DURABLE_TELEMETRY_SCHEMA;
pub use tool::{ExecuteToolOptions, ToolExecutionApi};
pub use tool_registration::*;
pub use transcript::{ConversationTranscript, EntryLoader, TranscriptItem, TranscriptView};
pub use types::*;
pub use view::{ConversationView, ConversationWatch, ConversationWindow};
