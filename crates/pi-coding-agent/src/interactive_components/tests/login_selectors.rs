//! Provider selector and Radius menu; ports `oauth-selector.test.ts`.

use std::time::Duration;

use pi_tui::Component;

use pi_ai::auth::{AuthCheck, AuthType};

use crate::interactive_components::login::{
    AuthSelectorMode, AuthSelectorProvider, OAuthSelector, RadiusLoginMenu, RadiusOption, radius_shimmer,
};
use crate::interactive_components::test_support::{Calls, Harness, keys, render_text, setup};

fn provider(id: &str, name: &str, auth_type: AuthType, status: Option<(AuthType, &str)>) -> AuthSelectorProvider {
    AuthSelectorProvider {
        id: id.into(),
        name: name.into(),
        auth_type,
        method_name: None,
        status: status.map(|(auth_type, source)| AuthCheck { auth_type, source: source.into() }),
        subscription: None,
    }
}

fn render_one(provider: AuthSelectorProvider) -> String {
    let _kb = setup();
    let mut selector = OAuthSelector::new(AuthSelectorMode::Login, vec![provider], |_: &str, _| {}, || {}, None);
    render_text(&mut selector, 120)
}

#[test]
fn renders_an_option_without_auth_status_as_not_configured() {
    let output = render_one(provider("google", "Google", AuthType::ApiKey, None));
    assert!(output.contains("not configured"), "{output}");
    assert!(!output.contains("✓ configured"));
}

#[test]
fn shows_oauth_auth_distinctly_in_the_api_key_selector() {
    let output = render_one(provider("anthropic", "Anthropic", AuthType::ApiKey, Some((AuthType::OAuth, "OAuth"))));
    assert!(output.contains("subscription configured"), "{output}");
}

#[test]
fn shows_environment_api_key_auth_as_configured() {
    let status = Some((AuthType::ApiKey, "OPENAI_API_KEY"));
    let output = render_one(provider("openai", "OpenAI", AuthType::ApiKey, status));
    assert!(output.contains("✓ env: OPENAI_API_KEY"), "{output}");
    assert!(!output.contains("not configured"));
}

#[test]
fn shows_models_json_key_and_command_auth_as_configured() {
    let status = Some((AuthType::ApiKey, "key in models.json"));
    assert!(
        render_one(provider("local-proxy", "local-proxy", AuthType::ApiKey, status)).contains("✓ key in models.json")
    );
    let status = Some((AuthType::ApiKey, "command in models.json"));
    let output = render_one(provider("op-proxy", "op-proxy", AuthType::ApiKey, status));
    assert!(output.contains("✓ command in models.json"));
}

#[test]
fn stored_credentials_show_as_configured_and_accounts_are_labeled() {
    let mut account = provider("meta", "Meta", AuthType::OAuth, None);
    account.subscription = Some(false);
    let key = provider("meta", "Meta", AuthType::ApiKey, Some((AuthType::ApiKey, "stored credential")));
    let _kb = setup();
    let mut selector = OAuthSelector::new(AuthSelectorMode::Login, vec![account, key], |_: &str, _| {}, || {}, None);
    let output = render_text(&mut selector, 120);
    assert!(output.contains("→ Meta [account] • not configured"), "{output}");
    assert!(output.contains("  Meta [API key] ✓ configured"), "{output}");
}

fn many_providers() -> Vec<AuthSelectorProvider> {
    ["Anthropic", "DeepSeek", "Google", "Groq", "Mistral", "OpenAI", "OpenRouter", "Together", "xAI", "Z.ai"]
        .iter()
        .map(|name| provider(&name.to_lowercase(), name, AuthType::ApiKey, None))
        .collect()
}

#[test]
fn navigates_scrolls_filters_and_selects() {
    let selected = Calls::new();
    let on_select = selected.recorder();
    let harness = Harness::new(100, 24, |_| {
        let mut on_select = on_select;
        OAuthSelector::new(
            AuthSelectorMode::Login,
            many_providers(),
            move |id: &str, auth_type| on_select((id.to_string(), auth_type)),
            || {},
            None,
        )
    });
    let screen = harness.screen();
    assert!(screen.contains(" Select provider to configure:"), "{screen}");
    assert!(screen.contains("→ Anthropic"));
    assert!(screen.contains("(1/10)"));
    assert!(screen.contains("Together") && !screen.contains("xAI"));
    harness.send_all(&[keys::UP, keys::DOWN, keys::DOWN]);
    assert!(harness.screen().contains("→ Google"));
    harness.type_text("open");
    let screen = harness.screen();
    assert!(screen.contains("> open"));
    assert!(screen.contains("OpenAI") && screen.contains("OpenRouter") && !screen.contains("Anthropic"), "{screen}");
    harness.send(keys::ENTER);
    assert_eq!(selected.get().len(), 1);
    assert_eq!(selected.get()[0].1, AuthType::ApiKey);
    harness.type_text("zzzz");
    assert!(harness.screen().contains("No matching providers"));
}

#[test]
fn initial_search_and_empty_states() {
    let _kb = setup();
    let mut selector =
        OAuthSelector::new(AuthSelectorMode::Login, many_providers(), |_: &str, _| {}, || {}, Some("groq"));
    let output = render_text(&mut selector, 100);
    assert!(output.contains("> groq") && output.contains("→ Groq"), "{output}");
    let mut empty = OAuthSelector::new(AuthSelectorMode::Logout, Vec::new(), |_: &str, _| {}, || {}, None);
    let output = render_text(&mut empty, 100);
    assert!(output.contains("Select provider to logout:"));
    assert!(output.contains("No providers logged in. Use /login first."));
    let mut empty = OAuthSelector::new(AuthSelectorMode::Login, Vec::new(), |_: &str, _| {}, || {}, None);
    assert!(render_text(&mut empty, 100).contains("No providers available"));
}

#[test]
fn cancel_calls_back() {
    let cancelled = Calls::new();
    let harness = Harness::new(80, 20, |_| {
        OAuthSelector::new(AuthSelectorMode::Logout, many_providers(), |_: &str, _| {}, cancelled.counter(), None)
    });
    harness.send(keys::ESCAPE);
    assert_eq!(cancelled.len(), 1);
}

fn radius_menu_harness(selected: &Calls<String>) -> Harness<RadiusLoginMenu> {
    let options = vec!["Sign in with an account".to_string(), "Sign in with Radius • not configured".to_string()];
    let radius = RadiusOption { label: options[1].clone(), text: "Sign in with Radius".into() };
    let on_select = selected.recorder();
    Harness::new(100, 20, move |tui| {
        let mut on_select = on_select;
        RadiusLoginMenu::new(
            tui,
            "Select authentication method:",
            options,
            radius,
            move |option: &str| on_select(option.to_string()),
            || {},
        )
    })
}

#[test]
fn radius_option_shimmers_only_while_selected() {
    let selected = Calls::new();
    let harness = radius_menu_harness(&selected);
    let plain_lines = harness.component.borrow_mut().selector.render(100);
    let animated = harness.component.borrow_mut().render(100);
    assert_eq!(animated, plain_lines, "unselected Radius option renders normally");
    harness.send(keys::DOWN);
    let screen = harness.screen();
    assert!(screen.contains("→ Sign in with Radius • not configured"), "{screen}");
    let animated = harness.component.borrow_mut().render(100);
    let plain_lines = harness.component.borrow_mut().selector.render(100);
    assert_ne!(animated, plain_lines);
    harness.run_for(Duration::from_millis(120));
    harness.send(keys::ENTER);
    assert_eq!(selected.get(), ["Sign in with Radius • not configured"]);
}

#[test]
fn shimmer_keeps_text_and_resets_color() {
    let shimmer = radius_shimmer("Radius", Duration::from_millis(250));
    assert_eq!(pi_tui::utils::strip_terminal_sequences(&shimmer), "Radius");
    assert!(shimmer.ends_with("\x1b[39m"));
    assert_ne!(shimmer, radius_shimmer("Radius", Duration::from_millis(900)));
}

#[test]
fn login_method_menu_matches_upstream_labels() {
    let _kb = setup();
    use crate::interactive_components::login::{LoginMethodChoice, LoginMethodMenu, api_key_login_details};
    let radius = provider("radius", "Radius", AuthType::OAuth, None);
    let menu = LoginMethodMenu::new(None, Some(&radius), None);
    assert_eq!(menu.title, "Select authentication method:");
    let plain_options: Vec<String> =
        menu.options.iter().map(|option| pi_tui::utils::strip_terminal_sequences(option)).collect();
    assert_eq!(
        plain_options,
        ["Sign in with an account", "Sign in with an API key", "Sign in with Radius • not configured"]
    );
    assert_eq!(menu.radius.as_ref().unwrap().text, "Sign in with Radius");
    assert_eq!(menu.choice(&menu.options[2]), LoginMethodChoice::Radius);
    assert_eq!(menu.choice("Sign in with an account"), LoginMethodChoice::AuthType(AuthType::OAuth));
    assert_eq!(menu.choice("Sign in with an API key"), LoginMethodChoice::AuthType(AuthType::ApiKey));

    let anthropic = [provider("anthropic", "Anthropic", AuthType::OAuth, None)];
    let menu = LoginMethodMenu::new(Some(&anthropic), Some(&radius), Some("Sign in with Claude"));
    assert_eq!(menu.title, "Select authentication method for Anthropic:");
    assert_eq!(menu.options, ["Sign in with Claude"]);
    assert!(menu.radius.is_none());

    assert!(api_key_login_details("openai", "docs").is_none());
    let details = api_key_login_details("amazon-bedrock", "/docs/providers.md").unwrap();
    let details: Vec<String> = details.iter().map(|line| pi_tui::utils::strip_terminal_sequences(line)).collect();
    assert_eq!(
        details,
        ["You can also use an AWS profile, IAM keys, or role-based credentials.", "See:", "  /docs/providers.md"]
    );
}

#[test]
fn ambient_dialog_explains_and_closes() {
    use crate::interactive_components::login::ambient_auth_dialog;
    let closed = Calls::new();
    let harness = Harness::new(100, 20, |tui| {
        ambient_auth_dialog(
            tui,
            "google-vertex",
            "Google Vertex AI",
            Some("Google Cloud credentials"),
            closed.counter(),
        )
    });
    let screen = harness.screen();
    assert!(screen.contains("Google Vertex AI setup"), "{screen}");
    assert!(screen.contains("Google Cloud credentials is configured outside pi."));
    assert!(screen.contains("(escape/ctrl+c to close)"));
    harness.send(keys::ESCAPE);
    assert_eq!(closed.len(), 1);
}
