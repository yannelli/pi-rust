//! Extension components for the TUI: factories registered through `ctx.ui` (custom dialogs,
//! widgets, header, footer, editor, autocomplete) and renderers, and the components they build.
//! Components render in JavaScript; [`JsComponent`] makes one a `pi_tui::Component`.

use std::sync::{Arc, Weak};

use pi_js::{HostValue, JsError, JsRef};
use pi_tui::{Component, ComponentRef};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use super::runner::ExtensionRunner;

/// What a factory builds; the factory's arguments follow upstream for each kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComponentKind {
    /// `(tui, theme, keybindings, done)`.
    Custom,
    /// `(tui, theme)`.
    Widget,
    /// `(tui, theme, footerData)`.
    Footer,
    /// `(tui, theme)`.
    Header,
    /// `(tui, editorTheme, keybindings)`; the result is an editor component.
    Editor,
}

/// A component factory an extension handed to the UI. Building runs JavaScript.
#[derive(Clone)]
pub struct ComponentFactory {
    runner: Weak<ExtensionRunner>,
    id: u64,
    target: u64,
    kind: ComponentKind,
}

impl std::fmt::Debug for ComponentFactory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ComponentFactory").field("id", &self.id).field("kind", &self.kind).finish()
    }
}

impl ComponentFactory {
    pub(crate) fn new(runner: &Arc<ExtensionRunner>, id: u64, target: u64, kind: ComponentKind) -> Self {
        Self { runner: Arc::downgrade(runner), id, target, kind }
    }

    pub fn kind(&self) -> ComponentKind {
        self.kind
    }

    fn args(&self) -> Vec<HostValue> {
        vec![json!(self.id).into(), json!(self.target).into(), Value::Null.into()]
    }

    fn host(&self) -> Result<JsRef, String> {
        let runner = self.runner.upgrade().ok_or("The extension host has shut down")?;
        Ok(runner.host().clone())
    }

    /// Build the component, waiting for the JavaScript thread. Call it off that thread, such as
    /// from the TUI; async factories are awaited.
    pub fn create(&self) -> Result<JsComponent, String> {
        let component = self.host()?.blocking_call_method_opt_ref("createComponent", self.args());
        wrap(component, self.kind)
    }

    pub async fn create_async(&self) -> Result<JsComponent, String> {
        let component = self.host()?.call_method_opt_ref("createComponent", self.args()).await;
        wrap(component, self.kind)
    }
}

fn wrap(component: Result<Option<JsRef>, JsError>, kind: ComponentKind) -> Result<JsComponent, String> {
    match component {
        Ok(Some(component)) => Ok(JsComponent::new(component)),
        Ok(None) => Err(format!("The {kind:?} factory returned no component")),
        Err(error) => Err(error.message),
    }
}

/// A `ctx.ui.custom()` call: show the component until its `done` callback delivers a result.
pub struct CustomRequest {
    pub factory: ComponentFactory,
    /// Show it as an overlay above the editor instead of replacing the editor.
    pub overlay: bool,
    /// Upstream `OverlayOptions`, as given.
    pub overlay_options: Option<Value>,
    /// Resolves with the value passed to `done`.
    pub done: oneshot::Receiver<Value>,
}

/// A JavaScript component (`render`, optional `handleInput`, `invalidate`, `dispose`). Calls
/// block on the JavaScript thread; a failing call renders nothing.
pub struct JsComponent {
    component: JsRef,
    handles_input: bool,
    wants_key_release: bool,
    focusable: bool,
}

impl JsComponent {
    pub fn new(component: JsRef) -> Self {
        let flags = component
            .handle()
            .blocking_with({
                let component = component.clone();
                move |scope| {
                    let value = scope.value(&component)?;
                    let object = value.as_object().cloned();
                    let has = |name: &str| {
                        object.as_ref().is_some_and(|object| {
                            object.get::<_, rquickjs::Value>(name).is_ok_and(|value| !value.is_undefined())
                        })
                    };
                    Ok((has("handleInput"), has("wantsKeyRelease"), has("focused")))
                }
            })
            .unwrap_or((false, false, false));
        Self { component, handles_input: flags.0, wants_key_release: flags.1, focusable: flags.2 }
    }

    /// The JavaScript object, for calling other methods.
    pub fn js(&self) -> &JsRef {
        &self.component
    }

    fn call(&self, method: &str, args: Vec<HostValue>) -> Option<Value> {
        self.component.blocking_call_method(method, args).ok().map(HostValue::into_json)
    }

    /// Call `dispose()` when the component has one.
    pub fn dispose(&self) {
        let _ = self.component.blocking_with_optional_method("dispose");
    }
}

/// Calling a method only when the object has it.
trait OptionalMethod {
    fn blocking_with_optional_method(&self, method: &str) -> Result<(), JsError>;
}

impl OptionalMethod for JsRef {
    fn blocking_with_optional_method(&self, method: &str) -> Result<(), JsError> {
        let (target, method) = (self.clone(), method.to_string());
        self.handle().blocking_with(move |scope| {
            let value = scope.value(&target)?;
            let Some(object) = value.as_object() else { return Ok(()) };
            let function: rquickjs::Value = object.get(method.as_str()).map_err(|error| scope.caught(error))?;
            if let Some(function) = function.as_function() {
                let _: rquickjs::Value =
                    function.call((rquickjs::function::This(value.clone()),)).map_err(|error| scope.caught(error))?;
            }
            Ok(())
        })
    }
}

impl Component for JsComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        match self.call("render", vec![json!(width).into()]) {
            Some(Value::Array(lines)) => lines.iter().map(|line| line.as_str().unwrap_or_default().to_string()).collect(),
            _ => Vec::new(),
        }
    }

    fn handle_input(&mut self, data: &str) {
        if self.handles_input {
            self.call("handleInput", vec![json!(data).into()]);
        }
    }

    fn handles_input(&self) -> bool {
        self.handles_input
    }

    fn wants_key_release(&self) -> bool {
        self.wants_key_release
    }

    fn invalidate(&mut self) {
        let _ = self.component.blocking_with_optional_method("invalidate");
    }

    fn is_focusable(&self) -> bool {
        self.focusable
    }

    fn set_focused(&mut self, focused: bool) {
        let target = self.component.clone();
        let _ = self.component.handle().blocking_with(move |scope| {
            let value = scope.value(&target)?;
            if let Some(object) = value.as_object() {
                object.set("focused", focused).map_err(|error| scope.caught(error))?;
            }
            Ok(())
        });
    }

    fn container_children(&self) -> Option<Vec<ComponentRef>> {
        None
    }
}
