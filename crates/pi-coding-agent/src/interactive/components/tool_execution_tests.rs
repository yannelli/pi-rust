//! Streamed argument rendering (upstream `tool-execution-component.test.ts`, #9265 case).

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

use pi_tui::utils::strip_terminal_sequences;
use pi_tui::{Component, ComponentRef, Tui};
use serde_json::{Value, json};

use super::{ToolDisplay, ToolExecutionComponent, ToolExecutionOptions};
use crate::interactive::renderers::{ToolRenderContext, ToolRenderer, ToolResultView, WriteRenderer};
use crate::interactive_components::test_support::Harness;

/// The write renderer, counting how often it draws the call.
struct Counted(Rc<Cell<usize>>);

impl ToolRenderer for Counted {
    fn render_call(&self, context: &mut ToolRenderContext<'_>) -> ComponentRef {
        self.0.set(self.0.get() + 1);
        WriteRenderer.render_call(context)
    }

    fn render_result(&self, result: &ToolResultView, context: &mut ToolRenderContext<'_>) -> Option<ComponentRef> {
        WriteRenderer.render_result(result, context)
    }
}

fn write_component(tui: &Tui, args: Value, renderer: Rc<dyn ToolRenderer>) -> ToolExecutionComponent {
    let display = ToolDisplay::Renderer(renderer);
    ToolExecutionComponent::new(
        "write",
        "tool-9",
        args,
        ToolExecutionOptions::default(),
        display,
        tui,
        PathBuf::from("."),
    )
}

fn shown(component: &mut ToolExecutionComponent) -> String {
    component.render(120).iter().map(|line| strip_terminal_sequences(line)).collect::<Vec<_>>().join("\n")
}

#[test]
fn renders_streamed_partial_arguments_once_per_frame_with_the_latest_arguments() {
    let harness = Harness::new(120, 10, |_| pi_tui::Spacer::new(0));
    let tui = &harness.tui;
    let calls = Rc::new(Cell::new(0));
    let mut component = write_component(tui, json!({}), Rc::new(Counted(calls.clone())));
    let before = calls.get();
    let mut content = String::new();
    for index in 0..200 {
        content.push_str(&format!("const line{index} = {index};\n"));
        component.update_args(json!({"path": "src/file.ts", "content": content}));
    }
    assert_eq!(calls.get(), before);

    let streamed = shown(&mut component);
    assert_eq!(calls.get() - before, 1);
    let direct = |args: Value| shown(&mut write_component(tui, args, Rc::new(WriteRenderer)));
    assert_eq!(streamed, direct(json!({"path": "src/file.ts", "content": content})));

    // More deltas before the next frame: one more renderer run, which shows them all.
    component.update_args(json!({"path": "src/file.ts", "content": format!("{content}const tail = 1;\n")}));
    let last = json!({"path": "src/file.ts", "content": format!("{content}const tail = 2;\n")});
    component.update_args(last.clone());
    let next = shown(&mut component);
    assert_eq!(calls.get() - before, 2);
    assert!(next.contains("201 total"), "{next}");
    assert_eq!(next, direct(last));
    // Rendering again without new arguments does not run the renderer.
    shown(&mut component);
    assert_eq!(calls.get() - before, 2);
}
