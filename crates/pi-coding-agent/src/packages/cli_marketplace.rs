use super::cli::PackageCommandOptions;
use super::cli_support::Io;
use super::marketplace::{InstalledPackage, Marketplace, MarketplaceEntry};

pub(super) async fn run(action: &str, args: &[String], options: &PackageCommandOptions, io: &Io) -> i32 {
    let valid = args.len() <= 1 && (action != "info" || args.len() == 1);
    if !valid || args.first().is_some_and(|arg| arg.starts_with('-')) {
        io.error(if action == "info" {
            "Usage: pi extensions info <name>"
        } else {
            "Usage: pi extensions search [query]"
        });
        return 1;
    }
    let marketplace = Marketplace::new(
        options.agent_dir.clone(),
        options.http_client.clone().unwrap_or_else(pi_ai::http::shared_client),
        options.offline.unwrap_or_else(super::is_offline_mode_enabled),
    );
    match browse(&marketplace, action, args.first().map(String::as_str)).await {
        Ok(text) => {
            io.line(&text);
            0
        }
        Err(error) => {
            io.error(&format!("Error: {error:#}"));
            1
        }
    }
}

async fn browse(marketplace: &Marketplace, action: &str, query: Option<&str>) -> anyhow::Result<String> {
    let catalog = marketplace.browse(None).await?;
    let index = catalog.index;
    let installed = catalog.installed;
    if action == "info" {
        let name = query.unwrap_or_default().strip_prefix("marketplace:").unwrap_or(query.unwrap_or_default());
        let entry = index
            .extensions
            .iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| anyhow::anyhow!("Marketplace extension not found: {name}"))?;
        return Ok(format!(
            "{} ({})\n{}\nVersion: {}\nRelease: {}\nInstalled: {}\nPlatforms: {}\nTags: {}\nInstall: pi install marketplace:{}",
            entry.display_name,
            entry.name,
            entry.description,
            entry.version,
            index.tag,
            installed.iter().find(|package| package.name == entry.name).map_or("no", |package| package.tag.as_str()),
            entry.platforms.join(", "),
            entry.tags.join(", "),
            entry.name
        ));
    }
    Ok(render_search(&index.extensions, &installed, query.unwrap_or_default()))
}

fn render_search(entries: &[MarketplaceEntry], installed: &[InstalledPackage], query: &str) -> String {
    let query = query.to_lowercase();
    let lines: Vec<_> = entries
        .iter()
        .filter(|entry| {
            [&entry.name, &entry.display_name, &entry.description]
                .into_iter()
                .chain(entry.tags.iter())
                .any(|text| text.to_lowercase().contains(&query))
        })
        .map(|entry| {
            let state = installed
                .iter()
                .find(|package| package.name == entry.name)
                .map_or(String::new(), |package| format!(" [installed: {}]", package.tag));
            format!("{} ({}){state}\n  {}", entry.name, entry.version, entry.description)
        })
        .collect();
    if lines.is_empty() { "No marketplace extensions found.".into() } else { lines.join("\n") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packages::{CommandOutput, run_config_command, run_package_command};

    #[tokio::test]
    async fn marketplace_commands_validate_arguments_and_honor_offline() {
        let dir = tempfile::tempdir().unwrap();
        let mut options = PackageCommandOptions::new(dir.path().join("project"), dir.path().join("agent"));
        options.offline = Some(true);
        options.home = dir.path().into();
        let (stderr, errors) = CommandOutput::capture();
        options.stderr = stderr;
        options.stdout = CommandOutput::capture().0;
        for args in [
            vec!["extensions", "info"],
            vec!["extensions", "search", "a", "b"],
            vec!["extensions", "info", "--unknown"],
        ] {
            let args = args.into_iter().map(String::from).collect::<Vec<_>>();
            assert_eq!(run_config_command(&args, &options, None).await, Some(1));
            assert!(String::from_utf8(std::mem::take(&mut *errors.lock().unwrap())).unwrap().contains("Usage:"));
        }
        for args in [
            vec!["extensions", "search"],
            vec!["extensions", "info", "todo"],
            vec!["extensions", "install", "marketplace:todo"],
            vec!["install", "marketplace:todo"],
        ] {
            let args = args.into_iter().map(String::from).collect::<Vec<_>>();
            let code = if args[0] == "extensions" {
                run_config_command(&args, &options, None).await
            } else {
                run_package_command(&args, &options).await
            };
            assert_eq!(code, Some(1));
            let error = String::from_utf8(std::mem::take(&mut *errors.lock().unwrap())).unwrap();
            assert!(error.to_lowercase().contains("offline"), "{error}");
        }
        assert!(!options.agent_dir.join("marketplace/packages/todo/package.json").exists());
    }

    #[test]
    fn search_matches_name_description_and_tags() {
        let entries: Vec<MarketplaceEntry> = serde_json::from_value(serde_json::json!([
            {"name":"todo", "displayName":"Todo", "description":"Manage tasks", "version":"1.0.0",
             "binary":"pi-rust-ext", "args":["todo"], "tags":["productivity"], "platforms":[]}
        ]))
        .unwrap();
        for query in ["TODO", "tasks", "PRODUCTIVITY", ""] {
            assert!(render_search(&entries, &[], query).contains("todo (1.0.0)"));
        }
        assert_eq!(render_search(&entries, &[], "unmatched"), "No marketplace extensions found.");
    }
}
