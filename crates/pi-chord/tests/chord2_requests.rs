//! Port of upstream `test/requests.test.ts`, in-host cases: #1 and #2 up to their link and reload halves, #4,
//! #6, and #10 with the server's type served on the same host (closing links is phase 3). Left out: #3 and #7
//! (reloads), #5, #8 and #9 (links and clusters).

mod chord2_support;

use std::collections::HashSet;
use std::future::ready;
use std::sync::{Arc, Mutex};

use chord2_support::*;
use pi_chord::delta::apply_immutable;
use pi_chord::host2::declare::{OpenOptions, ResourceDeclaration};
use pi_chord::host2::{
    Contract, Ctx, Declare, FacetApi, GetOptions, Host, HostOptions, Implementation, LoadOptions, Ref, RegistryView,
    RemoteDef, ResourceHandle, RootOptions, ServeOptions, StateEvent, VariantDef, create_host, define_facet,
    define_registry, host_named, method, on_event, on_value, opaque, remote,
};
use pi_chord::{Json, json};

fn named(name: &str) -> Host {
    create_host(HostOptions {
        name: Some(name.into()),
        principal: json!({"kind": "host"}),
        on_error: Some(Arc::new(|_| {})),
        ..HostOptions::default()
    })
}

fn hold(host: &Host, contract: &Contract) -> Ref {
    host.hold(contract, json!({}), GetOptions::default()).unwrap()
}

fn view() -> Json {
    object(&[req("text", string())])
}

fn on_server(name: &str, variant: &str, members: VariantDef) -> Contract {
    let variants = vec![(variant.to_string(), members)];
    let def = RemoteDef { version: 1, input: object(&[]), host: host_named("server").unwrap(), variants };
    remote(name, &def).unwrap()
}

#[tokio::test]
async fn r1_a_held_object_state_member_can_be_returned_as_this_object_state_member() {
    let _serial = serial().await;
    let conv =
        on_server("t.conv", "main", VariantDef::new().state("view", view()).method("set", method(string(), None)));
    let chat = on_server("t.chat", "viewer", VariantDef::new().state("view", view()));
    let server = named("server");
    let conv_factory = |_: &Json, _: &Json, decl: &mut Declare| {
        let view = decl.state("view", view(), json!({"text": ""}))?;
        Ok(Implementation::variant("main")
            .state("view", view.clone())
            .method("set", move |text, ctx| ready(view.replace(&ctx, json!({"text": text})).map(|()| Json::Null))))
    };
    server.serve(&conv, conv_factory, ServeOptions::default()).unwrap();
    // No derivation: the reference member is the implementation member.
    let held = conv.clone();
    let chat_factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        let conv = decl.get(&held, json!({}), GetOptions::default())?;
        Ok(Implementation::variant("viewer").state("view", conv.state("view").unwrap()))
    };
    server.serve(&chat, chat_factory, ServeOptions::default()).unwrap();
    let chat_ref = hold(&server, &chat);
    let values: Log<Json> = Log::default();
    let valued = values.clone();
    let _values = on_value(&*chat_ref.state("view").unwrap(), move |value| valued.push(value.clone()));
    chat_ref.ready().await.unwrap();
    hold(&server, &conv).call("set", Some(json!("hi")), None).await.unwrap();
    tick().await;
    assert_eq!(chat_ref.state("view").unwrap().value().unwrap(), json!({"text": "hi"}));
    assert_eq!(values.all().last(), Some(&json!({"text": "hi"})));
}

#[tokio::test]
async fn r2_facets_reach_their_own_host_through_facet_api_root_and_registry() {
    let _serial = serial().await;
    let config = define_registry::<String>("t.config", None);
    let settings = local_type("t.settings", object(&[]), vec![]);
    let reader = local_type("t.reader", object(&[]), vec![("read", method(None, string()).into())]);
    let captured: Arc<Mutex<Vec<FacetApi>>> = Arc::default();
    let (kept, served_reader) = (captured.clone(), reader.clone());
    let facet = define_facet("config", move |api| {
        kept.lock().unwrap().push(api.clone());
        let registered = config.clone();
        let register = move |_: &Json, _: &Json, decl: &mut Declare| {
            decl.register(&registered, [("greeting".to_string(), "v1".to_string())])?;
            Ok(Implementation::new())
        };
        api.serve(&settings, register, ServeOptions::host())?;
        let (reading, read) = (api.clone(), config.clone());
        let factory = move |_: &Json, _: &Json, _: &mut Declare| {
            let (api, config) = (reading.clone(), read.clone());
            Ok(Implementation::new().method("read", move |_, ctx| {
                let greeting = api.registry(&config).get("greeting", Some(ctx.code_version()));
                ready(greeting.map(|greeting| json!(greeting.unwrap_or_default())))
            }))
        };
        api.serve(&served_reader, factory, ServeOptions::default())
    });
    let host = named("worker");
    host.install(&facet, LoadOptions::default()).await.unwrap();
    sleep_ms(5).await;
    let api = captured.lock().unwrap()[0].clone();
    assert_eq!(api.name(), "worker");
    // A callback outside Chord (a Durable tool, say) starts its own root.
    let run = move |ctx: Ctx| async move {
        let reader = ctx.get(&reader, json!({}), GetOptions::default()).await?;
        reader.call_json("read", Json::Null, Some(&ctx)).await
    };
    assert_eq!(api.root(run, RootOptions::default()).await.unwrap(), json!("v1"));
}

/// `{entries, status}` built without copying `entries` (a new revision that shares structure).
fn doc(entries: Json, status: &str) -> Json {
    let mut value = Json::empty_object();
    value.set("entries", entries).unwrap();
    value.set("status", status).unwrap();
    value
}

#[tokio::test]
async fn r4_replace_diffs_a_new_revision_that_shares_structure_and_unrelated_values_replace() {
    let _serial = serial().await;
    let mirror =
        local_type("t.mirror", object(&[]), vec![("doc", unknown().into()), ("set", method(unknown(), None).into())]);
    let schema = object(&[req("entries", array(string())), req("status", string())]);
    let diff: Arc<Mutex<Option<bool>>> = Arc::default();
    let host = named("host");
    let forced = diff.clone();
    let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        let doc = decl.state("doc", schema.clone(), json!({"entries": ["a", "b"], "status": "idle"}))?;
        let forced = forced.clone();
        Ok(Implementation::new().state("doc", doc.clone()).method("set", move |value, ctx| {
            let diff = *forced.lock().unwrap();
            ready(doc.replace_with(&ctx, value, diff).map(|()| Json::Null))
        }))
    };
    host.serve(&mirror, factory, ServeOptions::default()).unwrap();
    let reference = hold(&host, &mirror);
    reference.ready().await.unwrap();
    let events: Log<StateEvent> = Log::default();
    let replica: Arc<Mutex<Option<Json>>> = Arc::default();
    let (recorded, replayed) = (events.clone(), replica.clone());
    let doc_member = reference.state("doc").unwrap();
    let _events = on_event(&*doc_member, move |event| {
        let mut replica = replayed.lock().unwrap();
        *replica = Some(match event {
            StateEvent::Ops { ops, .. } => apply_immutable(replica.as_ref(), ops).unwrap(),
            other => other.value().clone(),
        });
        recorded.push(event.clone());
    });
    let last_ops = || match events.all().last() {
        Some(StateEvent::Ops { ops, .. }) => ops.iter().map(|op| op.to_json()).collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    let verb = || last_ops().first().map(|op| op[0].clone());
    let set = |value: Json| reference.call("set", Some(value), None);
    let current = doc_member.value().unwrap();
    // A new revision reusing `entries`: a diff.
    set(doc(current["entries"].clone(), "running")).await.unwrap();
    assert_eq!(last_ops(), [json!(["s", ["status"], "running"])]);
    // Unrelated containers: one replacement op.
    set(json!({"entries": ["a", "b", "c"], "status": "running"})).await.unwrap();
    assert_eq!(verb(), Some(json!("r")));
    // Forced either way.
    *diff.lock().unwrap() = Some(true);
    set(json!({"entries": ["a", "b", "c", "d"], "status": "running"})).await.unwrap();
    assert_ne!(verb(), Some(json!("r")));
    *diff.lock().unwrap() = Some(false);
    let shared = doc_member.value().unwrap();
    set(doc(shared["entries"].clone(), "done")).await.unwrap();
    assert_eq!(verb(), Some(json!("r")));
    tick().await;
    assert_eq!(replica.lock().unwrap().clone(), Some(doc_member.value().unwrap()));
}

type Native = Arc<Mutex<Vec<String>>>;

#[tokio::test]
async fn r6_expose_makes_an_opaque_member_backed_by_a_resource_readable_once_live() {
    let _serial = serial().await;
    let store = local_type("t.store", object(&[]), vec![("db", opaque("t.db@1").unwrap().into())]);
    let gate = Latch::default();
    let native: Native = Arc::default();
    let host = named("host");
    let (opened, value) = (gate.clone(), native.clone());
    let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        let (opened, value) = (opened.clone(), value.clone());
        let open = move |_: OpenOptions| {
            let (opened, value) = (opened.wait(), value.clone());
            async move {
                opened.await;
                Ok(value)
            }
        };
        let db = decl.resource("db", ResourceDeclaration::new(1, open))?;
        Ok(Implementation::new().member("db", decl.expose(&db)?))
    };
    host.serve(&store, factory, ServeOptions::default()).unwrap();
    let reference = hold(&host, &store);
    assert!(reference.opaque_as::<Native>("db").unwrap().is_none());
    gate.open(Json::Null);
    reference.ready().await.unwrap();
    let db = reference.opaque_as::<Native>("db").unwrap().unwrap();
    assert!(Arc::ptr_eq(&db, &native));

    // Upstream passes a lookalike object; here, a resource another object declared.
    let (owner, other) = (local_type("t.owner", object(&[]), vec![]), local_type("t.other", object(&[]), vec![]));
    let stolen: Arc<Mutex<Option<ResourceHandle<i64>>>> = Arc::default();
    let keep = stolen.clone();
    let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        *keep.lock().unwrap() = Some(decl.resource("x", ResourceDeclaration::value(1, 1))?);
        Ok(Implementation::new())
    };
    host.serve(&owner, factory, ServeOptions::default()).unwrap();
    hold(&host, &owner).ready().await.unwrap();
    let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        let foreign = stolen.lock().unwrap().clone().unwrap();
        decl.expose(&foreign)?;
        Ok(Implementation::new())
    };
    host.serve(&other, factory, ServeOptions::default()).unwrap();
    let error = hold(&host, &other).ready().await.unwrap_err();
    assert!(error.to_string().contains("expose takes a resource declared by this object"), "{error}");
}

type Tracker = Arc<Mutex<HashSet<String>>>;

#[tokio::test]
async fn r10_host_close_disposes_holders_before_what_they_hold() {
    let _serial = serial().await;
    let log: Log = Log::default();
    let errors = log.clone();
    let worker = create_host(HostOptions {
        name: Some("w".into()),
        principal: json!({"kind": "host"}),
        on_error: Some(Arc::new(move |error| errors.push(format!("error: {error}")))),
        ..HostOptions::default()
    });
    let note = local_type("t.note10", object(&[]), vec![("note", method(string(), None).into())]);
    let noted = log.clone();
    let note_factory = move |_: &Json, _: &Json, _: &mut Declare| {
        let noted = noted.clone();
        Ok(Implementation::new().method("note", move |text, _| {
            noted.push(format!("note: {}", text.as_str().unwrap_or_default()));
            ready(Ok(Json::Null))
        }))
    };
    worker.serve(&note, note_factory, ServeOptions::default()).unwrap();
    let proc = local_type("t.proc", object(&[]), vec![("tracker", opaque("t.tracker@1").unwrap().into())]);
    let closed = log.clone();
    let proc_factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        let closed = closed.clone();
        let declaration = ResourceDeclaration::new(1, |_| ready(Ok(Tracker::default()))).with_close(move |_| {
            closed.push("proc closed");
            ready(Ok(()))
        });
        let tracker = decl.resource("tracker", declaration)?;
        Ok(Implementation::new().member("tracker", decl.expose(&tracker)?))
    };
    // Eager: activated before the object that holds it.
    worker.serve(&proc, proc_factory, ServeOptions::host()).unwrap();
    sleep_ms(5).await;
    let chat = local_type("t.chat10", object(&[]), vec![]);
    let (held_proc, held_note, logged) = (proc.clone(), note.clone(), log.clone());
    let chat_factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        let proc = decl.get(&held_proc, json!({}), GetOptions::default())?;
        let upstream = decl.get(&held_note, json!({}), GetOptions::default())?;
        let (opening, logged) = (proc.clone(), logged.clone());
        let open = move |_: OpenOptions| {
            let tracker = opening.opaque_as::<Tracker>("tracker").ok().flatten();
            ready(Ok(tracker.map(|tracker| tracker.lock().unwrap().insert("chat".into())).is_some()))
        };
        let close = move |_: bool| {
            let tracker = proc.opaque_as::<Tracker>("tracker").ok().flatten();
            let state = if tracker.is_some() { "ok" } else { "gone" };
            if let Some(tracker) = &tracker {
                tracker.lock().unwrap().remove("chat");
            }
            logged.push(format!("chat closed, proc {}, tracker {state}", proc.status().as_str()));
            let call = upstream.call("note", Some(json!("bye")), None);
            async move { call.await.map(drop) }
        };
        decl.resource("session", ResourceDeclaration::new(1, open).with_close(close))?;
        Ok(Implementation::new())
    };
    worker.serve(&chat, chat_factory, ServeOptions::default()).unwrap();
    worker.get(&chat, json!({}), GetOptions::default()).await.unwrap();
    worker.close().await.unwrap();
    assert_eq!(log.all(), ["chat closed, proc live, tracker ok", "note: bye", "proc closed"]);
}
