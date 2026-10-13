//! `ctx.ui.custom()` with its overlay handle, and the autocomplete providers extensions stack
//! (upstream `showExtensionCustom`, `OverlayHandle`, and `addAutocompleteProvider`).

use std::cell::RefCell;
use std::rc::Rc;

use pi_tui::{AutocompleteProvider, ComponentRef, OverlayHandle, OverlayOptions};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use super::extension_view::{dispose, overlay_options};
use super::mode::InteractiveMode;
use crate::extensions::autocomplete::JsAutocompleteProvider;
use crate::extensions::{ComponentFactory, CustomRequest, JsComponent};

/// What extensions read about an overlay without waiting for the UI thread.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct OverlayMirror {
    pub hidden: bool,
    pub focused: bool,
    pub bounds: Value,
}

fn mirror_of(handle: &OverlayHandle) -> OverlayMirror {
    let bounds = handle.get_bounds().map_or(
        Value::Null,
        |bounds| json!({"row": bounds.row, "col": bounds.col, "width": bounds.width, "height": bounds.height}),
    );
    OverlayMirror { hidden: handle.is_hidden(), focused: handle.is_focused(), bounds }
}

/// The overlay options, else the component's `width` property (upstream fallback).
fn custom_overlay_options(options: Option<&Value>, js: &JsComponent) -> OverlayOptions {
    if options.is_some() {
        return overlay_options(options);
    }
    let reference = js.js().clone();
    let width = js
        .js()
        .handle()
        .blocking_with(move |scope| {
            let value = scope.value(&reference)?;
            Ok(value.as_object().and_then(|object| object.get::<_, f64>("width").ok()))
        })
        .ok()
        .flatten();
    match width {
        Some(width) if width > 0.0 => overlay_options(Some(&json!({"width": width}))),
        _ => OverlayOptions::default(),
    }
}

impl InteractiveMode {
    /// `ctx.ui.custom()`: show the component until it calls `done`, then answer `sender`. A
    /// factory that fails rejects the call.
    pub(super) fn show_extension_custom(&self, request: CustomRequest, sender: oneshot::Sender<Result<Value, String>>) {
        let saved_text = self.current_editor_text();
        self.spawn(move |mode| async move {
            let CustomRequest { factory, overlay, overlay_options: options, done, handle_key } = request;
            let js = match factory.create_async().await {
                Ok(js) => Rc::new(RefCell::new(js)),
                Err(error) => {
                    let _ = sender.send(Err(error));
                    return;
                }
            };
            let view: ComponentRef = js.clone();
            let overlay_id = factory.id();
            let close: Rc<dyn Fn()> = if overlay {
                let options = custom_overlay_options(options.as_ref(), &js.borrow());
                let handle = mode.tui().show_overlay(view, options);
                if let Some(key) = handle_key {
                    mode.register_extension_overlay(overlay_id, handle.clone());
                    let _ = factory.invoke_callback(key, json!(overlay_id)).await;
                }
                Rc::new(move || handle.hide())
            } else {
                mode.mount_view(view)
            };
            mode.request_render();
            let result = done.await.unwrap_or(Value::Null);
            close();
            mode.unregister_extension_overlay(overlay_id);
            if !overlay {
                mode.set_extension_editor_text(&saved_text);
            }
            dispose(&js);
            mode.request_render();
            let _ = sender.send(Ok(result));
        });
    }

    fn register_extension_overlay(&self, id: u64, handle: OverlayHandle) {
        let mirror = self.state.borrow().extensions.mirror.clone();
        mirror.set_overlay(id, Some(mirror_of(&handle)));
        self.extensions_state().overlays.insert(id, handle);
    }

    fn unregister_extension_overlay(&self, id: u64) {
        let mirror = self.state.borrow().extensions.mirror.clone();
        mirror.set_overlay(id, None);
        self.extensions_state().overlays.remove(&id);
    }

    /// An action of an overlay handle: `hide`, `setHidden`, `focus`, or `unfocus`.
    pub(super) fn extension_overlay_action(&self, id: u64, method: &str, arg: &Value) {
        let Some(handle) = self.extensions_state().overlays.get(&id).cloned() else { return };
        match method {
            "hide" => handle.hide(),
            "setHidden" => handle.set_hidden(arg.as_bool().unwrap_or(false)),
            "focus" => handle.focus(),
            "unfocus" => handle.unfocus(None),
            _ => return,
        }
        self.sync_extension_overlays();
        self.request_render();
    }

    /// Refresh what extensions read about their overlays.
    pub(super) fn sync_extension_overlays(&self) {
        let handles: Vec<(u64, OverlayHandle)> =
            self.extensions_state().overlays.iter().map(|(id, handle)| (*id, handle.clone())).collect();
        let mirror = self.state.borrow().extensions.mirror.clone();
        for (id, handle) in handles {
            mirror.set_overlay(id, Some(mirror_of(&handle)));
        }
    }

    /// `ctx.ui.addAutocompleteProvider()`: stack the factory and rebuild the editor's provider.
    pub(super) fn add_extension_autocomplete(&self, factory: ComponentFactory) {
        self.extensions_state().autocomplete_factories.push(factory);
        self.setup_autocomplete_provider();
    }

    /// Wrap `base` with the extension providers; `base` when there are none or they fail.
    pub(super) fn wrap_extension_autocomplete(
        &self,
        base: Rc<dyn AutocompleteProvider>,
    ) -> Rc<dyn AutocompleteProvider> {
        self.extensions_state().autocomplete_base = Some(base.clone());
        let factories = self.extensions_state().autocomplete_factories.clone();
        if factories.is_empty() {
            return base;
        }
        match JsAutocompleteProvider::build(&factories) {
            Ok(provider) => Rc::new(provider),
            Err(error) => {
                self.show_error(&format!("Extension autocomplete provider failed: {error}"));
                base
            }
        }
    }

    /// Suggestions of the built-in provider for the extension chain.
    pub(super) fn base_autocomplete_suggestions(
        &self,
        request: crate::extension_ui::AutocompleteRequest,
        sender: oneshot::Sender<Option<pi_tui::autocomplete::AutocompleteSuggestions>>,
    ) {
        let Some(base) = self.extensions_state().autocomplete_base.clone() else {
            let _ = sender.send(None);
            return;
        };
        let options = pi_tui::autocomplete::SuggestionOptions { force: request.force, ..Default::default() };
        let future = base.get_suggestions(&request.lines, request.cursor_line, request.cursor_col, options);
        tokio::task::spawn_local(async move {
            let _ = sender.send(future.await);
        });
    }

    /// Drop overlay handles and stacked providers with the extension runtime.
    pub(super) fn reset_extension_overlays_and_autocomplete(&self) {
        let ids: Vec<u64> = self.extensions_state().overlays.drain().map(|(id, _)| id).collect();
        let mirror = self.state.borrow().extensions.mirror.clone();
        for id in ids {
            mirror.set_overlay(id, None);
        }
        let had_providers = !std::mem::take(&mut self.extensions_state().autocomplete_factories).is_empty();
        if had_providers {
            self.setup_autocomplete_provider();
        }
    }
}
