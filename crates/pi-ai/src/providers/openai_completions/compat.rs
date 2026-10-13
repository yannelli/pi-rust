//! Resolved `OpenAICompletionsCompat`: detected from the provider and base URL, then
//! overridden by `model.compat`. Port of upstream `detectCompat` and `getCompat`.

use serde_json::{Map, Value};

use crate::providers::common::{compat_bool, compat_str, compat_value};
use crate::types::Model;

pub(crate) struct Compat {
    pub supports_store: bool,
    pub supports_developer_role: bool,
    pub supports_reasoning_effort: bool,
    pub supports_usage_in_streaming: bool,
    pub supports_finish_reason: bool,
    /// `max_tokens` or `max_completion_tokens`.
    pub max_tokens_field: String,
    pub requires_tool_result_name: bool,
    pub requires_assistant_after_tool_result: bool,
    pub requires_thinking_as_text: bool,
    pub requires_reasoning_content: bool,
    pub thinking_format: String,
    pub chat_template_kwargs: Map<String, Value>,
    pub chat_template_args: Map<String, Value>,
    pub zai_tool_stream: bool,
    /// Top-level field that caps reasoning tokens, if the server has one.
    pub thinking_budget_field: Option<String>,
    pub supports_strict_mode: bool,
    /// Grammar tools are sent as OpenAI `custom` tools.
    pub supports_openai_grammar_tools: bool,
    pub supports_mid_convo_system_messages: bool,
    pub supports_mid_convo_tool_additions: bool,
    /// Anthropic-style `cache_control` markers (`cacheControlFormat: "anthropic"`).
    pub anthropic_cache_control: bool,
    pub send_session_affinity_headers: bool,
    pub session_affinity_format: String,
    pub supports_long_cache_retention: bool,
    /// vLLM scheduler priority sent as the top-level `priority` field.
    pub vllm_priority: Option<Value>,
    pub open_router_routing: Option<Value>,
    pub vercel_gateway_routing: Option<Value>,
}

/// Provider families detected from `model.provider` and `model.base_url`.
struct Detected {
    zai: bool,
    together: bool,
    moonshot: bool,
    openrouter: bool,
    cf_workers: bool,
    cf_gateway: bool,
    nvidia: bool,
    ant_ling: bool,
    deepseek: bool,
    grok: bool,
    non_standard: bool,
    use_max_tokens: bool,
}

impl Detected {
    fn new(model: &Model) -> Self {
        let provider = model.provider.as_str();
        let url = model.base_url.as_str();
        let zai =
            matches!(provider, "zai" | "zai-coding-cn") || url.contains("api.z.ai") || url.contains("open.bigmodel.cn");
        let together = provider == "together" || url.contains("api.together.ai") || url.contains("api.together.xyz");
        let moonshot = matches!(provider, "moonshotai" | "moonshotai-cn") || url.contains("api.moonshot.");
        let cf_workers = provider == "cloudflare-workers-ai" || url.contains("api.cloudflare.com");
        let cf_gateway = provider == "cloudflare-ai-gateway" || url.contains("gateway.ai.cloudflare.com");
        let nvidia = provider == "nvidia" || url.contains("integrate.api.nvidia.com");
        let ant_ling = provider == "ant-ling" || url.contains("api.ant-ling.com");
        let cerebras = provider == "cerebras" || url.contains("cerebras.ai");
        let deepseek = provider == "deepseek" || url.to_lowercase().contains("deepseek.com");
        let grok = provider == "xai" || url.contains("api.x.ai");
        let chutes = url.contains("chutes.ai");
        let opencode = provider == "opencode" || url.contains("opencode.ai");
        Self {
            zai,
            together,
            moonshot,
            openrouter: provider == "openrouter" || url.contains("openrouter.ai"),
            cf_workers,
            cf_gateway,
            nvidia,
            ant_ling,
            deepseek,
            grok,
            non_standard: nvidia
                || cerebras
                || grok
                || together
                || chutes
                || deepseek
                || zai
                || moonshot
                || opencode
                || cf_workers
                || cf_gateway
                || ant_ling,
            use_max_tokens: chutes || deepseek || moonshot || cf_gateway || together || nvidia || ant_ling || zai,
        }
    }

    fn thinking_format(&self) -> &'static str {
        match () {
            _ if self.deepseek => "deepseek",
            _ if self.zai => "zai",
            _ if self.together => "together",
            _ if self.ant_ling => "ant-ling",
            _ if self.openrouter => "openrouter",
            _ => "openai",
        }
    }
}

impl Compat {
    pub fn resolve(model: &Model) -> Self {
        let detected = Detected::new(model);
        let flag = |key: &str, default: bool| compat_bool(model, key).unwrap_or(default);
        let text = |key: &str, default: &str| compat_str(model, key).unwrap_or(default).to_string();
        let object = |key: &str| compat_value(model, key).and_then(Value::as_object).cloned().unwrap_or_default();
        let present = |key: &str| compat_value(model, key).filter(|value| !value.is_null()).cloned();
        let openrouter_developer =
            detected.openrouter && (model.id.starts_with("anthropic/") || model.id.starts_with("openai/"));
        let anthropic_cache_detected = model.provider == "openrouter" && model.id.starts_with("anthropic/");
        let budget_field = compat_str(model, "thinkingTokenBudgetField")
            .filter(|field| !field.is_empty())
            .map(str::to_string)
            .or_else(|| flag("supportsThinkingTokenBudget", false).then(|| "thinking_token_budget".to_string()));
        let Detected { zai, together, moonshot, openrouter, cf_workers, cf_gateway, nvidia, ant_ling, .. } = detected;
        Self {
            supports_store: flag("supportsStore", !detected.non_standard),
            supports_developer_role: flag(
                "supportsDeveloperRole",
                openrouter_developer || (!detected.non_standard && !openrouter),
            ),
            supports_reasoning_effort: flag(
                "supportsReasoningEffort",
                !detected.grok && !zai && !moonshot && !together && !cf_gateway && !nvidia && !ant_ling,
            ),
            supports_usage_in_streaming: flag("supportsUsageInStreaming", true),
            supports_finish_reason: flag("supportsFinishReason", true),
            max_tokens_field: text(
                "maxTokensField",
                if detected.use_max_tokens { "max_tokens" } else { "max_completion_tokens" },
            ),
            requires_tool_result_name: flag("requiresToolResultName", false),
            requires_assistant_after_tool_result: flag("requiresAssistantAfterToolResult", false),
            requires_thinking_as_text: flag("requiresThinkingAsText", false),
            requires_reasoning_content: flag("requiresReasoningContentOnAssistantMessages", detected.deepseek),
            thinking_format: text("thinkingFormat", detected.thinking_format()),
            chat_template_kwargs: object("chatTemplateKwargs"),
            chat_template_args: object("chatTemplateArgs"),
            zai_tool_stream: flag("zaiToolStream", false),
            thinking_budget_field: budget_field,
            supports_strict_mode: flag("supportsStrictMode", false),
            supports_openai_grammar_tools: flag("supportsOpenAIGrammarTools", false),
            supports_mid_convo_system_messages: flag("supportsMidConvoSystemMessages", false),
            supports_mid_convo_tool_additions: flag("supportsMidConvoToolAdditions", false),
            anthropic_cache_control: compat_str(model, "cacheControlFormat")
                .map_or(anthropic_cache_detected, |format| format == "anthropic"),
            send_session_affinity_headers: flag("sendSessionAffinityHeaders", openrouter),
            session_affinity_format: text("sessionAffinityFormat", if openrouter { "openrouter" } else { "openai" }),
            supports_long_cache_retention: flag(
                "supportsLongCacheRetention",
                !(together || cf_workers || cf_gateway || nvidia || ant_ling),
            ),
            vllm_priority: present("vllmPriority"),
            open_router_routing: present("openRouterRouting"),
            vercel_gateway_routing: present("vercelGatewayRouting"),
        }
    }

    /// Tool additions anchor in tool-bearing system messages only when both flags are set.
    pub fn anchors_tool_additions(&self) -> bool {
        self.supports_mid_convo_system_messages && self.supports_mid_convo_tool_additions
    }
}
