//! Components of the interactive chat (upstream `modes/interactive/components/*`). Selectors and
//! dialogs live in `crate::interactive_components`.

pub mod bash_execution;
pub mod bordered_loader;
pub mod custom_editor;
pub mod diff;
pub mod footer;
pub mod messages;
pub mod pi_logo;
pub mod status_indicator;
pub mod summaries;
pub mod themed_text;
pub mod tool_execution;
pub mod visual_truncate;

use pi_tui::ComponentRef;
use pi_tui::components::markdown::MarkdownTheme;

use crate::markdown_transform::MarkdownTransformer;
use crate::theme::get_markdown_theme;

pub use bash_execution::BashExecutionComponent;
pub use custom_editor::{CustomEditor, EditorHandle};
pub use footer::FooterComponent;
pub use messages::{AssistantMessageComponent, UserMessageComponent};
pub use status_indicator::{IdleStatus, SharedIndicator, StatusIndicator, StatusKind};
pub use summaries::{
    Collapsible, CollapsibleMessage, CustomEntryComponent, CustomEntryRenderer, CustomMessageComponent,
    CustomMessageRenderer,
};
pub use themed_text::{ExpandableText, ThemedText};
pub use tool_execution::{ToolDisplay, ToolExecutionComponent, ToolExecutionOptions};

/// Markdown styling and transformers shared by the message components.
#[derive(Clone)]
pub struct RenderOptions {
    pub markdown_theme: MarkdownTheme,
    pub output_pad: usize,
    pub transformers: Vec<MarkdownTransformer>,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self { markdown_theme: get_markdown_theme(), output_pad: 1, transformers: Vec::new() }
    }
}

/// Upstream `isExpandable(child) && child.setExpanded(expanded)` for chat children.
pub fn set_component_expanded(component: &ComponentRef, expanded: bool) -> bool {
    let Ok(mut component) = component.try_borrow_mut() else { return false };
    let any = component.as_any_mut();
    if let Some(tool) = any.downcast_mut::<ToolExecutionComponent>() {
        tool.set_expanded(expanded);
    } else if let Some(bash) = any.downcast_mut::<BashExecutionComponent>() {
        bash.set_expanded(expanded);
    } else if let Some(message) = any.downcast_mut::<CollapsibleMessage>() {
        message.set_expanded(expanded);
    } else if let Some(message) = any.downcast_mut::<CustomMessageComponent>() {
        message.set_expanded(expanded);
    } else if let Some(entry) = any.downcast_mut::<CustomEntryComponent>() {
        entry.set_expanded(expanded);
    } else if let Some(text) = any.downcast_mut::<ExpandableText>() {
        text.set_expanded(expanded);
    } else {
        return false;
    }
    true
}

/// Upstream `"setOutputPad" in child && child.setOutputPad(padding)` for chat children.
pub fn set_component_output_pad(component: &ComponentRef, padding: usize) {
    let Ok(mut component) = component.try_borrow_mut() else { return };
    let any = component.as_any_mut();
    if let Some(tool) = any.downcast_mut::<ToolExecutionComponent>() {
        tool.set_output_pad(padding);
    } else if let Some(bash) = any.downcast_mut::<BashExecutionComponent>() {
        bash.set_output_pad(padding);
    } else if let Some(message) = any.downcast_mut::<AssistantMessageComponent>() {
        message.set_output_pad(padding);
    } else if let Some(message) = any.downcast_mut::<CollapsibleMessage>() {
        message.set_output_pad(padding);
    } else if let Some(entry) = any.downcast_mut::<CustomEntryComponent>() {
        entry.set_output_pad(padding);
    }
}

/// Applies `apply` to each tool execution component among `children`.
pub fn for_each_tool(children: &[ComponentRef], mut apply: impl FnMut(&mut ToolExecutionComponent)) {
    for child in children {
        if let Ok(mut child) = child.try_borrow_mut()
            && let Some(tool) = child.as_any_mut().downcast_mut::<ToolExecutionComponent>()
        {
            apply(tool);
        }
    }
}
