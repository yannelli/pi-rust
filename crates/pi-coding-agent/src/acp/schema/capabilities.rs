use serde::Serialize;

use super::{IntoOption, Meta};

/// What the agent supports. `auth` is always sent, as an empty object.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    pub load_session: bool,
    pub prompt_capabilities: PromptCapabilities,
    pub mcp_capabilities: McpCapabilities,
    pub session_capabilities: SessionCapabilities,
    pub auth: AgentAuthCapabilities,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

impl AgentCapabilities {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn load_session(mut self, load_session: bool) -> Self {
        self.load_session = load_session;
        self
    }

    pub fn prompt_capabilities(mut self, prompt_capabilities: PromptCapabilities) -> Self {
        self.prompt_capabilities = prompt_capabilities;
        self
    }

    pub fn mcp_capabilities(mut self, mcp_capabilities: McpCapabilities) -> Self {
        self.mcp_capabilities = mcp_capabilities;
        self
    }

    pub fn session_capabilities(mut self, session_capabilities: SessionCapabilities) -> Self {
        self.session_capabilities = session_capabilities;
        self
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptCapabilities {
    pub image: bool,
    pub audio: bool,
    pub embedded_context: bool,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

impl PromptCapabilities {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn image(mut self, image: bool) -> Self {
        self.image = image;
        self
    }

    pub fn audio(mut self, audio: bool) -> Self {
        self.audio = audio;
        self
    }

    pub fn embedded_context(mut self, embedded_context: bool) -> Self {
        self.embedded_context = embedded_context;
        self
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct McpCapabilities {
    pub http: bool,
    pub sse: bool,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

impl McpCapabilities {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn http(mut self, http: bool) -> Self {
        self.http = http;
        self
    }

    pub fn sse(mut self, sse: bool) -> Self {
        self.sse = sse;
        self
    }
}

/// The session methods beyond the baseline that the agent supports.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct SessionCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list: Option<SessionListCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete: Option<SessionDeleteCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close: Option<SessionCloseCapabilities>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

impl SessionCapabilities {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn list(mut self, list: impl IntoOption<SessionListCapabilities>) -> Self {
        self.list = list.into_option();
        self
    }

    pub fn delete(mut self, delete: impl IntoOption<SessionDeleteCapabilities>) -> Self {
        self.delete = delete.into_option();
        self
    }

    pub fn close(mut self, close: impl IntoOption<SessionCloseCapabilities>) -> Self {
        self.close = close.into_option();
        self
    }
}

/// Objects with only `_meta`: empty capabilities and results.
macro_rules! meta_only {
    ($($name:ident),* $(,)?) => {$(
        #[derive(Clone, Debug, Default, PartialEq, Serialize)]
        pub struct $name {
            #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
            pub meta: Option<Meta>,
        }

        impl $name {
            pub fn new() -> Self {
                Self::default()
            }
        }
    )*};
}

meta_only!(
    AgentAuthCapabilities,
    SessionListCapabilities,
    SessionDeleteCapabilities,
    SessionCloseCapabilities,
    AuthenticateResponse,
    CloseSessionResponse,
    DeleteSessionResponse,
    SetSessionModeResponse,
);
