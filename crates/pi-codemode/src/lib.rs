//! Codemode for the pi agent harness: a QuickJS sandbox that runs model-written JavaScript which
//! calls other tools, TypeScript declarations of those tools, BM25 tool search, and the `codemode`
//! tool built on a host trait the application implements.

pub mod declarations;
pub mod identifier;
pub mod js;
pub mod sandbox;
pub mod schema;
pub mod source;

pub use declarations::{
    DEFAULT_INPUT_SCHEMA_MAX_CHARS, MCP_TYPESCRIPT_PREAMBLE, ToolDeclaration, mcp_structured_content_schema,
    render_declarations, render_tool_output_type, render_tool_sample, render_tool_signature,
};
pub use identifier::to_codemode_identifier;
pub use sandbox::{
    CodemodeCall, CodemodeCallStatus, CodemodeError, CodemodeErrorKind, CodemodeOutputItem, CodemodeResult,
    CodemodeSandbox, CodemodeStoreWrites, CodemodeSuccess, ExecuteOptions, MAX_OUTPUT_CHARS, MAX_OUTPUT_ITEMS,
    MAX_STORE_TOTAL_CHARS, MAX_STORE_VALUE_CHARS, SandboxError, SandboxExecute, SandboxFunction, SandboxOptions,
    Timeout,
};
pub use schema::schema_to_type;
pub use source::{
    CODEMODE_OPTIONS_PREFIX, CODEMODE_SOURCE_GRAMMAR, CodemodeSourceError, CodemodeSourceOptions, ParsedCodemodeSource,
    parse_codemode_source,
};
