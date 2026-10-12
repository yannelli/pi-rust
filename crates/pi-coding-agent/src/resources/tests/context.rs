//! Context file tests from upstream `test/resource-loader.test.ts`, including the nested
//! linked-worktree cases.

use std::path::{Path, PathBuf};

use super::write;
use crate::resources::load_project_context_files;

fn contents(cwd: &Path, agent: &Path) -> Vec<String> {
    load_project_context_files(cwd, agent).into_iter().map(|file| file.content).collect()
}

fn paths(cwd: &Path, agent: &Path) -> Vec<PathBuf> {
    load_project_context_files(cwd, agent).into_iter().map(|file| file.path).collect()
}

#[test]
fn prefers_override_per_directory_and_skips_directories() {
    let temp = tempfile::tempdir().unwrap();
    let (agent, cwd) = (temp.path().join("agent"), temp.path().join("project"));
    let nested = cwd.join("service");
    write(&agent.join("AGENTS.md"), "global instructions");
    write(&agent.join("AGENTS.override.md"), "global override");
    write(&cwd.join("AGENTS.md"), "project instructions");
    write(&nested.join("AGENTS.md"), "service instructions");
    write(&nested.join("AGENTS.override.md"), "service override");
    assert_eq!(
        load_project_context_files(&nested, &agent).into_iter().map(|f| (f.path, f.content)).collect::<Vec<_>>(),
        [
            (agent.join("AGENTS.override.md"), "global override".to_string()),
            (cwd.join("AGENTS.md"), "project instructions".to_string()),
            (nested.join("AGENTS.override.md"), "service override".to_string()),
        ]
    );

    let other = temp.path().join("other");
    std::fs::create_dir_all(other.join("AGENTS.override.md")).unwrap();
    std::fs::create_dir_all(other.join("AGENTS.md")).unwrap();
    write(&other.join("CLAUDE.md"), "\u{feff}Fallback instructions");
    let files = load_project_context_files(&other, &temp.path().join("none"));
    assert_eq!(files.last().unwrap().path, other.join("CLAUDE.md"));
    assert_eq!(files.last().unwrap().content, "Fallback instructions");
}

fn link_worktree(main: &Path, worktree: &Path, name: &str) -> PathBuf {
    let git_dir = main.join(".git/worktrees").join(name);
    write(&main.join(".git/HEAD"), "ref: refs/heads/main\n");
    write(&git_dir.join("HEAD"), "ref: refs/heads/feat\n");
    write(&git_dir.join("commondir"), "../..");
    write(&worktree.join(".git"), &format!("gitdir: {}\n", git_dir.display()));
    git_dir
}

struct Nested {
    _temp: tempfile::TempDir,
    agent: PathBuf,
    outer: PathBuf,
    main: PathBuf,
    worktree: PathBuf,
    src: PathBuf,
}

fn nested_worktree() -> Nested {
    let temp = tempfile::tempdir().unwrap();
    let outer = temp.path().join("outer");
    let main = outer.join("main");
    let worktree = main.join("worktrees/feat");
    let src = worktree.join("src");
    std::fs::create_dir_all(&src).unwrap();
    link_worktree(&main, &worktree, "feat");
    let agent = temp.path().join("agent");
    Nested { _temp: temp, agent, outer, main, worktree, src }
}

#[test]
fn nested_worktree_skips_the_main_checkout_duplicate() {
    let n = nested_worktree();
    write(&n.main.join("AGENTS.md"), "main repo instructions");
    write(&n.worktree.join("AGENTS.md"), "worktree instructions");
    assert_eq!(contents(&n.src, &n.agent), ["worktree instructions"]);

    write(&n.outer.join("AGENTS.md"), "outer instructions");
    assert_eq!(contents(&n.src, &n.agent), ["outer instructions", "worktree instructions"]);
}

#[cfg(unix)]
#[test]
fn nested_worktree_handles_symlinked_context_files() {
    let n = nested_worktree();
    write(&n.main.join("AGENTS.md"), "main repo instructions");
    std::os::unix::fs::symlink(n.main.join("AGENTS.md"), n.worktree.join("AGENTS.md")).unwrap();
    assert_eq!(paths(&n.src, &n.agent), [n.worktree.join("AGENTS.md")]);

    let m = nested_worktree();
    let shared = m.outer.join("shared-agents.md");
    write(&shared, "shared instructions");
    std::os::unix::fs::symlink(&shared, m.main.join("AGENTS.md")).unwrap();
    std::os::unix::fs::symlink(&shared, m.worktree.join("AGENTS.md")).unwrap();
    assert_eq!(paths(&m.src, &m.agent), [m.worktree.join("AGENTS.md")]);
}

#[test]
fn nested_worktree_keeps_inherited_and_differently_named_files() {
    let n = nested_worktree();
    write(&n.main.join("AGENTS.md"), "main repo instructions");
    assert_eq!(contents(&n.src, &n.agent), ["main repo instructions"]);

    let m = nested_worktree();
    write(&m.main.join("CLAUDE.md"), "main repo instructions");
    write(&m.worktree.join("AGENTS.md"), "worktree instructions");
    assert_eq!(contents(&m.src, &m.agent), ["main repo instructions", "worktree instructions"]);
}

#[test]
fn bare_sibling_submodule_and_plain_layouts_climb_normally() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");

    let proj = temp.path().join("proj");
    let bare = proj.join(".bare");
    let worktree = proj.join("main");
    let gitdir = bare.join("worktrees/main");
    write(&bare.join("HEAD"), "ref: refs/heads/main\n");
    write(&gitdir.join("HEAD"), "ref: refs/heads/main\n");
    write(&gitdir.join("commondir"), "../..");
    write(&worktree.join(".git"), &format!("gitdir: {}\n", gitdir.display()));
    write(&proj.join("AGENTS.md"), "container instructions");
    write(&worktree.join("AGENTS.md"), "worktree instructions");
    assert_eq!(contents(&worktree, &agent), ["container instructions", "worktree instructions"]);

    let outer = temp.path().join("outer");
    let sibling = outer.join("sib-feat");
    std::fs::create_dir_all(sibling.join("src")).unwrap();
    std::fs::create_dir_all(outer.join("main")).unwrap();
    write(&outer.join("AGENTS.md"), "outer instructions");
    write(&sibling.join("AGENTS.md"), "sibling worktree instructions");
    link_worktree(&outer.join("main"), &sibling, "sib");
    assert_eq!(contents(&sibling.join("src"), &agent), ["outer instructions", "sibling worktree instructions"]);

    let sup = temp.path().join("super");
    let sub = sup.join("vendor/lib");
    std::fs::create_dir_all(sub.join("src")).unwrap();
    write(&sup.join("AGENTS.md"), "superproject instructions");
    write(&sub.join("AGENTS.md"), "submodule instructions");
    let sub_git = sup.join(".git/modules/vendor/lib");
    write(&sub_git.join("HEAD"), "ref: refs/heads/main\n");
    write(&sub.join(".git"), &format!("gitdir: {}\n", sub_git.display()));
    assert_eq!(contents(&sub.join("src"), &agent), ["superproject instructions", "submodule instructions"]);

    let corrupt = temp.path().join("corrupt");
    write(&corrupt.join(".git"), "gitdir: /nonexistent/path/worktrees/feat\n");
    write(&corrupt.join("AGENTS.md"), "repo instructions");
    write(&corrupt.join("src/AGENTS.md"), "src instructions");
    assert_eq!(contents(&corrupt.join("src"), &agent), ["repo instructions", "src instructions"]);
}

#[test]
fn config_delegates_to_the_resource_module() {
    let temp = tempfile::tempdir().unwrap();
    write(&temp.path().join("AGENTS.md"), "here");
    let files = crate::config::load_context_files(temp.path(), &temp.path().join("agent"));
    assert_eq!(files.last().unwrap().content, "here");
}
