//! [`ExtensionEditor`]: a titled multi-line editor for extensions, with the external editor on
//! `app.editor.external` (upstream `extension-editor.ts`).

use pi_tui::components::{Editor, EditorOptions};
use pi_tui::{Component, Container, Tui, get_keybindings};

use super::common::{border, fg, spacer, text};
use super::external_editor::{ExternalEditorResult, default_editor_command, edit_in_external_editor};
use super::key_hint;
use crate::theme::{ThemeColor, get_editor_theme};

/// Options of an [`ExtensionEditor`]: the editor's own options plus a description.
#[derive(Default)]
pub struct ExtensionEditorOptions {
    pub editor: EditorOptions,
    pub description: Option<String>,
}

pub struct ExtensionEditor {
    container: Container,
    editor: Editor,
    tui: Tui,
    on_cancel: Box<dyn FnMut()>,
    external_editor_command: String,
}

impl ExtensionEditor {
    /// `external_editor_command` defaults to `$VISUAL`, `$EDITOR`, then `notepad` or `nano`.
    /// Keys come from the process-wide keybindings.
    pub fn new(
        tui: &Tui,
        title: &str,
        prefill: Option<&str>,
        mut on_submit: impl FnMut(&str) + 'static,
        on_cancel: impl FnMut() + 'static,
        options: ExtensionEditorOptions,
        external_editor_command: Option<String>,
    ) -> Self {
        let external_editor_command =
            external_editor_command.filter(|command| !command.is_empty()).unwrap_or_else(default_editor_command);
        let mut container = Container::new();
        container.add_child(border());
        container.add_child(spacer(1));
        container.add_child(text(&fg(ThemeColor::Accent, title), 1, 0));
        if let Some(description) = options.description.as_deref().filter(|description| !description.is_empty()) {
            container.add_child(spacer(1));
            container.add_child(text(&fg(ThemeColor::Text, description), 1, 0));
        }
        container.add_child(spacer(1));

        let editor = Editor::new(tui, get_editor_theme(), options.editor);
        if let Some(prefill) = prefill.filter(|prefill| !prefill.is_empty()) {
            editor.set_text(prefill);
        }
        editor.on_submit(move |text| on_submit(text));
        container.add_child(pi_tui::component(editor.clone()));
        container.add_child(spacer(1));
        let hint = format!(
            "{}  {}  {}  {}",
            key_hint("tui.select.confirm", "submit"),
            key_hint("tui.input.newLine", "newline"),
            key_hint("tui.select.cancel", "cancel"),
            key_hint("app.editor.external", "external editor"),
        );
        container.add_child(text(&hint, 1, 0));
        container.add_child(spacer(1));
        container.add_child(border());

        Self { container, editor, tui: tui.clone(), on_cancel: Box::new(on_cancel), external_editor_command }
    }

    pub fn editor(&self) -> &Editor {
        &self.editor
    }

    fn open_external_editor(&self) {
        let editor = self.editor.clone();
        let tui = self.tui.clone();
        let command = self.external_editor_command.clone();
        let content = editor.get_text();
        tui.stop(Default::default());
        tokio::task::spawn_local(async move {
            let result = edit_in_external_editor(&command, &content).await;
            if let ExternalEditorResult::Complete(text) = result {
                editor.set_text(&text);
            }
            tui.start();
            tui.request_render(true);
        });
    }
}

impl Component for ExtensionEditor {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.container.render(width)
    }

    fn handle_input(&mut self, data: &str) {
        let kb = get_keybindings();
        if kb.matches(data, "tui.select.cancel") {
            (self.on_cancel)();
            return;
        }
        if kb.matches(data, "app.editor.external") {
            self.open_external_editor();
            return;
        }
        self.editor.handle_input(data);
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn is_focusable(&self) -> bool {
        true
    }

    fn set_focused(&mut self, focused: bool) {
        self.editor.set_focused(focused);
    }

    fn invalidate(&mut self) {
        self.container.invalidate();
    }
}
