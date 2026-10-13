//! llama.cpp login text. The server URL login itself is [`crate::llama::login_llama`], which
//! [`super::run_dialog_login`] runs for `llama.cpp`.

/// The status text after a llama.cpp login without a selected model.
pub fn llama_cpp_post_login_guidance(action_label: &str, loaded_model_count: usize) -> String {
    if loaded_model_count == 0 {
        format!("{action_label}. No llama.cpp models are loaded. Use /llama to load a model, then /model to select it.")
    } else {
        format!("{action_label}. Use /model to select a loaded llama.cpp model, or /llama to manage models.")
    }
}
