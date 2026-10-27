//! Development CLI for the OAuth logins (upstream `ai/src/cli.ts`): `pi-ai login [provider]`
//! saves the credential to `./auth.json`, and `pi-ai list` shows the providers.

use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pi_ai::auth::oauth::builtin_oauth_providers;
use pi_ai::auth::{
    AuthError, AuthEvent, AuthInteraction, AuthPrompt, AuthResult, Credential, LoginOptions, OAuthAuth, OAuthCredential,
};
use serde_json::{Map, Value};
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio_util::sync::CancellationToken;

const AUTH_FILE: &str = "auth.json";

type Providers = Vec<(String, Arc<dyn OAuthAuth>)>;
type Output = Arc<Mutex<dyn Write + Send>>;

/// Lines typed by the user, read on a separate thread so prompts can be cancelled.
type Input = Arc<AsyncMutex<mpsc::UnboundedReceiver<String>>>;

fn say(out: &Output, text: &str) {
    let mut out = out.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let _ = write!(out, "{text}");
    let _ = out.flush();
}

async fn read_line(input: &Input, cancel: &CancellationToken) -> AuthResult<String> {
    let mut input = input.lock().await;
    tokio::select! {
        line = input.recv() => line.map(|line| line.trim_end_matches(['\r', '\n']).to_string()).ok_or(AuthError::Cancelled),
        () = cancel.cancelled() => Err(AuthError::Cancelled),
    }
}

/// The 1-based choice in `line`, as an index below `count`.
fn parse_choice(line: &str, count: usize) -> Option<usize> {
    let number: usize = line.trim().parse().ok()?;
    (1..=count).contains(&number).then(|| number - 1)
}

struct CliInteraction {
    input: Input,
    out: Output,
}

#[async_trait]
impl AuthInteraction for CliInteraction {
    async fn prompt(&self, prompt: AuthPrompt, cancel: CancellationToken) -> AuthResult<String> {
        match prompt {
            AuthPrompt::Select { message, options } => {
                let mut text = format!("\n{message}\n");
                for (index, option) in options.iter().enumerate() {
                    text.push_str(&format!("  {}. {}\n", index + 1, option.label));
                }
                text.push_str(&format!("Enter number (1-{}): ", options.len()));
                say(&self.out, &text);
                let line = read_line(&self.input, &cancel).await?;
                let choice = parse_choice(&line, options.len()).ok_or_else(|| AuthError::other("Invalid selection"))?;
                Ok(options[choice].id.clone())
            }
            AuthPrompt::Text { message, placeholder }
            | AuthPrompt::Secret { message, placeholder }
            | AuthPrompt::ManualCode { message, placeholder } => {
                let hint = placeholder.map(|p| format!(" ({p})")).unwrap_or_default();
                say(&self.out, &format!("{message}{hint}: "));
                read_line(&self.input, &cancel).await
            }
        }
    }

    fn notify(&self, event: AuthEvent) {
        let text = match event {
            AuthEvent::AuthUrl { url, instructions } => {
                let extra = instructions.map(|i| format!("{i}\n")).unwrap_or_default();
                format!("\nOpen this URL in your browser:\n{url}\n{extra}")
            }
            AuthEvent::DeviceCode { user_code, verification_uri, .. } => {
                format!("\nOpen this URL in your browser:\n{verification_uri}\nEnter code: {user_code}\n")
            }
            AuthEvent::Info { message, .. } | AuthEvent::Progress { message } => format!("{message}\n"),
        };
        say(&self.out, &text);
    }
}

fn provider_lines(providers: &Providers, indent: &str) -> String {
    providers.iter().map(|(id, oauth)| format!("{indent}{id:<20} {}\n", oauth.name())).collect()
}

fn usage(providers: &Providers) -> String {
    format!(
        "Usage: pi-ai <command> [provider]\n\nCommands:\n  login [provider]  Login to an OAuth provider\n  list              List available providers\n\nProviders:\n{}",
        provider_lines(providers, "  ")
    )
}

/// Store `credential` under `provider` in the auth file, keeping the other entries.
fn save_credential(path: &Path, provider: &str, credential: OAuthCredential) -> Result<(), String> {
    let mut auth: Map<String, Value> =
        std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default();
    let value = serde_json::to_value(Credential::OAuth(credential)).map_err(|e| e.to_string())?;
    auth.insert(provider.to_string(), value);
    let text = serde_json::to_string_pretty(&Value::Object(auth)).map_err(|e| e.to_string())?;
    write_private(path, &text).map_err(|e| format!("Could not write {}: {e}", path.display()))
}

/// OAuth tokens are readable by the user only. `mode` applies when the file is created; the
/// chmod covers a file an older version created, before the new tokens are in it.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600);
        if path.exists() {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    options.open(path)?.write_all(text.as_bytes())
}

/// A random version 4 UUID.
fn random_uuid() -> String {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut bytes = [0u8; 16];
    let _ = SystemRandom::new().fill(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

async fn login(providers: &Providers, id: &str, interaction: &CliInteraction, auth_path: &Path) -> Result<(), String> {
    let Some((_, oauth)) = providers.iter().find(|(provider, _)| provider == id) else {
        return Err(format!("Unknown provider: {id}"));
    };
    // This dev CLI keeps no installation id; apps should reuse one across logins.
    let options = LoginOptions { device_id: Some(Arc::new(random_uuid)), ..LoginOptions::default() };
    let credential =
        oauth.login(interaction, &options, CancellationToken::new()).await.map_err(|error| error.to_string())?;
    save_credential(auth_path, id, credential)?;
    say(&interaction.out, &format!("\nCredentials saved to {}\n", auth_path.display()));
    Ok(())
}

async fn run(
    args: &[String],
    providers: &Providers,
    interaction: &CliInteraction,
    auth_path: &Path,
) -> Result<(), String> {
    let out = &interaction.out;
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => say(out, &usage(providers)),
        Some("list") => say(out, &provider_lines(providers, "")),
        Some("login") => {
            let id = match args.get(1) {
                Some(id) => id.clone(),
                None => {
                    let mut text = String::new();
                    for (index, (_, oauth)) in providers.iter().enumerate() {
                        text.push_str(&format!("  {}. {}\n", index + 1, oauth.name()));
                    }
                    text.push_str(&format!("Enter number (1-{}): ", providers.len()));
                    say(out, &text);
                    let line = read_line(&interaction.input, &CancellationToken::new()).await.unwrap_or_default();
                    match parse_choice(&line, providers.len()) {
                        Some(index) => providers[index].0.clone(),
                        None => return Err("Unknown provider: ".into()),
                    }
                }
            };
            login(providers, &id, interaction, auth_path).await?;
        }
        Some(command) => return Err(format!("Unknown command: {command}")),
    }
    Ok(())
}

fn stdin_lines() -> Input {
    let (sender, receiver) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    Arc::new(AsyncMutex::new(receiver))
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let out: Output = Arc::new(Mutex::new(std::io::stdout()));
    let interaction = CliInteraction { input: stdin_lines(), out };
    if let Err(error) = run(&args, &builtin_oauth_providers(), &interaction, Path::new(AUTH_FILE)).await {
        eprintln!("Error: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use pi_ai::auth::{ModelAuth, PromptOption};

    use super::*;

    struct FakeOAuth;

    #[async_trait]
    impl OAuthAuth for FakeOAuth {
        fn name(&self) -> &str {
            "Fake Provider"
        }

        async fn login(
            &self,
            interaction: &dyn AuthInteraction,
            options: &LoginOptions,
            cancel: CancellationToken,
        ) -> AuthResult<OAuthCredential> {
            interaction.notify(AuthEvent::AuthUrl { url: "https://example.test/auth".into(), instructions: None });
            let option = |id: &str| PromptOption { id: id.into(), label: id.to_uppercase(), description: None };
            let options_prompt = AuthPrompt::Select { message: "Plan".into(), options: vec![option("a"), option("b")] };
            let plan = interaction.prompt(options_prompt, cancel.clone()).await?;
            let code_prompt = AuthPrompt::ManualCode { message: "Code".into(), placeholder: Some("paste".into()) };
            let code = interaction.prompt(code_prompt, cancel).await?;
            let device = options.device_id.as_ref().map(|id| id()).unwrap_or_default();
            let mut extra = Map::new();
            extra.insert("plan".into(), Value::String(plan));
            extra.insert("device".into(), Value::String(device));
            Ok(OAuthCredential { refresh: "r".into(), access: code, expires: 1, extra })
        }

        async fn refresh(&self, credential: &OAuthCredential, _: CancellationToken) -> AuthResult<OAuthCredential> {
            Ok(credential.clone())
        }

        fn to_auth(&self, _: &OAuthCredential) -> ModelAuth {
            ModelAuth::default()
        }
    }

    fn harness(lines: &[&str]) -> (CliInteraction, Arc<Mutex<Vec<u8>>>, Providers) {
        let (sender, receiver) = mpsc::unbounded_channel();
        for line in lines {
            sender.send(line.to_string()).unwrap();
        }
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let out: Output = buffer.clone();
        let providers: Providers = vec![("fake".into(), Arc::new(FakeOAuth))];
        (CliInteraction { input: Arc::new(AsyncMutex::new(receiver)), out }, buffer, providers)
    }

    fn printed(buffer: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8(buffer.lock().unwrap().clone()).unwrap()
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    #[tokio::test]
    async fn lists_providers_and_prints_usage() {
        let (interaction, buffer, providers) = harness(&[]);
        let path = Path::new("unused.json");
        run(&args(&["list"]), &providers, &interaction, path).await.unwrap();
        assert_eq!(printed(&buffer), format!("{:<20} Fake Provider\n", "fake"));
        buffer.lock().unwrap().clear();
        run(&[], &providers, &interaction, path).await.unwrap();
        assert!(printed(&buffer).starts_with("Usage: pi-ai <command> [provider]"));
        assert!(printed(&buffer).ends_with("  fake                 Fake Provider\n"));
        let err = run(&args(&["nope"]), &providers, &interaction, path).await.unwrap_err();
        assert_eq!(err, "Unknown command: nope");
    }

    #[tokio::test]
    async fn login_saves_the_credential_next_to_existing_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(AUTH_FILE);
        std::fs::write(&path, r#"{"other":{"type":"api_key","key":"k"}}"#).unwrap();
        let (interaction, buffer, providers) = harness(&["1", "2", "the-code"]);
        run(&args(&["login"]), &providers, &interaction, &path).await.unwrap();

        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["other"]["key"], "k");
        let fake = &saved["fake"];
        assert_eq!((fake["type"].as_str(), fake["access"].as_str()), (Some("oauth"), Some("the-code")));
        assert_eq!(fake["plan"], "b");
        assert_eq!(fake["device"].as_str().unwrap().len(), 36);
        let output = printed(&buffer);
        assert!(output.contains("Open this URL in your browser:\nhttps://example.test/auth\n"));
        assert!(output.contains("  1. A\n  2. B\nEnter number (1-2): "));
        assert!(output.contains("Code (paste): "));
        assert!(output.ends_with(&format!("Credentials saved to {}\n", path.display())));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[tokio::test]
    async fn login_rejects_unknown_providers_and_invalid_selections() {
        let (interaction, _, providers) = harness(&["7"]);
        let path = Path::new("unused.json");
        let err = run(&args(&["login", "missing"]), &providers, &interaction, path).await.unwrap_err();
        assert_eq!(err, "Unknown provider: missing");
        let err = run(&args(&["login", "fake"]), &providers, &interaction, path).await.unwrap_err();
        assert_eq!(err, "Invalid selection");
    }

    #[test]
    fn random_uuid_is_version_four() {
        let id = random_uuid();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert_ne!(id, random_uuid());
    }
}
