//! Watch cases. Each step waits up to three seconds for the change to be reported.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{EnvConformanceOptions, Watching, context, shell_argv};
use crate::env::{FileChange, RemoveOptions, SharedEnv, WatchExclude, WatchMode, WatchTarget};

fn target(path: &str, recursive: bool) -> WatchTarget {
    WatchTarget { path: path.into(), recursive, exclude: None }
}

async fn write(env: &SharedEnv, path: &str, content: &str) {
    env.write_file(path, content.as_bytes(), &context()).await.unwrap();
}

async fn rename(env: &SharedEnv, from: &str, to: &str) {
    env.rename_file(from, to, &context()).await.unwrap();
}

pub async fn watch_reports_a_missing_files_creation_changes_replacement_and_removal(
    env: SharedEnv,
    _options: &EnvConformanceOptions,
) {
    let watching = Watching::start(&env, &[target("AGENTS.md", false)]).await;
    watching.expect_change("AGENTS.md", write(&env, "AGENTS.md", "one")).await;
    watching.expect_change("AGENTS.md", write(&env, "AGENTS.md", "two!")).await;
    // Editors replace a file by renaming a new one over it.
    let replace = async {
        write(&env, "AGENTS.md.tmp", "three").await;
        rename(&env, "AGENTS.md.tmp", "AGENTS.md").await;
    };
    watching.expect_change("AGENTS.md", replace).await;
    watching.expect_change("AGENTS.md", write(&env, "AGENTS.md", "four")).await;
    let remove = async { env.remove("AGENTS.md", RemoveOptions::default(), &context()).await.unwrap() };
    watching.expect_change("AGENTS.md", remove).await;
    watching.close().await;
}

pub async fn watch_reports_a_missing_target_whose_ancestors_are_created(
    env: SharedEnv,
    _options: &EnvConformanceOptions,
) {
    let watching = Watching::start(&env, &[target("a/b/c/AGENTS.md", false)]).await;
    watching.expect_change("a/b/c/AGENTS.md", write(&env, "a/b/c/AGENTS.md", "x")).await;
    watching.close().await;
}

pub async fn watch_follows_directories_created_together_with_their_contents(
    env: SharedEnv,
    _options: &EnvConformanceOptions,
) {
    env.create_dir("skills", true, &context()).await.unwrap();
    let watching = Watching::start(&env, &[target("skills", true)]).await;
    // Written before any watcher on the new directories can exist.
    watching.expect_change("skills/a/b/SKILL.md", write(&env, "skills/a/b/SKILL.md", "one")).await;
    watching.expect_change("skills/a/b/SKILL.md", write(&env, "skills/a/b/SKILL.md", "two!")).await;
    watching.expect_change("skills/a/b/c/SKILL.md", write(&env, "skills/a/b/c/SKILL.md", "deeper")).await;
    watching.close().await;
}

pub async fn watch_keeps_watching_a_path_whose_parent_is_renamed_and_recreated(
    env: SharedEnv,
    _options: &EnvConformanceOptions,
) {
    write(&env, "proj/.pi/skills/x.md", "x").await;
    let watching = Watching::start(&env, &[target("proj/.pi/skills", true)]).await;
    watching.expect_change("proj/.pi/skills", rename(&env, "proj/.pi", "proj/old")).await;
    watching.expect_change("proj/.pi/skills/y.md", write(&env, "proj/.pi/skills/y.md", "y")).await;
    watching.expect_change("proj/.pi/skills/y.md", write(&env, "proj/.pi/skills/y.md", "yy")).await;
    watching.close().await;
}

pub async fn watch_skips_excluded_entries_and_reports_a_rename_out_of_them(
    env: SharedEnv,
    _options: &EnvConformanceOptions,
) {
    env.create_dir("skills", true, &context()).await.unwrap();
    let exclude = WatchExclude { hidden: true, names: vec!["node_modules".into()] };
    let targets = [WatchTarget { path: "skills".into(), recursive: true, exclude: Some(exclude) }];
    let watching = Watching::start(&env, &targets).await;
    write(&env, "skills/node_modules/dep/SKILL.md", "dep").await;
    write(&env, "skills/.SKILL.md.tmp", "draft").await;
    watching.expect_change("skills/SKILL.md", rename(&env, "skills/.SKILL.md.tmp", "skills/SKILL.md")).await;
    let hidden = [watching.absolute("skills/node_modules").await, watching.absolute("skills/.SKILL.md.tmp").await];
    for change in watching.recorded() {
        let FileChange::Paths(paths) = change else { continue };
        for path in paths {
            assert!(!hidden.iter().any(|excluded| path.starts_with(excluded.as_str())), "excluded {path}");
        }
    }
    watching.close().await;
}

pub async fn watch_keeps_recursive_coverage_where_a_non_recursive_target_overlaps(
    env: SharedEnv,
    _options: &EnvConformanceOptions,
) {
    write(&env, "skills/a/one.md", "one").await;
    let watching = Watching::start(&env, &[target("skills", false), target("skills", true)]).await;
    watching.expect_change("skills/a/two.md", write(&env, "skills/a/two.md", "two")).await;
    watching.close().await;
}

pub async fn watch_follows_a_directory_replaced_at_the_same_path(env: SharedEnv, _options: &EnvConformanceOptions) {
    write(&env, "skills/a/x.md", "x").await;
    let watching = Watching::start(&env, &[target("skills", true)]).await;
    let replace = async {
        rename(&env, "skills/a", "skills-old").await;
        env.create_dir("skills/a", true, &context()).await.unwrap();
    };
    watching.expect_change("skills/a", replace).await;
    watching.expect_change("skills/a/y.md", write(&env, "skills/a/y.md", "y")).await;
    watching.expect_change("skills/a/y.md", write(&env, "skills/a/y.md", "yy")).await;
    watching.close().await;
}

pub async fn watch_stops_reporting_once_closed(env: SharedEnv, _options: &EnvConformanceOptions) {
    let changes = Arc::new(Mutex::new(Vec::new()));
    let sink = changes.clone();
    let listener = Arc::new(move |change| sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(change));
    let watcher = env.watch(&[target("file.txt", false)], listener, &context()).await.unwrap();
    assert!(matches!(watcher.mode(), WatchMode::Native | WatchMode::Polling), "watcher reports its mode");
    watcher.close(&context()).await;
    watcher.close(&context()).await;
    write(&env, "file.txt", "x").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(changes.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).is_empty());
}

pub async fn watch_reports_changes_to_the_file_a_watched_symbolic_link_points_to(
    env: SharedEnv,
    options: &EnvConformanceOptions,
) {
    if !options.symlinks {
        return;
    }
    write(&env, "data/real.md", "one").await;
    env.create_dir("config", true, &context()).await.unwrap();
    let command = shell_argv(options, &["ln -s ../data/real.md config/AGENTS.md"]);
    let linked = env.exec(command, Default::default(), &context()).await;
    assert_eq!(linked.unwrap().exit_code, 0);
    let watching = Watching::start(&env, &[target("config/AGENTS.md", false)]).await;
    watching.expect_change("config/AGENTS.md", write(&env, "data/real.md", "two!")).await;
    watching.close().await;
}
