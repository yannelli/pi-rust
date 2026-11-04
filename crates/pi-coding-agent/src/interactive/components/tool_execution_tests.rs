//! Streamed argument rendering (upstream `tool-execution-component.test.ts`, #9265 case).

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

use pi_tui::utils::strip_terminal_sequences;
use pi_tui::{Component, ComponentRef, Tui};
use serde_json::{Value, json};

use super::{ToolDisplay, ToolExecutionComponent, ToolExecutionOptions};
use crate::interactive::renderers::{
    ToolRenderContext, ToolRenderer, ToolResultExpander, ToolResultView, WriteRenderer,
};
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

/// Claims results whose details hold `stored` and draws 30 numbered lines with a limit of 6.
struct Lines;

impl ToolResultExpander for Lines {
    fn expand(
        &self,
        _tool_name: &str,
        result: &ToolResultView,
        _context: &mut ToolRenderContext<'_>,
    ) -> Option<(ComponentRef, usize)> {
        result.detail("stored")?;
        let text: Vec<String> = (1..=30).map(|n| format!("original {n}")).collect();
        Some((pi_tui::component(pi_tui::Text::new(&text.join("\n"), 0, 0)), 6))
    }
}

fn wheel_at(y: i32, delta: i32) -> pi_tui::TuiMouseEvent {
    pi_tui::TuiMouseEvent {
        kind: pi_tui::TuiMouseEventType::Wheel,
        button: pi_tui::TuiMouseButton::None,
        x: 4,
        y,
        screen_x: 4,
        screen_y: y,
        width: 120,
        height: 20,
        shift: false,
        alt: false,
        ctrl: false,
        wheel_delta: Some(delta),
        click_count: None,
    }
}

#[test]
fn an_extension_draws_the_expanded_body_with_a_height_limit() {
    let harness = Harness::new(120, 10, |_| pi_tui::Spacer::new(0));
    let mut component = ToolExecutionComponent::new(
        "mcp_tool",
        "tool-x",
        json!({}),
        ToolExecutionOptions::default(),
        ToolDisplay::Generic,
        &harness.tui,
        PathBuf::from("."),
    );
    component.set_result_expander(Some(Rc::new(Lines)));
    let view = |details: Value| ToolResultView {
        content: vec![pi_ai::ContentBlock::text("shortened")],
        details: Some(details),
        ..ToolResultView::default()
    };
    component.update_result(view(json!({"stored": true})), false);
    let collapsed = shown(&mut component);
    assert!(collapsed.contains("shortened") && !collapsed.contains("original 1"), "{collapsed}");

    component.set_expanded(true);
    let open = shown(&mut component);
    assert!(open.contains("original 5") && !open.contains("original 6"), "{open}");
    assert!(open.contains("lines 1-5 of 30") && !open.contains("shortened"), "{open}");

    let body_row = component.render(120).iter().position(|line| line.contains("original 1")).unwrap() as i32;
    assert!(component.handle_mouse(&wheel_at(body_row, 4)).is_some());
    let scrolled = shown(&mut component);
    assert!(scrolled.contains("original 5") && scrolled.contains("original 9"), "{scrolled}");
    assert!(scrolled.contains("lines 5-9 of 30") && !scrolled.contains("original 4\n"), "{scrolled}");

    component.set_expanded(false);
    assert!(shown(&mut component).contains("shortened"));
    component.set_expanded(true);
    assert!(shown(&mut component).contains("lines 1-5 of 30"), "collapse resets the scroll");

    component.update_result(view(json!({})), false);
    let plain = shown(&mut component);
    assert!(plain.contains("shortened") && !plain.contains("original"), "{plain}");
}
