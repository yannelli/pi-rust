//! The facet loader seam (upstream `loader.ts` and `test/loader.test.ts`): fresh facets per load, named by module,
//! and a redefined module serving its new code on the next load, through [`InProcessLoader`].

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use pi_chord::host2::{FacetLoader, InProcessLoader};
use pi_chord::{ChordError, ChordResult};

/// A stand-in for the host's facet type: its name, the code version it runs and which build it is.
#[derive(Debug, PartialEq)]
struct Facet {
    name: String,
    greeting: &'static str,
    build: u64,
}

fn module(loader: &InProcessLoader<Facet>, name: &str, greeting: &'static str, builds: &Arc<AtomicU64>) {
    let builds = builds.clone();
    loader.define(name, move |module| {
        let build = builds.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(Facet { name: module.to_string(), greeting, build })
    });
}

fn modules(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| name.to_string()).collect()
}

#[tokio::test]
async fn loads_fresh_facets_in_order_named_by_their_modules() {
    let loader = InProcessLoader::new();
    let builds = Arc::new(AtomicU64::new(0));
    module(&loader, "plugins/a", "a", &builds);
    module(&loader, "plugins/b", "b", &builds);
    let loaded = loader.load(&modules(&["plugins/b", "plugins/a"])).await.unwrap();
    let names: Vec<_> = loaded.iter().map(|facet| facet.name.as_str()).collect();
    assert_eq!(names, ["plugins/b", "plugins/a"]);
    let again = loader.load(&modules(&["plugins/a"])).await.unwrap();
    assert_eq!(again[0], Facet { name: "plugins/a".into(), greeting: "a", build: 3 });
    assert_eq!(loader.generation(), 2);
}

#[tokio::test]
async fn a_redefined_module_serves_its_new_code_on_the_next_load() {
    let loader = InProcessLoader::new();
    let builds = Arc::new(AtomicU64::new(0));
    module(&loader, "facet", "v1", &builds);
    let first = loader.load(&modules(&["facet"])).await.unwrap();
    module(&loader, "facet", "v2", &builds);
    let second = loader.load(&modules(&["facet"])).await.unwrap();
    assert_eq!((first[0].greeting, second[0].greeting), ("v1", "v2"));
    assert!(loader.remove("facet"));
    assert!(!loader.remove("facet"));
}

#[tokio::test]
async fn an_unknown_module_fails_the_load_before_anything_is_built() {
    let loader = InProcessLoader::new();
    let builds = Arc::new(AtomicU64::new(0));
    module(&loader, "known", "k", &builds);
    let dynamic: Arc<dyn FacetLoader<Facet>> = Arc::new(loader);
    let Err(error) = dynamic.load(&modules(&["known", "missing"])).await else { panic!("loaded a missing module") };
    assert!(
        matches!(&error, ChordError::Type(message) if message == "missing: no facet module of that name"),
        "{error}"
    );
    assert_eq!(builds.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_module_that_fails_to_build_fails_the_load() {
    let loader: InProcessLoader<Facet> = InProcessLoader::new();
    loader.define("broken", |module| -> ChordResult<Facet> {
        Err(ChordError::type_error(format!("{module}: no facet")))
    });
    let Err(error) = loader.load(&modules(&["broken"])).await else { panic!("built a broken module") };
    assert_eq!(error.to_string(), "broken: no facet");
}
