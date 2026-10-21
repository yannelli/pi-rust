//! Port of upstream `test/tutorial.test.ts`, sections 7 to 11: listeners and tasks, facets with registries,
//! reloads, a fake provider, and short roots. Sections 1 to 6 are in `chord2_tutorial.rs`. Like upstream, the
//! tests pass facets built in memory to `load` and `reload`, so no `FacetLoader` takes part.

mod chord2_support;

use std::collections::HashMap;
use std::future::ready;
use std::sync::{Arc, Mutex};

use chord2_support::reload::spawn_root;
use chord2_support::tutorial::*;
use chord2_support::*;
use pi_chord::host2::object::Adapter;
use pi_chord::host2::{
    Ctx, Declare, Facet, FacetApi, GetOptions, HostOptions, Implementation, LoadOptions, RegistryView, ReloadOptions,
    RootOptions, ServeOptions, StateOptions, create_host, define_facet, define_registry, on_event,
};
use pi_chord::{BoxFuture, ChordResult, Json, json};

#[tokio::test]
async fn t07_listeners_react_and_tasks_start_fresh_calls() {
    let _serial = serial().await;
    let activity = local_type("tutorial.activity", object(&[]), vec![("seen", number().into())]);
    let host = host_as("server", None, false);
    host.serve(&todo_store(), todo_factory(), ServeOptions::default()).unwrap();
    let (observed, seeded, stopped) = (Latch::default(), Latch::default(), Latch::default());
    let latches = (observed.clone(), seeded.clone(), stopped.clone());
    let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        let (observed, seeded, stopped) = latches.clone();
        let todos = decl.get(&todo_store(), family(), GetOptions::default())?;
        let seen = decl.state("seen", number(), json!(0))?;
        let counter = seen.clone();
        decl.on("rows", todos.state("rows").unwrap(), move |rows, ctx| {
            let length = rows.as_array().map_or(0, Vec::len);
            let result = counter.replace(&ctx, json!(length));
            if length == 1 {
                observed.open(Json::Null);
            }
            ready(result)
        })?;
        decl.task(move |ctx| {
            let (todos, seeded, stopped) = (todos.clone(), seeded.clone(), stopped.clone());
            async move {
                todos.call("add", Some(json!({"text": "Review the list"})), Some(&ctx)).await?;
                seeded.open(Json::Null);
                ctx.signal().cancelled().await;
                stopped.open(Json::Null);
                Ok(())
            }
        });
        Ok(Implementation::new().state("seen", seen))
    };
    host.serve(&activity, factory, ServeOptions::default()).unwrap();
    let activity = within(host.get(&activity, json!({}), GetOptions::default())).await.unwrap();
    within(async { futures::join!(seeded.wait(), observed.wait()) }).await;
    assert_eq!(read(&activity, "seen"), json!(1));
    activity.release();
    within(stopped.wait()).await;
    within(host.close()).await.unwrap();
}

/// A registered menu action: it runs in the caller's root and returns the open count.
type Action = Arc<dyn Fn(Ctx) -> BoxFuture<ChordResult<Json>> + Send + Sync>;

#[tokio::test]
async fn t08_plugins_contribute_registry_entries_and_expose_callbacks() {
    let _serial = serial().await;
    let actions = define_registry::<Action>("tutorial.actions", None);
    let menu = local_type("tutorial.menu", object(&[]), vec![]);
    // Upstream keeps a `click` closure over the saved `api`; the test keeps the `api` and calls through it.
    let saved: Arc<Mutex<Option<FacetApi>>> = Arc::default();
    let store_facet = define_facet("todo-store", |api| api.serve(&todo_store(), todo_factory(), Default::default()));
    let (registry, keep) = (actions.clone(), saved.clone());
    let menu_facet = define_facet("todo-menu", move |api| {
        let registry = registry.clone();
        let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
            let todos = decl.get(&todo_store(), family(), GetOptions::default())?;
            let add_sample: Action = Arc::new(move |ctx| {
                let todos = todos.clone();
                Box::pin(async move {
                    todos.call("add", Some(json!({"text": "Try plugins"})), Some(&ctx)).await?;
                    Ok(read(&todos, "openCount"))
                })
            });
            decl.register(&registry, [("addSample".to_string(), add_sample)])?;
            Ok(Implementation::new())
        };
        api.serve(&menu, factory, ServeOptions::host())?;
        *keep.lock().unwrap() = Some(api.clone());
        Ok(())
    })
    .with_uses(vec![todo_store()]);
    let host = host_as("server", None, false);
    let options = LoadOptions { await_host_objects: false };
    within(host.load(&[menu_facet, store_facet], options)).await.unwrap();
    within(host.ready()).await.unwrap();
    let api = saved.lock().unwrap().clone().expect("the menu facet ran");
    let (lookup, table) = (api.clone(), actions.clone());
    let click = move |ctx: Ctx| async move {
        let action = lookup.registry(&table).get("addSample", Some(ctx.code_version()))?.expect("registered");
        action(ctx).await
    };
    assert_eq!(within(api.root(click, RootOptions::default())).await.unwrap(), json!(1));
    assert_eq!(host.registry(&actions).table(None).unwrap().len(), 1);
    within(host.close()).await.unwrap();
}

/// Upstream `facet(label)`: the store, with a fixed label.
fn labelled(label: &'static str) -> Facet {
    define_facet("todos", move |api| {
        let factory = move |input: &Json, asker: &Json, decl: &mut Declare| {
            let store = todo_factory()(input, asker, decl)?;
            Ok(store.method("label", move |_, _| ready(Ok(json!(label)))))
        };
        api.serve(&todo_store(), factory, ServeOptions::default())
    })
}

/// The store with its rows at version 2; `migrate` upper-cases the texts.
fn migrated() -> Facet {
    let migrate = Arc::new(|old: Json, from: i64| {
        assert_eq!(from, 1);
        let upper = |todo: &Json| json!({"text": text(&todo["text"]).to_uppercase(), "done": todo["done"].clone()});
        Ok(Json::array(old.as_array().unwrap().iter().map(upper).collect()))
    });
    let options = StateOptions { version: Some(2), migrate: Some(migrate) };
    define_facet("todos", move |api| api.serve(&todo_store(), store_factory(options.clone()), Default::default()))
}

/// Serves `RenamedStore`, with an adapter that gives version 1 callers `label` back.
fn renamed() -> Facet {
    define_facet("todos", |api| {
        let factory = |input: &Json, asker: &Json, decl: &mut Declare| {
            let options = StateOptions { version: Some(2), migrate: None };
            let store = store_factory(options)(input, asker, decl)?;
            let mut renamed = Implementation::new();
            for (name, member) in store.members() {
                renamed = renamed.member(if name == "label" { "heading" } else { name }, member.clone());
            }
            Ok(renamed)
        };
        let adapter: Adapter = Arc::new(|current: &Implementation| {
            current.clone().member("label", current.get("heading").expect("a heading").clone())
        });
        let options = ServeOptions { accepts: Some(HashMap::from([(1, adapter)])), ..ServeOptions::default() };
        api.serve(&renamed_store(), factory, options)
    })
}

#[tokio::test]
async fn t09_compatible_reload_migration_and_an_old_contract_adapter() {
    let _serial = serial().await;
    let host = host_as("server", None, false);
    within(host.load(&[labelled("Original")], LoadOptions::default())).await.unwrap();
    let todos = within(host.get(&todo_store(), family(), GetOptions::default())).await.unwrap();
    within(call(&todos, "add", json!({"text": "Buy milk"}))).await.unwrap();
    let (entered, resume) = (Latch::default(), Latch::default());
    let old_root = spawn_root(&host, {
        let (todos, entered, resume) = (todos.clone(), entered.clone(), resume.clone());
        move |ctx| async move {
            let first = todos.call_json("label", Json::Null, Some(&ctx)).await?;
            entered.open(Json::Null);
            resume.wait().await; // Test-only pause: pin an old caller during reload.
            Ok(json!([first, todos.call_json("label", Json::Null, Some(&ctx)).await?]))
        }
    });
    within(entered.wait()).await;
    let compatible = within(host.reload(vec![labelled("Updated").into()], ReloadOptions::default())).await.unwrap();
    assert!(compatible.gated.is_empty(), "{:?}", compatible.gated);
    assert_eq!(within(call(&todos, "label", Json::Null)).await.unwrap(), json!("Updated"));
    resume.open(Json::Null);
    assert_eq!(within(old_root).await.unwrap().unwrap(), json!(["Original", "Original"]));
    assert_eq!(read(&todos, "rows")[0]["text"], json!("Buy milk"));

    let events: Log = Log::default();
    let seen = events.clone();
    let stop_events = on_event(&*todos.state("rows").unwrap(), move |event| seen.push(event.kind()));
    let report = within(host.reload(vec![migrated().into()], ReloadOptions::default())).await.unwrap();
    assert_eq!(report.gated.len(), 1);
    assert_eq!(read(&todos, "rows"), json!([{"text": "BUY MILK", "done": false}]));
    assert_eq!(events.all(), ["snap", "reset"]);
    stop_events.unsubscribe();

    within(host.reload(vec![renamed().into()], ReloadOptions::default())).await.unwrap();
    assert_eq!(within(call(&todos, "label", Json::Null)).await.unwrap(), json!("Todos for family"));
    let newer = within(host.get(&renamed_store(), family(), GetOptions::default())).await.unwrap();
    assert_eq!(within(call(&newer, "heading", Json::Null)).await.unwrap(), json!("Todos for family"));
    assert_eq!(read(&todos, "rows")[0]["text"], json!("BUY MILK"));
    within(host.close()).await.unwrap();
}

#[tokio::test]
async fn t10_stub_a_provider_with_a_fake_facet_and_check_error_codes() {
    let _serial = serial().await;
    let host = create_host(HostOptions {
        name: Some("server".into()),
        principal: json!("server"),
        dev: true,
        ..HostOptions::default()
    });
    let received: Log = Log::default();
    let recorded = received.clone();
    let fake = define_facet("fake-store", move |api| {
        let recorded = recorded.clone();
        let factory = move |input: &Json, asker: &Json, decl: &mut Declare| {
            let recorded = recorded.clone();
            let store = todo_factory()(input, asker, decl)?;
            Ok(store.method("add", move |args, _| {
                recorded.push(text(&args["text"]));
                ready(Ok(Json::Null))
            }))
        };
        api.serve(&todo_store(), factory, ServeOptions::default())
    });
    within(host.load(&[fake], LoadOptions::default())).await.unwrap();
    serve_views(&host);
    let owner = within(host.get(&todos_view(), family(), GetOptions::principal(json!("owner")))).await.unwrap();
    assert_eq!(owner.variant().as_deref(), Some("editor"));
    within(call(&owner, "add", json!({"text": "Test delegation"}))).await.unwrap();
    assert_eq!(received.all(), ["Test delegation"]);
    let error = within(call(&owner, "add", json!({"text": ""}))).await.unwrap_err();
    assert!(is(&error, "invalid"), "{error}");
    owner.release();
    let error = within(call(&owner, "add", json!({"text": "Too late"}))).await.unwrap_err();
    assert!(is(&error, "released"), "{error}");
    within(host.close()).await.unwrap();
}

#[tokio::test]
async fn t11_short_roots_release_temporary_holds() {
    let _serial = serial().await;
    let host = host_as("server", None, false);
    host.serve(&todo_store(), todo_factory(), ServeOptions::default()).unwrap();
    let screen = within(host.get(&todo_store(), family(), GetOptions::default())).await.unwrap();
    let run = |ctx: Ctx| async move {
        let temporary = ctx.get(&todo_store(), family(), GetOptions::default()).await?;
        temporary.call("add", Some(json!({"text": "Keep roots short"})), Some(&ctx)).await?;
        Ok(())
    };
    // The temporary reference is released when the root settles; screen still holds the store.
    within(host.root(run, RootOptions::default())).await.unwrap();
    assert_eq!(read(&screen, "openCount"), json!(1));
    screen.release(); // No reference remains; without persistence, disposal loses these rows.
    within(host.close()).await.unwrap();
}
