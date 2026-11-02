//! `DefaultResourceLoader` extensions: the trust preload (upstream `resource-loader.test.ts`), factories
//! in a child session, #8423 factory failure, #7193 bus lifecycle, and #8261 trust in a child session.

use pi_ai::providers::faux::FauxResponse;

use super::*;

const UNTIL: &str = r#"
    const until = async (check) => {
      for (let i = 0; i < 300 && !check(); i++) await new Promise((resolve) => setTimeout(resolve, 10));
      return check();
    };
    const child = (loader, settingsManager) => createAgentSession({ model: ctx.model, resourceLoader: loader,
      sessionManager: SessionManager.inMemory(ctx.cwd),
      settingsManager: settingsManager ?? SettingsManager.inMemory({ retry: { enabled: false }, compaction: { enabled: false } }) });
"#;

const PRELOAD: &str = r#"
    const count = () => readFileSync(join(root, 'count'), 'utf8').length;
    const loader = new DefaultResourceLoader({ cwd: ctx.cwd, agentDir });
    let pre;
    await loader.reload({ resolveProjectTrust: async ({ extensionsResult }) => {
      pre = extensionsResult.extensions.map((extension) => extension.path);
      return true;
    } });
    const final = loader.getExtensions();
    result.pre = pre;
    result.final = final.extensions.map((extension) => extension.path);
    result.commands = final.extensions.map((extension) => [...extension.commands.keys()]);
    result.errors = final.errors;
    result.count = count();
    result.handlers = await fail(() => final.extensions[0].handlers);
    await loader.reload();
    result.reloaded = count();
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trust_preload_loads_user_extensions_once_and_reuses_them() {
    let (h, _root, data) = run(PRELOAD, |h, root| {
        let count = json!(root.join("count"));
        let user = format!(
            "import {{ appendFileSync }} from 'node:fs';\nappendFileSync({count}, 'x');\n\
             export default function (pi) {{\n  pi.on('project_trust', () => ({{ trusted: 'yes' }}));\n  \
             pi.registerCommand('user-trust', {{ description: 'user trust', handler: async () => {{}} }});\n}}\n"
        );
        write(&h.agent_dir.join("extensions/user.ts"), &user);
        let project = "export default function (pi) {\n  pi.registerCommand('project-trusted', \
                       { description: 'project trusted', handler: async () => {} });\n}\n";
        write(&h.cwd.join(".pi/extensions/project.ts"), project);
        write(&root.join("count"), "");
    })
    .await;
    let user = h.agent_dir.join("extensions").join("user.ts");
    let project = h.cwd.join(".pi").join("extensions").join("project.ts");
    assert_eq!(data["pre"], json!([user]));
    assert_eq!(data["final"], json!([project, user]));
    assert_eq!(data["commands"], json!([["project-trusted"], ["user-trust"]]));
    assert_eq!(data["errors"], json!([]));
    assert_eq!(data["count"], 1, "the trust pass loaded user.ts and the final pass reused it");
    let handlers = "DefaultResourceLoader.getExtensions() handlers of a file extension is not supported in pi-rust";
    assert_eq!(data["handlers"], handlers);
    assert_eq!(data["reloaded"], 2, "reload() imports the module again");
}

const FACTORIES: &str = r#"
    const seen = { pongs: [], starts: 0 };
    let early = true;
    const factory = (pi) => {
      if (early) pi.events.emit('ping', 'early');
      early = false;
      pi.on('agent_start', () => { seen.starts += 1; });
      pi.registerTool({ name: 'echo', label: 'Echo', description: 'Echo text',
        parameters: { type: 'object', properties: { text: { type: 'string' } } },
        async execute(_id, params, signal, onUpdate, toolCtx) {
          onUpdate?.({ content: [{ type: 'text', text: 'working' }] });
          pi.appendEntry('tool-entry', { text: params.text });
          pi.events.emit('ping', params.text);
          const facts = [toolCtx.cwd === ctx.cwd, signal !== undefined, toolCtx.sessionManager.getEntries().length > 0];
          return { content: [{ type: 'text', text: `echo ${params.text} ${facts.join(' ')}` }] };
        } });
      pi.registerCommand('mark', { description: 'Mark', handler: async (args, commandCtx) => {
        pi.appendEntry('mark', { args, trusted: commandCtx.isProjectTrusted() });
      } });
    };
    const bus = join(root, 'ext', 'bus.ts');
    const loader = new DefaultResourceLoader({ cwd: ctx.cwd, agentDir, additionalExtensionPaths: [bus],
      extensionFactories: [factory] });
    loader.eventBus.on('pong', (data) => seen.pongs.push(data));
    await loader.reload();
    const { session, extensionsResult } = await child(loader);
    result.paths = extensionsResult.extensions.map((extension) => extension.path);
    result.same = extensionsResult === loader.getExtensions();
    const updates = [];
    session.subscribe((event) => {
      if (event.type === 'tool_execution_update') updates.push(event.partialResult?.content?.[0]?.text);
    });
    await session.prompt('use echo');
    await session.prompt('/mark now');
    await until(() => seen.pongs.length > 1);
    result.tool = session.messages.find((message) => message.role === 'toolResult')?.content?.[0]?.text;
    result.updates = updates;
    result.entries = session.sessionManager.getEntries().filter((entry) => entry.type === 'custom')
      .map((entry) => [entry.customType, entry.data]);
    Object.assign(result, seen);
    result.second = await fail(() => child(loader));
    session.dispose();
    result.afterDispose = await fail(() => child(loader));
    await loader.reload();
    const again = await child(loader);
    result.again = again.session.getAllTools().some((tool) => tool.name === 'echo');
    again.session.dispose();
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn factories_and_files_serve_one_child_session_per_reload() {
    let body = [UNTIL, FACTORIES].concat();
    let (h, root, data) = run(&body, |h, root| {
        h.provider.push(FauxResponse::tool_call("call-1", "echo", json!({"text": "hi"})));
        h.provider.push(FauxResponse::text("done"));
        let bus = "export default (pi) => pi.events.on('ping', (text) => pi.events.emit('pong', `pong ${text}`));\n";
        write(&root.join("ext/bus.ts"), bus);
    })
    .await;
    assert_eq!(data["paths"], json!([root.join("ext").join("bus.ts"), "<inline:1>"]));
    assert_eq!(data["same"], true);
    assert_eq!(data["tool"], "echo hi true true true");
    assert!(data["updates"].as_array().unwrap().contains(&json!("working")), "{data}");
    let entries = data["entries"].as_array().unwrap();
    assert!(entries.contains(&json!(["tool-entry", {"text": "hi"}])), "{data}");
    assert!(entries.contains(&json!(["mark", {"args": "now", "trusted": true}])), "{data}");
    assert_eq!(data["starts"], 1);
    assert_eq!(data["pongs"], json!(["pong early", "pong hi"]), "each bus listener ran once");
    let limit = "A second AgentSession on the extensions of one DefaultResourceLoader reload (call reload() first) \
                 is not supported in pi-rust";
    assert_eq!(data["second"], limit);
    assert_eq!(data["afterDispose"], limit);
    assert_eq!(data["again"], true);
    assert_eq!(h.provider.requests().len(), 2);
}

const LIFECYCLE: &str = r#"
    let captured, firstApi;
    let [failedCalls, calls, hostCalls] = [0, 0, 0];
    const failing = (pi) => {
      captured = pi;
      pi.events.on('factory-failure', () => { failedCalls += 1; });
      pi.registerFlag('failed-flag', { type: 'boolean', default: true });
      result.flagDuringLoad = pi.getFlag('failed-flag');
      throw new Error('factory failed');
    };
    const working = { name: 'working', factory: (pi) => {
      firstApi ??= pi;
      pi.events.on('reload:test', () => { calls += 1; });
    } };
    const loader = new DefaultResourceLoader({ cwd: ctx.cwd, agentDir, noExtensions: true,
      extensionFactories: [failing, working] });
    loader.eventBus.on('reload:test', () => { hostCalls += 1; });
    await loader.reload();
    result.errors = loader.getExtensions().errors;
    result.paths = loader.getExtensions().extensions.map((extension) => extension.path);
    loader.eventBus.emit('factory-failure');
    result.failedCalls = failedCalls;
    result.late = await fail(() => captured.registerFlag('late-flag', { type: 'boolean', default: true }));
    const emit = async () => {
      const [extension, host] = [calls, hostCalls];
      loader.eventBus.emit('reload:test');
      await new Promise((resolve) => setTimeout(resolve, 0));
      return { extension: calls - extension, host: hostCalls - host };
    };
    result.emits = [await emit()];
    await loader.reload();
    result.stale = await fail(() => firstApi.getCommands());
    result.emits.push(await emit());
    const { session } = await child(loader);
    result.emits.push(await emit());
    session.dispose();
    result.emits.push(await emit());
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_factories_discard_changes_and_bus_listeners_end_with_their_reload() {
    let body = [UNTIL, LIFECYCLE].concat();
    let (_h, _root, data) = run(&body, |_, _| {}).await;
    assert_eq!(data["errors"], json!([{"path": "<inline:1>", "error": "factory failed"}]));
    assert_eq!(data["paths"], json!(["<inline:working>"]));
    assert_eq!(data["flagDuringLoad"], true);
    assert_eq!(data["failedCalls"], 0);
    assert_eq!(data["late"], "Extension \"<inline:1>\" failed to load and its API is no longer active.");
    assert!(data["stale"].as_str().unwrap().contains("stale after session replacement or reload"), "{data}");
    let once = json!({"extension": 1, "host": 1});
    assert_eq!(data["emits"], json!([once, once, once, {"extension": 0, "host": 1}]));
}

const CHILD_TRUST: &str = r#"
    const make = (trusted) => new DefaultResourceLoader({ cwd: ctx.cwd, agentDir, noExtensions: true,
      settingsManager: SettingsManager.create(ctx.cwd, agentDir, { projectTrusted: trusted }),
      systemPromptOverride: () => 'Custom system prompt',
      extensionFactories: [(pi) => pi.registerCommand('trust', { handler: async (_args, commandCtx) => {
        pi.appendEntry('trust', commandCtx.isProjectTrusted());
      } })] });
    for (const trusted of [true, false]) {
      const loader = make(trusted);
      await loader.reload();
      const { session } = await child(loader, loader.settingsManager);
      await session.prompt('/trust');
      const entry = session.sessionManager.getEntries().find((each) => each.customType === 'trust');
      result[String(trusted)] = { entry: entry?.data ?? null, prompt: session.systemPrompt.includes('Custom system prompt') };
      session.dispose();
    }
    result.systemPrompt = await fail(() => createAgentSession({ model: ctx.model, resourceLoader: make(true),
      systemPrompt: 'Other', sessionManager: SessionManager.inMemory(ctx.cwd), settingsManager: SettingsManager.inMemory() }));
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_sessions_see_the_loader_trust_and_stored_overrides() {
    let body = [UNTIL, CHILD_TRUST].concat();
    let (h, _root, data) = run(&body, |_, _| {}).await;
    assert_eq!(data["true"], json!({"entry": true, "prompt": true}));
    assert_eq!(data["false"], json!({"entry": false, "prompt": true}));
    let refused = "createAgentSession.systemPrompt with a DefaultResourceLoader is not supported in pi-rust";
    assert_eq!(data["systemPrompt"], refused);
    assert!(h.provider.requests().is_empty());
}
