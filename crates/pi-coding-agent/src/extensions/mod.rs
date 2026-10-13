//! Extensions (upstream `core/extensions/*`).

pub mod vendor;

#[cfg(test)]
mod probe {
    use pi_js::{JsRuntime, JsRuntimeOptions, VirtualModule};

    #[tokio::test(flavor = "multi_thread")]
    async fn probe_vendor() {
        let root = super::vendor::vendor_root();
        let runtime = JsRuntime::new(JsRuntimeOptions::new(std::env::temp_dir())).unwrap();
        runtime.register_source_files(super::vendor::source_files(&root));
        let typebox = super::vendor::specifier(&root, "typebox/build/index.mjs");
        runtime.register_virtual_module("typebox", VirtualModule::Source(format!("export * from \"{typebox}\";")));
        for sub in ["compile", "value"] {
            let path = super::vendor::specifier(&root, &format!("typebox/build/{sub}/index.mjs"));
            runtime.register_virtual_module(format!("typebox/{sub}"), VirtualModule::Source(format!("export * from \"{path}\";")));
        }
        for key in ["typebox/build/index.mjs", "pi-ai/src/utils/json-parse.ts", "pi-ai/src/utils/event-stream.ts", "pi-ai/src/utils/validation.ts", "pi-ai/src/utils/transcript.ts", "pi-tui/src/index.ts"] {
            let spec = super::vendor::specifier(&root, key);
            let start = std::time::Instant::now();
            match runtime.import(spec.clone()).await {
                Ok(module) => eprintln!("OK {key} {:?} {:?}", start.elapsed(), module.to_json().await.map(|v| v.as_object().map(|o| o.len()))),
                Err(error) => eprintln!("FAIL {key}: {}", error.display_with_stack()),
            }
        }
        let value = runtime.eval(format!("import(\"{}\").then(m => JSON.stringify([m.parseStreamingJson('{{\"a\": [1, 2'), ]))", super::vendor::specifier(&root, "pi-ai/src/utils/json-parse.ts"))).await;
        eprintln!("{value:?}");
        let value = runtime.eval(format!("import(\"{}\").then(m => {{ const t = new m.Text('hello world', 1, 0); return JSON.stringify([t.render(8), m.visibleWidth('\\u4e2d\\u6587'), m.truncateToWidth('abcdefgh', 5)]) }})", super::vendor::specifier(&root, "pi-tui/src/index.ts"))).await;
        eprintln!("{value:?}");
        let value = runtime.eval("import('typebox').then(m => JSON.stringify(m.Type.Object({a: m.Type.String({description: 'x'})})))").await;
        eprintln!("{value:?}");
    }
}
