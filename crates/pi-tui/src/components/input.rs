//! Single-line text input (temporary minimal version while the full port lands).

use crate::component::Component;
use crate::components::scroll_view::StyleFn;

#[derive(Default, Clone)]
pub struct InputOptions {
    pub prompt: Option<String>,
    pub placeholder: Option<String>,
    pub placeholder_style: Option<StyleFn>,
}

pub struct Input {
    value: String,
    prompt: String,
    focused: bool,
}

impl Input {
    pub fn new(options: InputOptions) -> Self {
        Self { value: String::new(), prompt: options.prompt.unwrap_or_else(|| "> ".into()), focused: false }
    }
    pub fn get_value(&self) -> &str {
        &self.value
    }
}

impl Component for Input {
    fn render(&mut self, _width: usize) -> Vec<String> {
        vec![format!("{}{}", self.prompt, self.value)]
    }
    fn handle_input(&mut self, data: &str) {
        self.value.push_str(data);
    }
    fn handles_input(&self) -> bool {
        true
    }
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}
