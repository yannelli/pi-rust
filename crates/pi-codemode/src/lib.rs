//! Codemode for the pi agent harness: a QuickJS sandbox that runs model-written JavaScript which
//! calls other tools, TypeScript declarations of those tools, BM25 tool search, and the `codemode`
//! tool built on a host trait the application implements.

mod calls;
pub mod declarations;
mod description;
mod discovery;
mod execute;
pub mod host;
pub mod identifier;
pub mod js;
pub mod loadout;
mod model_checks;
mod models;
pub mod output;
pub mod sandbox;
pub mod schema;
pub mod source;
pub mod store;
mod tool;
pub mod tool_search;

pub use calls::{CodemodeNestedCall, CodemodeNestedCallStatus, CodemodeToolDetails, combine_usage};
pub use declarations::{
    DEFAULT_INPUT_SCHEMA_MAX_CHARS, ToolDeclaration, render_declarations, render_tool_output_type, render_tool_sample,
    render_tool_signature,
};
pub use description::{
    CODEMODE_PROMPT_GUIDELINES, CODEMODE_PROMPT_SNIPPET, CODEMODE_TOOL_NAME, CodemodeDescriptionOptions, CodemodeMode,
    DEFAULT_CODEMODE_INLINE_BUDGET, codemode_callable_tools, create_codemode_description, describe_script_call,
    to_codemode_declaration,
};
pub use discovery::is_namespace_name;
pub use execute::{
    CODEMODE_MEMORY_LIMIT_BYTES, CodemodeToolOutput, MAX_CONCURRENT_CODEMODE_WORKERS, execute_codemode,
    set_codemode_worker_limit,
};
pub use host::{
    CodemodeHost, CodemodeModels, CodemodeToolInfo, ModelType, NestedToolOutcome, ToolExposure, ToolNamespace,
};
pub use identifier::to_codemode_identifier;
pub use loadout::{
    CODEMODE_DOCS_FILE_NAME, CodemodeToolOptions, ToolLoadout, ToolLoadoutChanges, prepare_codemode_loadout,
};
pub use models::MAX_CONCURRENT_MODEL_CALLS;
pub use sandbox::{
    AcquireWorker, CodemodeCall, CodemodeCallStatus, CodemodeCallTarget, CodemodeDeclaredGlobal, CodemodeDeclaredTool,
    CodemodeDuring, CodemodeError, CodemodeErrorKind, CodemodeHostCall, CodemodeOutcome, CodemodeOutputItem,
    CodemodeResult, CodemodeRun, CodemodeRunOptions, CodemodeSandbox, CodemodeStoreWrites, CodemodeSuccess,
    ExecuteOptions, MAX_OUTPUT_CHARS, MAX_OUTPUT_ITEMS, MAX_STORE_TOTAL_CHARS, MAX_STORE_VALUE_CHARS, OnCall, OnIdle,
    OnStarted, RunDeliverer, SandboxError, SandboxExecute, SandboxFunction, SandboxOptions, Timeout, WorkerRelease,
};
pub use schema::schema_to_type;
pub use source::{
    CODEMODE_OPTIONS_PREFIX, CODEMODE_SOURCE_GRAMMAR, CodemodeSourceError, CodemodeSourceOptions, ParsedCodemodeSource,
    parse_codemode_source,
};
pub use store::{CODEMODE_STORE_ENTRY_TYPE, CodemodeStoreEntryData, read_codemode_store};
pub use tool::{
    CODEMODE_TOOL_EXPOSURE, CodemodeTool, codemode_constrained_sampling, codemode_schema, codemode_script_reach,
};
pub use tool_search::{
    Bm25Ranker, DEFAULT_TOOL_SEARCH_LIMIT, TOOL_SEARCH_DESCRIPTION, TOOL_SEARCH_PROMPT_SNIPPET, TOOL_SEARCH_TOOL_NAME,
    ToolRanker, ToolSearchDocument, ToolSearchMatch, ToolSearchResultTool, create_tool_search_document,
    format_tool_search_result, is_searchable, search_deferred_tools, search_tools, tokenize, tool_search_document,
    tool_search_limit, tool_search_schema,
};
