//! llama.cpp router support (upstream built-in extension `extensions/llama/*` and
//! `docs/llama-cpp.md`): the `llama.cpp` provider, the router client, Hugging Face search, and
//! the `/llama` flow behind the [`LlamaUi`] trait.

pub mod auth;
pub mod client;
pub mod huggingface;
pub mod manager;
pub mod provider;
pub mod view;
mod wait;

#[cfg(test)]
mod tests;

pub use auth::{
    ENV_LLAMA_API_KEY, ENV_LLAMA_BASE_URL, check_llama_auth, credential_server_url, login_llama, resolve_llama_auth,
    resolve_server_url,
};
pub use client::{
    LlamaArchitecture, LlamaClient, LlamaError, LlamaModelEvent, LlamaModelInfo, LlamaModelMeta, LlamaModelStatus,
    LlamaProgress, LlamaServerProps, format_bytes, llama_inference_url, normalize_llama_server_url,
    parse_download_progress, parse_load_progress,
};
pub use huggingface::{
    DEFAULT_HUGGING_FACE_URL, GatedAccess, HuggingFaceClient, HuggingFaceModel, HuggingFaceModelDetails,
    HuggingFaceQuantization, find_hugging_face_token, find_hugging_face_token_with,
};
pub use manager::{
    ConnectionChoice, LlamaCatalogSync, LlamaManager, LlamaManagerAction, LlamaUi, NotifyLevel, ProgressFn,
    ProgressOptions, ProgressOutcome, configured_llama_client, connection_error_message, quantization_option,
    run_with_progress,
};
pub use provider::{
    DEFAULT_LLAMA_SERVER_URL, LLAMA_PROVIDER_ID, LlamaProvider, context_window_of, is_chat_model, is_decision_model,
    model_is_selectable, to_chat_model, to_classifier_model,
};
