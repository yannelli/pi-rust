//! JavaScript facets from a bundle talking to Rust facets through the QuickJS bridge.
#![cfg(feature = "js")]

use std::path::Path;
use std::sync::{Arc, Mutex};

use pi_chord::facets::{Facet, FacetOptions, create_facet_host, define_facet};
use pi_chord::node::{
    BundleFacetsOptions, FacetBundleLoaderOptions, FacetBundlePlugin, QuickJsFacetEvaluator, bundle_facets,
    create_facet_bundle_loader,
};
use pi_chord::services::{ServiceHandle, ServiceImpl, ServiceRef, observe_handler};
use pi_chord::{BACKGROUND_CONTEXT, Json, json};

const ENTRY: &str = r#"
import { defineFacet, defineService } from "@earendil-works/chord";
const Counter = defineService("test.counter");
const Greeter = defineService("test.greeter");
const Workers = defineService("test.worker");
export default defineFacet({
  id: "js-counter",
  setup(env) {
    const count = env.replicatedState({ value: 0, greeting: "" });
    const greeter = env.use(Greeter);
    env.provide(Counter, {
      count,
      increment(by) {
        count.change(undefined, (draft) => { draft.value += by; });
        return count.value.value;
      },
      async greet(name) {
        const text = await greeter.greet(name);
        count.change(undefined, (draft) => { draft.greeting = text; });
        return text;
      },
    });
    const workers = env.provideMany(Workers);
    env.onActivate(async () => {
      await greeter.greet("activate");
      workers.spawn("one", { name() { return "worker one"; } });
    });
    env.own(() => { globalThis.__disposed = true; });
  },
});
"#;

async fn load(directory: &Path) -> (Arc<QuickJsFacetEvaluator>, Vec<Arc<dyn Facet>>) {
    let entry = directory.join("entry.js");
    std::fs::write(&entry, ENTRY).unwrap();
    let plugin = FacetBundlePlugin { id: "bridge".into(), version: None };
    let entries = [("worker".to_string(), entry)].into_iter().collect();
    let result = bundle_facets(BundleFacetsOptions::new(plugin, entries, directory.join("out"))).await.unwrap();
    let evaluator = QuickJsFacetEvaluator::new(directory).await.unwrap();
    let options = FacetBundleLoaderOptions::new(&result.manifest_path, "worker", evaluator.clone());
    let loaded = create_facet_bundle_loader(options).unwrap().load().await.unwrap();
    (evaluator, loaded.facets)
}

#[tokio::test(flavor = "multi_thread")]
async fn javascript_facets_provide_and_consume_services_and_state() {
    let temporary = tempfile::tempdir().unwrap();
    let (evaluator, js_facets) = load(temporary.path()).await;
    let greeted: Arc<Mutex<Vec<String>>> = Arc::default();
    let calls = greeted.clone();
    let greeter = define_facet("rust-greeter", move |env| {
        let calls = calls.clone();
        let implementation = ServiceImpl::new().method("greet", move |args, _| {
            let name = args.first().and_then(Json::as_str).unwrap_or_default().to_string();
            calls.lock().unwrap().push(name.clone());
            async move { Ok(Some(Json::from(format!("hello {name}")))) }
        });
        env.provide(&ServiceRef::remote("test.greeter"), implementation)
    });
    let handles: Arc<Mutex<Option<ServiceHandle>>> = Arc::default();
    let workers: Arc<Mutex<Vec<Option<Json>>>> = Arc::default();
    let (slot, names) = (handles.clone(), workers.clone());
    let consumer = define_facet("rust-consumer", move |env| {
        *slot.lock().unwrap() = Some(env.use_service(&ServiceRef::remote("test.counter"))?);
        let names = names.clone();
        let handler = observe_handler(move |worker, context| {
            let names = names.clone();
            async move {
                let name = worker.call("name", Vec::new(), &context).await?;
                names.lock().unwrap().push(name);
                Ok(())
            }
        });
        env.observe(&ServiceRef::remote("test.worker"), handler)
    });
    let mut facets = vec![greeter, consumer];
    facets.extend(js_facets);
    let host = create_facet_host(FacetOptions::new(facets)).await.unwrap();
    assert_eq!(*greeted.lock().unwrap(), ["activate"]);
    let counter = handles.lock().unwrap().clone().unwrap();

    let result = counter.call("increment", vec![Json::from(2)], &BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(result, Some(Json::from(2)));
    let count = counter.state("count").unwrap();
    assert_eq!(count.value().unwrap(), Some(json!({"value": 2, "greeting": ""})));

    let result = counter.call("greet", vec![Json::from("js")], &BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(result, Some(Json::from("hello js")));
    assert_eq!(count.value().unwrap(), Some(json!({"value": 2, "greeting": "hello js"})));
    assert_eq!(*greeted.lock().unwrap(), ["activate", "js"]);

    for _ in 0..100 {
        if !workers.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(*workers.lock().unwrap(), [Some(Json::from("worker one"))]);

    let missing = counter.call("absent", Vec::new(), &BACKGROUND_CONTEXT).await;
    assert!(missing.is_err());

    host.dispose().await.unwrap();
    let disposed = evaluator.handle().eval("globalThis.__disposed === true").await.unwrap();
    assert_eq!(disposed.into_json(), serde_json::Value::Bool(true));
}
