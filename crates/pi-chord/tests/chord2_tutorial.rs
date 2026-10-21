//! Port of upstream `test/tutorial.test.ts`, sections 1 to 6: hosts, contracts, cells, resources, views by
//! identity and a memory link. Sections 7 to 11 are in `chord2_tutorial_reload.rs`. Upstream also checks that
//! `docs/tutorial.md` prints its TypeScript examples unchanged; that drift check has no Rust counterpart.

mod chord2_support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chord2_support::tutorial::*;
use chord2_support::*;
use pi_chord::host2::declare::{OpenOptions, ResourceDeclaration};
use pi_chord::host2::wire::PeerKind;
use pi_chord::host2::{
    AcceptOptions, ConnectOptions, Ctx, Declare, GetOptions, HostOptions, Implementation, Link, RefStatus, RootOptions,
    ServeOptions, create_host, memory_link_pair, on_value, opaque,
};
use pi_chord::{Json, json};

#[tokio::test]
async fn t01_objects_live_inside_hosts() {
    let _serial = serial().await;
    let host = host_as("server", Some("todo-server"), false);
    assert_eq!(host.name(), "todo-server");
    within(host.close()).await.unwrap();
}

#[tokio::test]
async fn t02_first_contract_factory_reference_and_root() {
    let _serial = serial().await;
    let todo_label = local_type(
        "tutorial.todos",
        object(&[req("list", string())]),
        vec![("label", pi_chord::host2::method(None, string()).into())],
    );
    let host = host_as("server", None, false);
    let factory = |input: &Json, _: &Json, _: &mut Declare| {
        let label = format!("Todos for {}", text(&input["list"]));
        Ok(Implementation::new().method("label", move |_, _| std::future::ready(Ok(json!(label.clone())))))
    };
    host.serve(&todo_label, factory, ServeOptions::default()).unwrap();
    let run = move |ctx: Ctx| async move {
        let todos = ctx.get(&todo_label, family(), GetOptions::default()).await?;
        todos.call_json("label", Json::Null, Some(&ctx)).await
    };
    let result = within(host.root(run, RootOptions::default())).await.unwrap();
    assert_eq!(result, json!("Todos for family"));
    within(host.close()).await.unwrap();
}

#[tokio::test]
async fn t03_cells_publish_values_and_derivations() {
    let _serial = serial().await;
    let host = host_as("server", None, true);
    host.serve(&todo_store(), todo_factory(), ServeOptions::default()).unwrap();
    let todos = within(host.get(&todo_store(), family(), GetOptions::default())).await.unwrap();
    let counts: Log<Json> = Log::default();
    let counted = counts.clone();
    let stop = on_value(&*todos.state("openCount").unwrap(), move |value| counted.push(value.clone()));
    let store = todos.clone();
    let run = move |ctx: Ctx| async move {
        store.call("add", Some(json!({"text": "Buy milk"})), Some(&ctx)).await?;
        store.call("finish", Some(json!({"index": 0})), Some(&ctx)).await?;
        store.call("reset", Some(json!([])), Some(&ctx)).await?;
        Ok(())
    };
    within(host.root(run, RootOptions::default())).await.unwrap();
    assert_eq!(counts.all(), [json!(0), json!(1), json!(0)]);
    assert_eq!(read(&todos, "rows"), json!([]));
    stop.unsubscribe();
    todos.release();
    within(host.close()).await.unwrap();
}

/// The database's native value: the texts it saved.
type Db = Arc<Mutex<Vec<String>>>;

#[tokio::test]
async fn t04_holds_asynchronous_resources_and_release() {
    let _serial = serial().await;
    let database_type =
        local_type("tutorial.database", object(&[]), vec![("db", opaque("tutorial.database@1").unwrap().into())]);
    let saved_todos = local_type(
        "tutorial.saved-todos",
        object(&[req("list", string())]),
        vec![("add", pi_chord::host2::method(object(&[req("text", string())]), None).into())],
    );
    let host = host_as("server", None, false);
    let opens = Arc::new(AtomicUsize::new(0));
    let closed = Latch::default();
    let (opened, closing) = (opens.clone(), closed.clone());
    let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        let opened = opened.clone();
        let open = move |_: OpenOptions| {
            opened.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok(Db::default()))
        };
        let closing = closing.clone();
        let close = move |_: Db| {
            closing.open(Json::Null);
            std::future::ready(Ok(()))
        };
        let db = decl.resource("db", ResourceDeclaration::new(1, open).with_close(close))?;
        Ok(Implementation::new().member("db", decl.expose(&db)?))
    };
    host.serve(&database_type, factory, ServeOptions::default()).unwrap();

    host.serve(&todo_store(), todo_factory(), ServeOptions::default()).unwrap();
    let held = database_type.clone();
    let factory = move |input: &Json, _: &Json, decl: &mut Declare| {
        let database = decl.get(&held, json!({}), GetOptions::default())?;
        let todos = decl.get(&todo_store(), json!({"list": input["list"].clone()}), GetOptions::default())?;
        Ok(Implementation::new().method("add", move |args, ctx| {
            let (database, todos) = (database.clone(), todos.clone());
            async move {
                let db = database.opaque_as::<Db>("db")?.expect("the database is open");
                db.lock().unwrap().push(text(&args["text"]).to_string());
                todos.call("add", Some(args), Some(&ctx)).await?;
                Ok(Json::Null)
            }
        }))
    };
    host.serve(&saved_todos, factory, ServeOptions::linger(Duration::ZERO)).unwrap();

    let saved = within(host.get(&saved_todos, family(), GetOptions::default())).await.unwrap();
    let database = within(host.get(&database_type, json!({}), GetOptions::default())).await.unwrap();
    let adding = saved.clone();
    let run = move |ctx: Ctx| async move { adding.call("add", Some(json!({"text": "Buy milk"})), Some(&ctx)).await };
    within(host.root(run, RootOptions::default())).await.unwrap();
    assert_eq!(*database.opaque_as::<Db>("db").unwrap().unwrap().lock().unwrap(), ["Buy milk"]);
    assert_eq!(opens.load(Ordering::SeqCst), 1);
    let statuses: Log = Log::default();
    let seen = statuses.clone();
    let _statuses = saved.on_status(Arc::new(move |status, _| seen.push(status.as_str())));
    database.release();
    saved.release();
    within(closed.wait()).await;
    assert_eq!(saved.status(), RefStatus::Released);
    assert!(statuses.all().iter().any(|status| status == "released"), "{:?}", statuses.all());
    within(host.close()).await.unwrap();
}

#[tokio::test]
async fn t05_principals_select_views_of_one_shared_store() {
    let _serial = serial().await;
    let principal_key = Arc::new(|identity: &Json| identity.as_str().unwrap_or_default().to_string());
    let host = create_host(HostOptions {
        name: Some("server".into()),
        principal: json!("server"),
        principal_key: Some(principal_key),
        ..HostOptions::default()
    });
    host.serve(&todo_store(), todo_factory(), ServeOptions::default()).unwrap();
    serve_views(&host);
    let owner = within(host.get(&todos_view(), family(), GetOptions::principal(json!("owner")))).await.unwrap();
    let guest = within(host.get(&todos_view(), family(), GetOptions::principal(json!("guest")))).await.unwrap();
    assert_eq!(owner.variant().as_deref(), Some("editor"));
    assert_eq!(guest.variant().as_deref(), Some("viewer"));
    let adding = owner.clone();
    let run = move |ctx: Ctx| async move { adding.call("add", Some(json!({"text": "Buy milk"})), Some(&ctx)).await };
    let options = RootOptions { principal: Some(json!("owner")), ..RootOptions::default() };
    within(host.root(run, options)).await.unwrap();
    assert_eq!(read(&guest, "rows"), json!([{"text": "Buy milk", "done": false}]));
    // A viewer has no add: upstream's TypeScript refuses the call, and so does the provider.
    let error = within(call(&guest, "add", json!({"text": "Not allowed"}))).await.unwrap_err();
    assert!(is(&error, "not_in_variant"), "{error}");
    let error = within(call(&owner, "add", json!({"text": " "}))).await.unwrap_err();
    assert!(is(&error, "invalid"), "{error}");
    within(host.close()).await.unwrap();
}

#[tokio::test]
async fn t06_remote_methods_and_live_state_over_a_memory_link() {
    let _serial = serial().await;
    let server = host_as("server", Some("server"), false);
    server.serve(&todo_store(), todo_factory(), ServeOptions::default()).unwrap();
    serve_views(&server);
    let (down, up) = memory_link_pair();
    server.accept(down, AcceptOptions::client(json!("owner"))).unwrap();
    let upstream: Option<(Arc<dyn Link>, ConnectOptions)> = Some((up, ConnectOptions::new(json!("server"))));
    let browser = create_host(HostOptions {
        name: Some("browser".into()),
        principal: json!("browser"),
        kind: PeerKind::Client,
        upstream,
        ..HostOptions::default()
    });
    let todos = within(browser.get(&todos_view(), family(), GetOptions::default())).await.unwrap();
    assert_eq!(todos.variant().as_deref(), Some("editor"));
    assert_eq!(read(&todos, "rows"), json!([]));
    let updated = Latch::default();
    let opened = updated.clone();
    let stop = on_value(&*todos.state("rows").unwrap(), move |rows| {
        if rows.as_array().is_some_and(|rows| rows.len() == 1) {
            opened.open(Json::Null);
        }
    });
    let adding = todos.clone();
    let run = move |ctx: Ctx| async move { adding.call("add", Some(json!({"text": "Buy milk"})), Some(&ctx)).await };
    within(browser.root(run, RootOptions::default())).await.unwrap();
    within(updated.wait()).await;
    assert_eq!(read(&todos, "rows"), json!([{"text": "Buy milk", "done": false}]));
    // A malformed caller: upstream bypasses TypeScript, Rust passes any JSON.
    let error = within(call(&todos, "add", json!({"text": 123}))).await.unwrap_err();
    assert!(is(&error, "invalid"), "{error}");
    stop.unsubscribe();
    // A held reference keeps its state without listeners.
    assert_eq!(read(&todos, "rows"), json!([{"text": "Buy milk", "done": false}]));
    todos.release();
    within(browser.close()).await.unwrap();
    within(server.close()).await.unwrap();
}
