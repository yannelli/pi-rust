//! Extensions and their parts (upstream `defineExtension`, `section`, `hook`, `wrapTool`, and `wrapSection`).

use std::any::Any;
use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::sync::Arc;

use futures::future::BoxFuture;
use pi_chord::Context;

use super::task::Task;
use super::tool_registration::ToolRegistration;
use super::types::Agent;
use crate::env::SharedEnv;
use crate::errors::DurableResult;
use crate::ids::ConversationId;
use crate::session::Session;

/// Input to system prompt section rendering for one request preparation.
#[derive(Clone)]
pub struct PromptInput {
    pub conversation_id: ConversationId,
    /// The request's resolution; `agent.tools` are the tools offered in this request.
    pub agent: Agent,
    /// Built by `HarnessOptions::env` for this preparation.
    pub env: Option<SharedEnv>,
    /// Sections already in effect after replaying the active transcript.
    pub shown: BTreeMap<String, String>,
    /// Committed document reads.
    pub read: Session,
}

pub type RenderFn =
    Arc<dyn Fn(PromptInput, Context) -> BoxFuture<'static, DurableResult<Option<String>>> + Send + Sync>;

/// One system prompt section; the agent's sections render in order before each request.
#[derive(Clone)]
pub struct PromptSection {
    pub key: String,
    pub render: RenderFn,
    /// Default true: wrap the text as `<key>\n...\n</key>`.
    pub tag: Option<bool>,
}

impl fmt::Debug for PromptSection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PromptSection").field("key", &self.key).field("tag", &self.tag).finish()
    }
}

/// A prompt section; tagged unless `tag` is `Some(false)`.
pub fn section<F, Fut>(key: impl Into<String>, render: F, tag: Option<bool>) -> Arc<PromptSection>
where
    F: Fn(PromptInput, Context) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = DurableResult<Option<String>>> + Send + 'static,
{
    Arc::new(PromptSection {
        key: key.into(),
        render: Arc::new(move |input, context| Box::pin(render(input, context))),
        tag,
    })
}

/// A section that always renders `text`.
pub fn static_section(key: impl Into<String>, text: impl Into<String>) -> Arc<PromptSection> {
    let text = text.into();
    section(key, move |_, _| std::future::ready(Ok(Some(text.clone()))), None)
}

/// Hook handlers for tasks with one name; `handlers` is the task's hooks type, such as `GenerationHooks`.
#[derive(Clone)]
pub struct HookRegistration {
    pub task: String,
    pub handlers: Arc<dyn Any + Send + Sync>,
}

/// One selected extension's hook handlers for a task; derefs to the handlers.
pub struct ExtensionHooks<H: ?Sized> {
    /// The extension that registered them, named in reports of their failures.
    pub extension: String,
    pub handlers: Arc<H>,
}

impl<H: ?Sized> std::ops::Deref for ExtensionHooks<H> {
    type Target = H;

    fn deref(&self) -> &H {
        &self.handlers
    }
}

/// Hook handlers for tasks named like `task`.
pub fn hook<H: Any + Send + Sync>(task: &Task, handlers: H) -> HookRegistration {
    HookRegistration { task: task.name.clone(), handlers: Arc::new(handlers) }
}

pub type ToolWrapFn = Arc<dyn Fn(Arc<ToolRegistration>) -> DurableResult<Arc<ToolRegistration>> + Send + Sync>;
pub type SectionWrapFn = Arc<dyn Fn(Arc<PromptSection>) -> DurableResult<Arc<PromptSection>> + Send + Sync>;

/// Built by [`wrap_tool`] and [`wrap_section`]; targets a tool name or a section key. Wrappers are pure.
#[derive(Clone)]
pub enum Wrap {
    Tool { tool: String, wrap: ToolWrapFn },
    Section { section: String, wrap: SectionWrapFn },
}

/// Wrap the tool named `tool` wherever the wrapping extension is selected.
pub fn wrap_tool(
    tool: impl Into<String>,
    wrapper: impl Fn(Arc<ToolRegistration>) -> DurableResult<Arc<ToolRegistration>> + Send + Sync + 'static,
) -> Wrap {
    Wrap::Tool { tool: tool.into(), wrap: Arc::new(wrapper) }
}

/// Wrap the section `key` wherever the wrapping extension is selected.
pub fn wrap_section(
    key: impl Into<String>,
    wrapper: impl Fn(Arc<PromptSection>) -> DurableResult<Arc<PromptSection>> + Send + Sync + 'static,
) -> Wrap {
    Wrap::Section { section: key.into(), wrap: Arc::new(wrapper) }
}

/// Named bundle of code; installed in a registry and selected by conversations by name.
#[derive(Clone, Default)]
pub struct Extension {
    pub name: String,
    pub tools: Vec<Arc<ToolRegistration>>,
    pub sections: Vec<Arc<PromptSection>>,
    pub hooks: Vec<HookRegistration>,
    /// Apply where this extension is selected, in order.
    pub wraps: Vec<Wrap>,
    /// Resolved by name for every task, whichever conversations select this extension.
    pub tasks: Vec<Task>,
}

impl fmt::Debug for Extension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Extension").field("name", &self.name).finish_non_exhaustive()
    }
}

impl Extension {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), ..Self::default() }
    }

    pub fn tool(mut self, tool: impl Into<Arc<ToolRegistration>>) -> Self {
        self.tools.push(tool.into());
        self
    }

    pub fn section(mut self, section: Arc<PromptSection>) -> Self {
        self.sections.push(section);
        self
    }

    pub fn hook(mut self, hook: HookRegistration) -> Self {
        self.hooks.push(hook);
        self
    }

    pub fn wrap(mut self, wrap: Wrap) -> Self {
        self.wraps.push(wrap);
        self
    }

    pub fn task(mut self, task: Task) -> Self {
        self.tasks.push(task);
        self
    }

    pub fn into_arc(self) -> Arc<Self> {
        Arc::new(self)
    }
}

/// Define an extension; the identity function of upstream, kept for symmetry.
pub fn define_extension(extension: Extension) -> Arc<Extension> {
    Arc::new(extension)
}

/// The names of extensions or tools, for `AgentChange` lists.
pub fn names<T: Named + ?Sized>(items: &[Arc<T>]) -> Vec<String> {
    items.iter().map(|item| item.item_name().to_owned()).collect()
}

/// Something with a name: an extension or a tool.
pub trait Named {
    fn item_name(&self) -> &str;
}

impl Named for Extension {
    fn item_name(&self) -> &str {
        &self.name
    }
}

impl Named for ToolRegistration {
    fn item_name(&self) -> &str {
        self.name()
    }
}
