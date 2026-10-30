//! Extension components in the interactive layout (upstream `setExtensionWidget`,
//! `setExtensionFooter`, `setExtensionHeader`, `showExtensionCustom`, and `resetExtensionUI`).
//! Factories build their components without blocking the UI loop.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use pi_js::HostValue;
use pi_tui::components::LoaderIndicatorOptions;
use pi_tui::{
    ComponentRef, Container, OverlayAnchor, OverlayMargin, OverlayOptions, SizeValue, Spacer, Text, component,
};
use serde_json::Value;

use super::components::ThemedText;
use super::extension_ui::UiMirror;
use super::mode::InteractiveMode;
use super::widget_stack::{SharedWidgetStack, WidgetStack};
use crate::extension_ui::WidgetPlacement;
use crate::extensions::{ComponentFactory, JsComponent};
use crate::theme::{ThemeColor, theme};

/// Most lines a text widget shows (upstream `MAX_WIDGET_LINES`).
const MAX_WIDGET_LINES: usize = 10;

/// What `setWidget` shows: text lines or a component factory.
pub(super) enum Widget {
    Lines(Vec<String>),
    Factory(ComponentFactory),
}

pub(super) type SharedJs = Rc<RefCell<JsComponent>>;

/// A mounted extension component and the JavaScript object to dispose of with it.
struct Mounted {
    component: ComponentRef,
    js: Option<SharedJs>,
}

impl Mounted {
    fn js(js: JsComponent) -> Self {
        let js = Rc::new(RefCell::new(js));
        Self { component: js.clone(), js: Some(js) }
    }

    fn dispose(&self) {
        if let Some(js) = &self.js {
            dispose(js);
        }
    }
}

pub(super) fn dispose(js: &SharedJs) {
    if let Ok(js) = js.try_borrow() {
        js.dispose();
    }
}

/// Extension UI state of the mode.
#[derive(Default)]
pub(super) struct ExtensionUiState {
    pub mirror: Arc<UiMirror>,
    widgets_above: Vec<(String, Mounted)>,
    widgets_below: Vec<(String, Mounted)>,
    widget_stack: Option<SharedWidgetStack>,
    /// Latest change of each slot; a component built for an older change is dropped.
    generations: HashMap<String, u64>,
    next_generation: u64,
    footer: Option<Mounted>,
    header: Option<Mounted>,
    pub editor: Option<SharedJs>,
    pub default_editor: Option<ComponentRef>,
    pub terminal_listener: Option<pi_tui::TuiListenerId>,
    pub working_message: Option<String>,
    pub working_indicator: Option<LoaderIndicatorOptions>,
    pub tool_invalidators: HashMap<String, Rc<dyn Fn()>>,
    pub shutdown_requested: bool,
    /// Overlays of `custom()` calls whose `onHandle` received a handle, by overlay id.
    pub overlays: HashMap<u64, pi_tui::OverlayHandle>,
    /// `addAutocompleteProvider` factories, applied in order over the built-in provider.
    pub autocomplete_factories: Vec<ComponentFactory>,
    pub autocomplete_base: Option<Rc<dyn pi_tui::AutocompleteProvider>>,
}

impl ExtensionUiState {
    pub fn widget_stack(&mut self) -> SharedWidgetStack {
        let mirror = self.mirror.clone();
        self.widget_stack.get_or_insert_with(|| WidgetStack::shared(mirror)).clone()
    }

    pub fn bump(&mut self, slot: &str) -> u64 {
        self.next_generation += 1;
        self.generations.insert(slot.to_string(), self.next_generation);
        self.next_generation
    }

    fn is_current(&self, slot: &str, generation: u64) -> bool {
        self.generations.get(slot) == Some(&generation)
    }
}

fn number(value: &Value) -> Option<i32> {
    value.as_f64().map(|number| number as i32)
}

fn size(value: &Value) -> Option<SizeValue> {
    match value {
        Value::String(text) => SizeValue::parse(text),
        value => number(value).map(SizeValue::Absolute),
    }
}

/// Upstream `OverlayOptions` from their JSON form.
pub(super) fn overlay_options(value: Option<&Value>) -> OverlayOptions {
    let Some(value) = value else { return OverlayOptions::default() };
    let anchor = match value["anchor"].as_str() {
        Some("top-left") => Some(OverlayAnchor::TopLeft),
        Some("top-right") => Some(OverlayAnchor::TopRight),
        Some("bottom-left") => Some(OverlayAnchor::BottomLeft),
        Some("bottom-right") => Some(OverlayAnchor::BottomRight),
        Some("top-center") => Some(OverlayAnchor::TopCenter),
        Some("bottom-center") => Some(OverlayAnchor::BottomCenter),
        Some("left-center") => Some(OverlayAnchor::LeftCenter),
        Some("right-center") => Some(OverlayAnchor::RightCenter),
        Some("center") => Some(OverlayAnchor::Center),
        _ => None,
    };
    let margin = match &value["margin"] {
        Value::Object(sides) => {
            let side = |name: &str| sides.get(name).and_then(number).unwrap_or(0);
            Some(OverlayMargin { top: side("top"), right: side("right"), bottom: side("bottom"), left: side("left") })
        }
        other => number(other).map(|all| OverlayMargin { top: all, right: all, bottom: all, left: all }),
    };
    OverlayOptions {
        width: size(&value["width"]),
        min_width: number(&value["minWidth"]),
        max_height: size(&value["maxHeight"]),
        anchor,
        offset_x: number(&value["offsetX"]),
        offset_y: number(&value["offsetY"]),
        row: size(&value["row"]),
        col: size(&value["col"]),
        margin,
        visible: None,
        non_capturing: value["nonCapturing"].as_bool().unwrap_or(false),
    }
}

fn text_widget(lines: &[String]) -> ComponentRef {
    let mut container = Container::new();
    for line in lines.iter().take(MAX_WIDGET_LINES) {
        container.add_child(component(Text::new(line, 1, 0)));
    }
    if lines.len() > MAX_WIDGET_LINES {
        let note = ThemedText::new(|| theme().fg(ThemeColor::Muted, "... (widget truncated)"), 1, 0);
        container.add_child(component(note));
    }
    component(container)
}

/// Call `method` on a JavaScript object when it has one.
pub(super) fn call_js(js: &SharedJs, method: &str, args: Vec<Value>) -> Option<Value> {
    let target = js.try_borrow().ok()?.js().clone();
    let args: Vec<HostValue> = args.into_iter().map(HostValue::from).collect();
    target.blocking_call_method(method, args).ok().map(HostValue::into_json)
}

impl InteractiveMode {
    pub(super) fn extensions_state(&self) -> std::cell::RefMut<'_, ExtensionUiState> {
        std::cell::RefMut::map(self.state.borrow_mut(), |state| &mut state.extensions)
    }

    /// Build `factory`'s component off the UI loop and hand it to `mount` if `slot` did not
    /// change meanwhile.
    pub(super) fn build_component(
        &self,
        slot: String,
        factory: ComponentFactory,
        mount: impl FnOnce(&Rc<Self>, JsComponent) + 'static,
    ) {
        let generation = self.extensions_state().bump(&slot);
        self.spawn(move |mode| async move {
            let built = factory.create_async().await;
            if !mode.extensions_state().is_current(&slot, generation) {
                if let Ok(js) = built {
                    js.dispose();
                }
                return;
            }
            match built {
                Ok(js) => mount(&mode, js),
                Err(error) => mode.show_error(&format!("Extension component failed: {error}")),
            }
            mode.request_render();
        });
    }

    pub(super) fn set_extension_widget(&self, key: &str, widget: Option<Widget>, placement: WidgetPlacement) {
        let slot = format!("widget:{key}");
        {
            let mut state = self.extensions_state();
            state.bump(&slot);
            let state = &mut *state;
            for list in [&mut state.widgets_above, &mut state.widgets_below] {
                if let Some(index) = list.iter().position(|(existing, _)| existing == key) {
                    list.remove(index).1.dispose();
                }
            }
        }
        let insert = move |mode: &InteractiveMode, key: String, mounted: Mounted| {
            let mut state = mode.extensions_state();
            match placement {
                WidgetPlacement::AboveEditor => state.widgets_above.push((key, mounted)),
                WidgetPlacement::BelowEditor => state.widgets_below.push((key, mounted)),
            }
        };
        match widget {
            None => {}
            Some(Widget::Lines(lines)) => {
                insert(self, key.to_string(), Mounted { component: text_widget(&lines), js: None });
            }
            Some(Widget::Factory(factory)) => {
                let key = key.to_string();
                self.build_component(slot, factory, move |mode, js| {
                    insert(mode, key, Mounted::js(js));
                    mode.render_widgets();
                });
            }
        }
        self.render_widgets();
    }

    /// Fill the widget containers; an empty one above the editor keeps a blank line. The widgets
    /// of both containers share one [`WidgetStack`] and its height budget.
    pub(super) fn render_widgets(&self) {
        let (stack, has_below) = {
            let state = &mut self.state.borrow_mut().extensions;
            let components = |list: &Vec<(String, Mounted)>| {
                list.iter().map(|(key, mounted)| (key.clone(), mounted.component.clone())).collect()
            };
            let (above, below) = (components(&state.widgets_above), components(&state.widgets_below));
            let stack = state.widget_stack();
            let has_below = !state.widgets_below.is_empty();
            let mut shared = stack.borrow_mut();
            shared.above = above;
            shared.below = below;
            drop(shared);
            (stack, has_below)
        };
        {
            let mut container = self.ui.widgets_above.borrow_mut();
            container.clear();
            container.add_child(component(Spacer::new(1)));
            container.add_child(WidgetStack::view_component(&stack, false));
        }
        {
            let mut container = self.ui.widgets_below.borrow_mut();
            container.clear();
            if has_below {
                container.add_child(WidgetStack::view_component(&stack, true));
            }
        }
        self.request_render();
    }

    pub(super) fn set_extension_footer(&self, factory: Option<ComponentFactory>) {
        if let Some(old) = self.extensions_state().footer.take() {
            old.dispose();
        }
        {
            let mut footer = self.ui.footer.borrow_mut();
            footer.clear();
            footer.add_child(self.footer.clone());
        }
        match factory {
            Some(factory) => self.build_component("footer".into(), factory, |mode, js| {
                let mounted = Mounted::js(js);
                {
                    let mut footer = mode.ui.footer.borrow_mut();
                    footer.clear();
                    footer.add_child(mounted.component.clone());
                }
                mode.extensions_state().footer = Some(mounted);
            }),
            None => drop(self.extensions_state().bump("footer")),
        }
        self.request_render();
    }

    /// The header component shown now: an extension's or the built-in one.
    fn current_header(&self) -> Option<ComponentRef> {
        let state = self.state.borrow();
        let custom = state.extensions.header.as_ref().map(|header| header.component.clone());
        custom.or_else(|| state.header.clone().map(|header| header as ComponentRef))
    }

    fn replace_header(&self, next: ComponentRef) {
        let current = self.current_header();
        let mut container = self.ui.header.borrow_mut();
        let index = current
            .and_then(|current| container.children.iter().position(|child| pi_tui::same_component(child, &current)));
        match index {
            Some(index) => container.children[index] = next,
            None => container.children.insert(0, next),
        }
    }

    pub(super) fn set_extension_header(&self, factory: Option<ComponentFactory>) {
        let builtin = self.state.borrow().header.clone();
        let Some(builtin) = builtin else { return };
        if let Some(factory) = factory {
            return self.build_component("header".into(), factory, |mode, js| {
                let mounted = Mounted::js(js);
                mode.replace_header(mounted.component.clone());
                if let Some(old) = mode.extensions_state().header.replace(mounted) {
                    old.dispose();
                }
            });
        }
        self.extensions_state().bump("header");
        let expanded = self.state.borrow().tool_output_expanded;
        builtin.borrow_mut().set_expanded(expanded);
        self.replace_header(builtin);
        if let Some(old) = self.extensions_state().header.take() {
            old.dispose();
        }
        self.request_render();
    }

    pub(super) fn invalidate_extension_tool(&self, tool_call_id: &str) {
        let invalidate = self.state.borrow().extensions.tool_invalidators.get(tool_call_id).cloned();
        if let Some(invalidate) = invalidate {
            invalidate();
        }
        self.request_render();
    }

    /// Drop what extensions put in the UI before their runtime goes away (upstream
    /// `resetExtensionUI`).
    pub(super) fn reset_extension_ui(&self) {
        self.reset_dialogs();
        self.tui().hide_overlay();
        self.reset_extension_overlays_and_autocomplete();
        self.set_extension_terminal_input(false);
        self.set_extension_footer(None);
        self.set_extension_header(None);
        let keys: Vec<String> = {
            let state = &self.state.borrow().extensions;
            state.widgets_above.iter().chain(&state.widgets_below).map(|(key, _)| key.clone()).collect()
        };
        for key in keys {
            self.set_extension_widget(&key, None, WidgetPlacement::AboveEditor);
        }
        self.footer_data.borrow_mut().clear_extension_statuses();
        self.footer_data.borrow_mut().clear_branch_listeners();
        let mirror = self.state.borrow().extensions.mirror.clone();
        mirror.set_footer(None, &Default::default(), 0);
        if self.has_custom_editor() {
            self.set_custom_editor(None);
        }
        self.editor.set_on_extension_shortcut(None);
        self.state.borrow_mut().extensions.tool_invalidators.clear();
        self.update_terminal_title();
        self.state.borrow_mut().working_visible = true;
        self.set_working_message(None);
        self.set_working_indicator(None);
        self.set_hidden_thinking_label(None);
        self.request_render();
    }
}
