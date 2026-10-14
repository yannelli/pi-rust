//! Port of upstream `test/experimental-slash-commands.test.ts`.

use std::sync::{Arc, Mutex};

use pi_chord::ChordError;
use pi_chord::facets::{FacetOptions, create_facet_host, define_facet};
use pi_coding_agent::experimental::services::slash_commands::{
    SLASH_COMMANDS, SlashCommandContribution, SlashCommandRegistry, create_slash_commands_runtime_facet,
};

fn command(name: &str, description: &str) -> SlashCommandContribution {
    SlashCommandContribution::new(name, |_args, _context| async { Ok(None) }).with_description(description)
}

fn names(registry: &SlashCommandRegistry) -> Vec<String> {
    registry.list().into_iter().map(|command| command.name).collect()
}

#[test]
fn registers_and_removes_contributions() {
    let registry = SlashCommandRegistry::new();
    let snapshots = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
    let record = snapshots.clone();
    let unsubscribe = registry.subscribe(Arc::new(move |commands| {
        record.lock().unwrap().push(commands.iter().map(|command| command.name.clone()).collect());
    }));
    let close = registry.register(command("hello", "Hello")).unwrap();
    assert_eq!(names(&registry), ["hello"]);
    let error = registry.register(command("hello", "Again")).err().unwrap();
    assert!(error.to_string().contains("already registered"));
    close.close();
    close.close();
    assert!(registry.list().is_empty());
    assert_eq!(*snapshots.lock().unwrap(), [vec![], vec!["hello".to_string()], vec![]]);
    unsubscribe();
}

#[test]
fn stages_replacements_until_the_previous_registration_retires() {
    let registry = SlashCommandRegistry::new();
    let (first, second) = (command("hello", "First"), command("hello", "Second"));
    let close_first = registry.register(first.clone()).unwrap();
    let close_second = registry.replace(second.clone()).unwrap();
    assert!(registry.list()[0].same_run(&first));

    close_second.close();
    assert!(registry.list()[0].same_run(&first));
    let close_replacement = registry.replace(second.clone()).unwrap();
    close_first.close();
    assert!(registry.list()[0].same_run(&second));
    close_replacement.close();
    assert!(registry.list().is_empty());
}

fn hello_facet(command: SlashCommandContribution, failure: Option<ChordError>) -> Arc<dyn pi_chord::facets::Facet> {
    define_facet("@test/example-hello", move |env| {
        let commands = env.use_local(&SLASH_COMMANDS)?;
        let (activation_env, command, failure) = (env.clone(), command.clone(), failure.clone());
        env.on_activate(move || {
            let (env, commands, command, failure) =
                (activation_env.clone(), commands.clone(), command.clone(), failure.clone());
            async move {
                env.own(commands.get()?.replace(command)?.into_disposal())?;
                failure.map_or(Ok(()), Err)
            }
        })
    })
}

#[tokio::test]
async fn tracks_plugin_facet_reload_and_unload() {
    let registry = SlashCommandRegistry::new();
    let original = command("hello", "Original");
    let facets = vec![create_slash_commands_runtime_facet(registry.clone()), hello_facet(original.clone(), None)];
    let host = create_facet_host(FacetOptions::new(facets)).await.unwrap();
    assert_eq!(names(&registry), ["hello"]);

    let failure = ChordError::error("replacement failed");
    let failed = host.reload(vec![hello_facet(command("hello", "Failing"), Some(failure))]).await.unwrap_err();
    assert!(failed.to_string().contains("replacement failed"));
    assert_eq!(registry.list().len(), 1);
    assert!(registry.list()[0].same_run(&original));

    let replacement = command("hello", "Replacement");
    host.reload(vec![hello_facet(replacement.clone(), None)]).await.unwrap();
    let listed = registry.list();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].description.as_deref(), Some("Replacement"));
    assert!(listed[0].same_run(&replacement));

    host.dispose().await.unwrap();
    assert!(registry.list().is_empty());
}
