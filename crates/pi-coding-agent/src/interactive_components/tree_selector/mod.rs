//! [`TreeSelector`]: the `/tree` session tree navigator with search, filters, folding, and
//! inline label editing (upstream `tree-selector.ts`).

mod branch_summary;
mod help;
mod list;
mod nodes;
mod render;
mod text;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use pi_tui::components::{Input, Spacer, Text};
use pi_tui::utils::truncate_to_width;
use pi_tui::{Component, Container, component, get_keybindings, ui_loop};

pub use self::branch_summary::{
    ALREADY_AT_TARGET_STATUS, BRANCH_SUMMARY_PROMPT_TITLE, BranchSummaryChoice, BranchSummaryFlow, BranchSummaryStep,
    CUSTOM_INSTRUCTIONS_TITLE,
};
pub use self::list::{CopyCallback, EntryCallback, FilterMode, LabelCallback, TreeList};
pub use self::nodes::{TreeData, TreeNode};
use super::common::{Shared, shared};
use super::{DynamicBorder, key_hint};
use crate::theme::{ThemeColor, theme};

type SharedCopy = Rc<RefCell<Option<CopyCallback>>>;
type LabelRequest = Rc<RefCell<Option<(String, Option<String>)>>>;

/// What a key did in the label editor.
enum LabelAction {
    Submit(String, Option<String>),
    Cancel,
}

/// Inline label editor shown in place of the tree (upstream `LabelInput`).
struct LabelInput {
    input: Input,
    entry_id: String,
}

impl LabelInput {
    fn new(entry_id: &str, current_label: Option<&str>, focused: bool) -> Self {
        let mut input = Input::default();
        if let Some(label) = current_label.filter(|label| !label.is_empty()) {
            input.set_value(label);
        }
        input.focused = focused;
        Self { input, entry_id: entry_id.to_string() }
    }

    fn render(&mut self, width: usize) -> Vec<String> {
        let theme = theme();
        let mut lines =
            vec![truncate_to_width(&format!("  {}", theme.fg(ThemeColor::Muted, "Label (empty to remove):")), width)];
        let input_lines = self.input.render(width.saturating_sub(2));
        lines.extend(input_lines.iter().map(|line| truncate_to_width(&format!("  {line}"), width)));
        let hints =
            format!("  {}  {}", key_hint("tui.select.confirm", "save"), key_hint("tui.select.cancel", "cancel"));
        lines.push(truncate_to_width(&hints, width));
        lines
    }

    fn handle_input(&mut self, data: &str) -> Option<LabelAction> {
        let kb = get_keybindings();
        if kb.matches(data, "tui.select.confirm") {
            let value = self.input.get_value().trim();
            let label = (!value.is_empty()).then(|| value.to_string());
            Some(LabelAction::Submit(self.entry_id.clone(), label))
        } else if kb.matches(data, "tui.select.cancel") {
            Some(LabelAction::Cancel)
        } else {
            self.input.handle_input(data);
            None
        }
    }
}

/// Session tree selector. Enter reports the selected entry id to `on_select`; escape clears
/// the search or calls `on_cancel`; `app.tree.editLabel` edits the selected entry's label
/// inline and reports the result to `on_label_change`.
pub struct TreeSelector {
    header: Container,
    middle: Container,
    footer: Container,
    tree_list: TreeList,
    label_input: Option<LabelInput>,
    label_request: LabelRequest,
    on_label_change: Option<LabelCallback>,
    on_copy: SharedCopy,
    focused: bool,
}

impl TreeSelector {
    /// `terminal_height` sets the visible rows (half the terminal, at least 5). An empty tree
    /// calls `on_cancel` after 100 ms, as upstream does.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tree: impl Into<TreeData>,
        current_leaf_id: Option<&str>,
        terminal_height: usize,
        on_select: impl FnMut(&str) + 'static,
        on_cancel: impl FnMut() + 'static,
        on_label_change: Option<LabelCallback>,
        initial_selected_id: Option<&str>,
        initial_filter_mode: Option<FilterMode>,
    ) -> Self {
        let tree: TreeData = tree.into();
        let empty = tree.is_empty();
        let max_visible_lines = (terminal_height / 2).max(5);
        let mut tree_list =
            TreeList::new(tree, current_leaf_id, max_visible_lines, initial_selected_id, initial_filter_mode);
        let on_cancel: Shared<dyn FnMut()> = shared(Box::new(on_cancel));
        tree_list.on_select = Some(Box::new(on_select));
        let cancel = on_cancel.clone();
        tree_list.on_cancel = Some(Box::new(move || (cancel.borrow_mut())()));
        let on_copy: SharedCopy = Rc::default();
        let copy = on_copy.clone();
        tree_list.on_copy = Some(Box::new(move |text| {
            if let Some(callback) = copy.borrow_mut().as_mut() {
                callback(text);
            }
        }));
        let label_request: LabelRequest = Rc::default();
        let request = label_request.clone();
        tree_list.on_label_edit = Some(Box::new(move |entry_id, label| {
            *request.borrow_mut() = Some((entry_id.to_string(), label.map(String::from)));
        }));

        let mut header = Container::new();
        header.add_child(component(Spacer::new(1)));
        header.add_child(component(DynamicBorder::new()));
        header.add_child(component(Text::new(&theme().bold("  Session Tree"), 1, 0)));
        header.add_child(component(help::TreeHelp));
        let mut middle = Container::new();
        middle.add_child(component(DynamicBorder::new()));
        middle.add_child(component(Spacer::new(1)));
        let mut footer = Container::new();
        footer.add_child(component(Spacer::new(1)));
        footer.add_child(component(DynamicBorder::new()));

        if empty {
            ui_loop::set_timeout(Duration::from_millis(100), move || (on_cancel.borrow_mut())());
        }
        Self {
            header,
            middle,
            footer,
            tree_list,
            label_input: None,
            label_request,
            on_label_change,
            on_copy,
            focused: false,
        }
    }

    /// Receives the selected entry's full text on `app.message.copy` (`None` when it has none).
    pub fn set_on_copy(&mut self, on_copy: impl FnMut(Option<&str>) + 'static) {
        *self.on_copy.borrow_mut() = Some(Box::new(on_copy));
    }

    pub fn tree_list(&self) -> &TreeList {
        &self.tree_list
    }

    pub fn tree_list_mut(&mut self) -> &mut TreeList {
        &mut self.tree_list
    }

    /// Whether the inline label editor is open.
    pub fn is_editing_label(&self) -> bool {
        self.label_input.is_some()
    }

    fn search_line(&self, width: usize) -> String {
        let theme = theme();
        let prompt = theme.fg(ThemeColor::Muted, "Type to search:");
        let query = self.tree_list.get_search_query();
        let line = if query.is_empty() {
            format!("  {prompt}")
        } else {
            format!("  {prompt} {}", theme.fg(ThemeColor::Accent, query))
        };
        truncate_to_width(&line, width)
    }

    fn submit_label(&mut self, entry_id: &str, label: Option<&str>) {
        self.tree_list.update_node_label(entry_id, label, None);
        if let Some(on_label_change) = self.on_label_change.as_mut() {
            on_label_change(entry_id, label);
        }
        self.label_input = None;
    }
}

impl Component for TreeSelector {
    fn render(&mut self, width: usize) -> Vec<String> {
        let mut lines = self.header.render(width);
        lines.push(self.search_line(width));
        lines.extend(self.middle.render(width));
        match self.label_input.as_mut() {
            Some(label_input) => lines.extend(label_input.render(width)),
            None => lines.extend(self.tree_list.render(width)),
        }
        lines.extend(self.footer.render(width));
        lines
    }

    fn handle_input(&mut self, data: &str) {
        if let Some(label_input) = self.label_input.as_mut() {
            match label_input.handle_input(data) {
                Some(LabelAction::Submit(entry_id, label)) => self.submit_label(&entry_id, label.as_deref()),
                Some(LabelAction::Cancel) => self.label_input = None,
                None => {}
            }
            return;
        }
        self.tree_list.handle_input(data);
        if let Some((entry_id, label)) = self.label_request.borrow_mut().take() {
            self.label_input = Some(LabelInput::new(&entry_id, label.as_deref(), self.focused));
        }
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn is_focusable(&self) -> bool {
        true
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        if let Some(label_input) = self.label_input.as_mut() {
            label_input.input.focused = focused;
        }
    }

    fn invalidate(&mut self) {
        self.header.invalidate();
        self.middle.invalidate();
        self.footer.invalidate();
    }
}
