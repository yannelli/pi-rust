//! Theme color mode and validation (upstream `resource-loader-theme.test.ts`), packages, `~`, refused and
//! accepted options, and prompt overrides that reenter `_sdkOptions()`.

use super::*;

const THEMES: &str = r#"
    const themePath = join(root, 'capability-test.json');
    const make = (trueColor) => new DefaultResourceLoader({ cwd: ctx.cwd, agentDir,
      settingsManager: SettingsManager.inMemory({ terminal: { trueColor } }), additionalThemePaths: [themePath],
      noExtensions: true, noSkills: true, noPromptTemplates: true, noContextFiles: true });
    const bg = async (loader) => {
      await loader.reload();
      return loader.getThemes().themes.find((theme) => theme.name === 'capability-test')?.bg('userMessageBg', 'x') ?? null;
    };
    result.truecolor = await bg(make(true));
    result.color256 = await bg(make(false));
    result.auto = await bg(make(undefined));
    const strict = make(undefined);
    const json = JSON.parse(readFileSync(themePath, 'utf-8'));
    writeFileSync(themePath, JSON.stringify({ ...json, unsupported: true }));
    await strict.reload();
    result.first = strict.getThemes().diagnostics;
    writeFileSync(themePath, JSON.stringify(json));
    await strict.reload();
    result.fixed = { diagnostics: strict.getThemes().diagnostics, names: strict.getThemes().themes.map((theme) => theme.name) };
    writeFileSync(themePath, JSON.stringify({ ...json, unsupported: true }));
    await strict.reload();
    result.again = strict.getThemes().diagnostics;
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn theme_color_mode_follows_the_true_color_setting() {
    let (_h, root, data) =
        run(THEMES, |_, root| write(&root.join("capability-test.json"), &theme_json("capability-test"))).await;
    let truecolor = "\x1b[48;2;60;53;68mx\x1b[49m";
    let color256 = "\x1b[48;5;59mx\x1b[49m";
    assert_eq!(data["truecolor"], truecolor);
    assert_eq!(data["color256"], color256);
    let detected = pi_tui::terminal_image::detect_capabilities_with(&pi_tui::env::ProcessEnv, &|| false).true_color;
    assert_eq!(data["auto"], if detected { truecolor } else { color256 });
    let first = data["first"].as_array().unwrap();
    assert_eq!(first.len(), 1, "{first:?}");
    assert_eq!(first[0]["type"], "warning");
    assert_eq!(first[0]["path"], json!(root.join("capability-test.json")));
    assert!(first[0]["message"].as_str().unwrap().contains("/unsupported"), "{first:?}");
    assert_eq!(data["fixed"]["diagnostics"], json!([]));
    assert!(data["fixed"]["names"].as_array().unwrap().contains(&json!("capability-test")));
    assert_eq!(data["again"], data["first"]);
}

const LIMITS: &str = r#"
    const loader = new DefaultResourceLoader({ cwd: ctx.cwd, agentDir });
    await loader.reload();
    result.packageSkills = loader.getSkills().skills.map((skill) => skill.name);
    result.packageOptions = await fail(() => loader._sdkOptions());
    const make = (options) => new DefaultResourceLoader({ cwd: ctx.cwd, agentDir, ...options });
    result.tilde = (({ cwd, agentDir }) => ({ cwd, agentDir }))(make({ cwd: '~/proj', agentDir: '~/agent' }));
    const adding = make({ noExtensions: true, extensionsOverride: (base) => ({ ...base, extensions: [{ path: 'x' }] }) });
    result.blocked = [
      await fail(() => make({ extensionFactories: [{ name: 'core', factory: () => {}, builtin: true }] })),
      await fail(() => make({ additionalExtensionPaths: ['npm:some-package'] })),
      await fail(() => adding.reload()),
      await fail(() => make({ eventBus: {}, noExtensions: true })),
      await fail(() => make({ settingsManager: { isProjectTrusted: () => true } })),
      await fail(() => make({ noExtensions: true }).reload({ resolveProjectTrust: async () => true })),
      await fail(() => make({ additionalThemePaths: ['t.json'] })._sdkOptions()),
    ];
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packages_tilde_and_option_limits() {
    let (_h, root, data) = run(LIMITS, |h, root| {
        let package = root.join("pkg");
        write(&package.join("package.json"), r#"{"name":"local-pkg","version":"1.0.0","pi":{"skills":["./skills"]}}"#);
        skill(&package.join("skills/pkg-skill"), "pkg-skill");
        write(&h.agent_dir.join("settings.json"), &json!({"packages": [package]}).to_string());
    })
    .await;
    assert!(data["packageSkills"].as_array().unwrap().contains(&json!("pkg-skill")), "{data}");
    assert_eq!(data["packageOptions"], Value::Null, "a child session shares the loader's packages");
    let home = root.join("home");
    assert_eq!(data["tilde"], json!({"cwd": home.join("proj"), "agentDir": home.join("agent")}));
    let member = |name: &str| json!(format!("{name} is not supported in pi-rust"));
    let blocked = json!([
        member("DefaultResourceLoader.extensionFactories builtin entries"),
        member("DefaultResourceLoader.additionalExtensionPaths package sources"),
        member("DefaultResourceLoader.extensionsOverride adding extensions"),
        Value::Null,
        member("DefaultResourceLoader.settingsManager"),
        Value::Null,
        Value::Null,
    ]);
    assert_eq!(data["blocked"], blocked);
}

const REENTRY: &str = r#"
    const read = (options) => ({ systemPrompt: options.systemPrompt ?? null, appendSystemPrompt: options.appendSystemPrompt });
    const make = (failOnce) => {
      const probe = { calls: [], nested: [] };
      let failed = !failOnce;
      const hook = (kind) => (base) => {
        probe.calls.push(kind);
        if (!failed) { failed = true; throw new Error('override failed'); }
        if (!failOnce && probe.calls.filter((call) => call === kind).length === 1) {
          probe.nested.push(read(probe.loader._sdkOptions()));
        }
        return base;
      };
      probe.loader = new DefaultResourceLoader({ cwd: ctx.cwd, agentDir, noExtensions: true, noSkills: true,
        noPromptTemplates: true, noThemes: true, noContextFiles: true,
        systemPromptOverride: hook('systemPrompt'), appendSystemPromptOverride: hook('appendSystemPrompt') });
      return probe;
    };
    const report = ({ loader, calls, nested }, extra) => ({ calls, nested, ...extra,
      sources: { source: loader.getSystemPromptSource() ?? null, append: loader.getAppendSystemPromptSources() } });
    const lazy = make(false);
    const options = read(lazy.loader._sdkOptions());
    lazy.loader._sdkOptions();
    result.lazy = report(lazy, { options });
    const reloaded = make(false);
    await reloaded.loader.reload();
    result.reloaded = report(reloaded, {});
    const failing = make(true);
    const error = await fail(() => failing.loader._sdkOptions());
    failing.loader._sdkOptions();
    result.failing = report(failing, { error });
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_overrides_reentering_options_keep_their_sources() {
    let (h, _root, data) = run(REENTRY, |h, _| {
        write(&h.agent_dir.join("SYSTEM.md"), "Global system prompt.");
        write(&h.agent_dir.join("APPEND_SYSTEM.md"), "Global append.");
    })
    .await;
    let calls = json!(["systemPrompt", "appendSystemPrompt"]);
    let nested = json!([
        {"systemPrompt": null, "appendSystemPrompt": []},
        {"systemPrompt": "Global system prompt.", "appendSystemPrompt": []},
    ]);
    let (system, append) = (h.agent_dir.join("SYSTEM.md"), h.agent_dir.join("APPEND_SYSTEM.md"));
    let sources = json!({"source": {"path": system}, "append": [{"path": append}]});
    for case in ["lazy", "reloaded"] {
        assert_eq!(data[case]["calls"], calls, "{case}: {data}");
        assert_eq!(data[case]["nested"], nested, "{case}: {data}");
        assert_eq!(data[case]["sources"], sources, "{case}: {data}");
    }
    let options = json!({"systemPrompt": "Global system prompt.", "appendSystemPrompt": ["Global append."]});
    assert_eq!(data["lazy"]["options"], options);
    assert_eq!(data["failing"]["error"], "override failed");
    assert_eq!(data["failing"]["calls"], json!(["systemPrompt", "systemPrompt", "appendSystemPrompt"]));
    assert_eq!(data["failing"]["sources"], sources);
}
