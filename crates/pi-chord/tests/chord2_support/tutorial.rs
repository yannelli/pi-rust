//! The shared todo list of the tutorial tests (upstream `test/tutorial.test.ts`): its contracts, the store's
//! factory, the views that depend on who asks, and hosts identified by plain strings.

use std::future::{Future, ready};
use std::time::Duration;

use pi_chord::host2::{
    Contract, Declare, Host, HostOptions, Implementation, Initial, LocalDef, Member, ReadableRef, Ref, RemoteDef,
    ServeOptions, StateOptions, VariantDef, create_host, forbidden, host_named, invalid, local, method, remote,
};
use pi_chord::{ChordError, ChordResult, Json, json};

use super::{array, boolean, integer, number, object, req, string, text};

/// Upstream's per-test timeout: a step that waits longer fails instead of hanging.
pub async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), future).await.expect("the step timed out")
}

/// A host whose identity is a plain string (upstream `createHost({ self, name })`).
pub fn host_as(identity: &str, name: Option<&str>, dev: bool) -> Host {
    let principal = json!(identity);
    create_host(HostOptions { name: name.map(str::to_string), principal, dev, ..HostOptions::default() })
}

pub fn family() -> Json {
    json!({"list": "family"})
}

fn list_input() -> Json {
    object(&[req("list", string())])
}

fn rows() -> Json {
    array(object(&[req("text", string()), req("done", boolean())]))
}

fn text_args() -> Json {
    object(&[req("text", string())])
}

fn store_members() -> Vec<(String, Member)> {
    let members: Vec<(&str, Member)> = vec![
        ("label", method(None, string()).into()),
        ("rows", rows().into()),
        ("openCount", number().into()),
        ("add", method(text_args(), None).into()),
        ("finish", method(object(&[req("index", integer())]), None).into()),
        ("reset", method(rows(), None).into()),
    ];
    members.into_iter().map(|(name, member)| (name.to_string(), member)).collect()
}

/// `TodoStore`: the list's rows, its open count and the methods that change them.
pub fn todo_store() -> Contract {
    local("tutorial.todos", &LocalDef { version: 1, input: list_input(), members: store_members() }).unwrap()
}

/// `RenamedStore` (section 9): version 2 calls `label` `heading`.
pub fn renamed_store() -> Contract {
    let mut members: Vec<_> = store_members().into_iter().filter(|(name, _)| name != "label").collect();
    members.push(("heading".into(), method(None, string()).into()));
    local("tutorial.todos", &LocalDef { version: 2, input: list_input(), members }).unwrap()
}

fn void(result: Result<(), ChordError>) -> std::future::Ready<ChordResult<Json>> {
    ready(result.map(|()| Json::Null))
}

fn open_count(values: &[Json]) -> ChordResult<Json> {
    let todos = values[0].as_array().map_or(&[][..], Vec::as_slice);
    Ok(json!(todos.iter().filter(|todo| todo["done"] != json!(true)).count()))
}

/// The store's factory (upstream `todoStore(options)`); `options` versions its rows (section 9).
pub fn store_factory(
    options: StateOptions,
) -> impl Fn(&Json, &Json, &mut Declare) -> ChordResult<Implementation> + Clone + Send + Sync + 'static {
    move |input, _, decl| {
        let label = format!("Todos for {}", text(&input["list"]));
        let rows = decl.state_with("rows", rows(), Initial::Value(json!([])), options.clone())?;
        let open = decl.derive(vec![rows.clone() as ReadableRef], open_count);
        let (added, finished, reset) = (rows.clone(), rows.clone(), rows.clone());
        Ok(Implementation::new()
            .method("label", move |_, _| ready(Ok(json!(label.clone()))))
            .state("rows", rows)
            .state("openCount", open)
            .method("add", move |args, ctx| {
                void(added.change(&ctx, |draft| draft.push(json!({"text": args["text"].clone(), "done": false}))))
            })
            .method("finish", move |args, ctx| {
                let index = args["index"].as_u64().map_or(usize::MAX, |index| index as usize);
                void(finished.change(&ctx, |draft| match draft.at_mut(index) {
                    Some(todo) => todo.set("done", true),
                    None => Err(invalid("args", "No such todo", None)),
                }))
            })
            .method("reset", move |next, ctx| void(reset.replace_with(&ctx, next, Some(true)))))
    }
}

/// Upstream `todoFactory`: the store with unversioned rows.
pub fn todo_factory() -> impl Fn(&Json, &Json, &mut Declare) -> ChordResult<Implementation> + Clone + Send + Sync {
    store_factory(StateOptions::default())
}

/// `Todos`: the server's views of a list, `viewer` or `editor` by who asks.
pub fn todos_view() -> Contract {
    let viewer = VariantDef::new().state("rows", rows());
    let editor = VariantDef::new().state("rows", rows()).method("add", method(text_args(), None));
    let variants = vec![("viewer".into(), viewer), ("editor".into(), editor)];
    let def = RemoteDef { version: 1, input: list_input(), host: host_named("server").unwrap(), variants };
    remote("tutorial.todo-view", &def).unwrap()
}

/// Upstream `serveViews`: the owner and the server edit the shared store, a guest only reads it.
pub fn serve_views(host: &Host) {
    let factory = |input: &Json, asker: &Json, decl: &mut Declare| {
        if input["list"] != json!("family") {
            return Err(forbidden(Some("This list is not shared")));
        }
        let store = decl.get(&todo_store(), json!({"list": input["list"].clone()}), Default::default())?;
        let shared = store.state("rows").expect("the store has rows");
        if *asker == json!("guest") {
            return Ok(Implementation::variant("viewer").state("rows", shared));
        }
        Ok(Implementation::variant("editor").state("rows", shared).method("add", move |args, ctx| {
            let store = store.clone();
            async move {
                // Like upstream's `text.trim()`, this fails on a non-string: the contract checks the arguments first.
                let Some(text) = args["text"].as_str() else {
                    return Err(ChordError::type_error("text is not a string"));
                };
                if text.trim().is_empty() {
                    return Err(invalid("args", "A todo needs text", None));
                }
                let args = json!({"text": text});
                Ok(store.call("add", Some(args), Some(&ctx)).await?.unwrap_or_default())
            }
        }))
    };
    host.serve(&todos_view(), factory, ServeOptions::default()).unwrap();
}

/// Calls `member` with `args` in a fresh root; `null` for a void result.
pub async fn call(reference: &Ref, member: &str, args: Json) -> ChordResult<Json> {
    Ok(reference.call(member, Some(args), None).await?.unwrap_or_default())
}

/// A state member's current value.
pub fn read(reference: &Ref, member: &str) -> Json {
    reference.state(member).expect("a state member").value().unwrap()
}
