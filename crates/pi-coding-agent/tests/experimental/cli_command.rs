//! Port of upstream `test/experimental-cli-command.test.ts`.

use pi_coding_agent::experimental::cli::{
    AuthInput, ClientCommand, ExperimentalCommand, ServerCommand, TransportAddress, parse,
};

const UNSUPPORTED_SERVER_OPTIONS: &str = "The experimental server command does not support existing CLI options yet";
const UNSUPPORTED_CLIENT_OPTIONS: &str = "The experimental client command does not support existing CLI options yet";
const SERVER_ID: &str = "00000000-0000-4000-8000-000000000001";

fn args(argv: &[&str]) -> Vec<String> {
    argv.iter().map(|arg| arg.to_string()).collect()
}

fn client(argv: &[&str]) -> ClientCommand {
    match parse(&args(argv)) {
        Ok(ExperimentalCommand::Client(command)) => command,
        other => panic!("{argv:?} parsed to {other:?}"),
    }
}

fn server(argv: &[&str]) -> ServerCommand {
    match parse(&args(argv)) {
        Ok(ExperimentalCommand::Server(command)) => command,
        other => panic!("{argv:?} parsed to {other:?}"),
    }
}

#[test]
fn parses_server_configuration() {
    let argv = [
        "server",
        "--server-id",
        SERVER_ID,
        "--session-dir",
        "~/pi-sessions",
        "--provider",
        "anthropic",
        "--model",
        "claude-sonnet-4-5",
        "-e",
        "./first-plugin",
        "-e=./second-plugin",
    ];
    let expected = ServerCommand {
        server_id: Some(SERVER_ID.into()),
        session_dir: Some("~/pi-sessions".into()),
        provider: Some("anthropic".into()),
        model: Some("claude-sonnet-4-5".into()),
        plugin_packages: Some(vec!["./first-plugin".into(), "./second-plugin".into()]),
        auth: None,
    };
    assert_eq!(server(&argv), expected);
}

#[test]
fn parses_provider_qualified_server_models() {
    let expected = ServerCommand { model: Some("anthropic/claude-sonnet-4-5:high".into()), ..ServerCommand::default() };
    assert_eq!(server(&["server", "--model", "anthropic/claude-sonnet-4-5:high"]), expected);
}

#[test]
fn parses_client_transport_addresses() {
    let unix = TransportAddress::Unix { path: "/tmp/pi.sock".into() };
    assert_eq!(client(&["client", "--connect", "unix:///tmp/pi.sock"]).connect, Some(unix));
    let radius = format!("radius://{SERVER_ID}");
    let expected = ClientCommand {
        connect: Some(TransportAddress::Radius { server_id: SERVER_ID.into() }),
        session_id: Some("demo-1".into()),
        ..ClientCommand::default()
    };
    assert_eq!(client(&["client", "--connect", &radius, "--session-id", "demo-1"]), expected);
}

#[test]
fn parses_client_session_selection() {
    for (option, resume) in [("-c", false), ("--continue", false), ("-r", true), ("--resume", true)] {
        let expected = ClientCommand { continue_session: !resume, resume, ..ClientCommand::default() };
        assert_eq!(client(&["client", option]), expected, "{option}");
    }
}

#[test]
fn parses_a_session_selection_followed_by_a_one_shot_prompt() {
    let expected = ClientCommand { resume: true, prompt: Some("Explain this project".into()), ..Default::default() };
    assert_eq!(client(&["client", "-r", "Explain this project"]), expected);
}

#[test]
fn parses_client_model_and_plugin_configuration() {
    let argv =
        ["client", "--model", "anthropic/claude-sonnet-4-5:high", "-e", "./first-plugin", "-e", "./second-plugin"];
    let expected = ClientCommand {
        model: Some("anthropic/claude-sonnet-4-5:high".into()),
        plugin_packages: Some(vec!["./first-plugin".into(), "./second-plugin".into()]),
        ..ClientCommand::default()
    };
    assert_eq!(client(&argv), expected);
}

#[test]
fn parses_authentication_sources() {
    let cases = [
        (["--auth-token", "secret"], AuthInput::Token("secret".into())),
        (["--auth-token-file", "/tmp/token"], AuthInput::File("/tmp/token".into())),
    ];
    for (argv, auth) in cases {
        assert_eq!(server(&["server", argv[0], argv[1]]).auth, Some(auth.clone()));
        assert_eq!(client(&["client", argv[0], argv[1]]).auth, Some(auth));
    }
}

#[test]
fn permits_omitted_authentication() {
    assert_eq!(server(&["server"]), ServerCommand::default());
    assert_eq!(client(&["client"]), ClientCommand::default());
}

#[test]
fn rejects_invalid_experimental_input() {
    let cases: [(&[&str], &str); 17] = [
        (&["client", "--listen", "unix:///tmp/pi.sock"], UNSUPPORTED_CLIENT_OPTIONS),
        (&["server", "--listen", "unix:///tmp/pi.sock"], UNSUPPORTED_SERVER_OPTIONS),
        (&["server", "--connect", "unix:///tmp/pi.sock"], UNSUPPORTED_SERVER_OPTIONS),
        (&["client", "--connect", "ws://localhost:8080"], "Unsupported --connect transport \"ws:\""),
        (&["client", "--connect", "radius://not-a-server"], "Radius transport address requires"),
        (&["client", "--connect", "unix://relative.sock"], "Unix transport address must not include an authority"),
        (&["client", "--connect", "unix:///tmp/pi.sock?wrong=value"], "Invalid --connect address"),
        (&["client", "--provider", "anthropic"], "--provider requires --model"),
        (&["client", "-c", "-r"], "--session-id, --continue, and --resume are mutually exclusive"),
        (&["client", "--continue=true"], "--continue does not take a value"),
        (&["server", "--provider", "anthropic"], "--provider requires --model"),
        (&["server", "--server-id", "not-a-uuid"], "Invalid --server-id"),
        (&["server", "--server-id"], "--server-id requires a value"),
        (&["server", "--session-dir"], "--session-dir requires a value"),
        (&["client", "-e"], "-e requires a value"),
        (
            &["server", "--session-dir", "/tmp/first", "--session-dir=/tmp/second"],
            "--session-dir may only be specified once",
        ),
        (&["client", "--connect="], "--connect requires a value"),
    ];
    for (argv, error) in cases {
        let errors = parse(&args(argv)).expect_err(&format!("{argv:?} must fail"));
        assert!(errors.iter().any(|candidate| candidate.contains(error)), "{argv:?}: {errors:?}");
    }
}

#[test]
fn rejects_unsupported_options_without_parsing_them_through_the_stable_cli() {
    let errors = parse(&args(&["client", "--tui-mode", "wrong", "--model", "claude-sonnet"])).unwrap_err();
    assert_eq!(errors, [UNSUPPORTED_CLIENT_OPTIONS]);
}
