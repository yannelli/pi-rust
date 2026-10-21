//! Port of upstream `test/state.test.ts`: state cells, derivations, listeners and tasks of one host.

mod chord2_support;

use std::collections::HashSet;
use std::future::ready;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use chord2_support::*;
use pi_chord::delta::apply_immutable;
use pi_chord::host2::declare::ResourceDeclaration;
use pi_chord::host2::{
    Contract, Ctx, Declare, GetOptions, Host, HostOptions, Implementation, Listenable, ReadableRef, RootOptions,
    ServeOptions, StateEvent, create_host, method, on_event, on_value,
};
use pi_chord::{Json, json};
use tokio::sync::oneshot;

fn host_with(dev: bool) -> Host {
    create_host(HostOptions {
        principal: json!({"kind": "host"}),
        dev,
        on_error: Some(Arc::new(|_| {})),
        ..HostOptions::default()
    })
}

fn hold(host: &Host, contract: &Contract) -> pi_chord::host2::Ref {
    host.hold(contract, json!({}), GetOptions::default()).unwrap()
}

fn items() -> Json {
    let item = object(&[req("id", number()), req("label", string())]);
    object(&[req("items", array(item)), req("count", number())])
}

fn list() -> Contract {
    let item = object(&[req("id", number()), req("label", string())]);
    local_type(
        "t.list",
        object(&[]),
        vec![
            ("list", items().into()),
            ("labels", array(string()).into()),
            ("push", method(item, None).into()),
            ("set", method(items(), None).into()),
        ],
    )
}

fn serve_list(host: &Host) {
    let factory = |_: &Json, _: &Json, decl: &mut Declare| {
        let state = decl.state("list", items(), json!({"items": [], "count": 0}))?;
        let labels = decl.derive(vec![state.clone() as ReadableRef], |values| {
            let items = values[0]["items"].as_array().cloned().unwrap_or_default();
            Ok(Json::array(items.iter().map(|item| item["label"].clone()).collect()))
        });
        let (pushed, set) = (state.clone(), state.clone());
        Ok(Implementation::new()
            .state("list", state)
            .state("labels", labels)
            .method("push", move |item, ctx| {
                let result = pushed.change(&ctx, |draft| {
                    draft.get_mut("items").map(|items| items.push(item)).transpose()?;
                    let count = draft["count"].as_f64().unwrap_or_default();
                    draft.set("count", count + 1.0)
                });
                ready(result.map(|()| Json::Null))
            })
            .method("set", move |value, ctx| ready(set.replace(&ctx, value).map(|()| Json::Null))))
    };
    host.serve(&list(), factory, ServeOptions::default()).unwrap();
}

fn record(log: &Log<StateEvent>) -> impl Fn(&StateEvent) + Send + Sync + 'static {
    let log = log.clone();
    move |event| log.push(event.clone())
}

#[tokio::test]
async fn publishes_snapshots_then_exact_ops_and_derivations_follow() {
    let _serial = serial().await;
    let host = host_with(false);
    serve_list(&host);
    let reference = hold(&host, &list());
    let (events, labels): (Log<StateEvent>, Log<StateEvent>) = Default::default();
    let _events = on_event(&*reference.state("list").unwrap(), record(&events));
    let _labels = on_event(&*reference.state("labels").unwrap(), record(&labels));
    reference.call("push", Some(json!({"id": 1, "label": "a"})), None).await.unwrap();
    // Subscribed while loading: the snapshot once live, then exact ops.
    let events = events.all();
    assert_eq!(event_json(&events[0]), json!({"type": "snap", "value": {"items": [], "count": 0}}));
    let StateEvent::Ops { ops, value } = &events[1] else { panic!("expected ops, got {:?}", events[1]) };
    let mut ops: Vec<String> = ops.iter().map(|op| op.to_json().to_string()).collect();
    ops.sort();
    assert_eq!(ops, [r#"["p",["items"],0,0,[{"id":1,"label":"a"}]]"#, r#"["s",["count"],1]"#]);
    assert_eq!(value, &json!({"items": [{"id": 1, "label": "a"}], "count": 1}));
    assert_eq!(reference.state("labels").unwrap().value().unwrap(), json!(["a"]));
    assert_eq!(labels.all().iter().map(StateEvent::kind).collect::<Vec<_>>(), ["snap", "ops"]);
    reference.call("set", Some(json!({"items": [], "count": 0})), None).await.unwrap();
    assert_eq!(reference.state("labels").unwrap().value().unwrap(), json!([]));
}

#[tokio::test]
async fn supports_primitive_cells_and_value_returning_changes() {
    let _serial = serial().await;
    let host = host_with(false);
    let counter =
        local_type("t.prim", object(&[]), vec![("n", number().into()), ("inc", method(None, number()).into())]);
    let factory = |_: &Json, _: &Json, decl: &mut Declare| {
        let n = decl.state("n", number(), json!(0))?;
        Ok(Implementation::new().state("n", n.clone()).method("inc", move |_, ctx| {
            let result = n.update(&ctx, |n| Ok(json!(n.as_f64().unwrap_or_default() + 1.0)));
            ready(result.and_then(|()| pi_chord::host2::Readable::value(&*n)))
        }))
    };
    host.serve(&counter, factory, ServeOptions::default()).unwrap();
    let reference = hold(&host, &counter);
    let (values, events): (Log<Json>, Log<StateEvent>) = Default::default();
    reference.ready().await.unwrap();
    let n = reference.state("n").unwrap();
    let _events = on_event(&*n, record(&events));
    let valued = values.clone();
    let _values = on_value(&*n, move |value| valued.push(value.clone()));
    assert_eq!(reference.call("inc", None, None).await.unwrap(), Some(json!(1)));
    assert_eq!(reference.call("inc", None, None).await.unwrap(), Some(json!(2)));
    assert_eq!(values.all(), [json!(0), json!(1), json!(2)]);
    let events: Vec<Json> = events.all().iter().map(event_json).collect();
    assert_eq!(
        events,
        [
            json!({"type": "snap", "value": 0}),
            json!({"type": "ops", "ops": [["r", 1]], "value": 1}),
            json!({"type": "ops", "ops": [["r", 2]], "value": 2}),
        ]
    );
}

#[tokio::test]
async fn rejects_a_returned_value_that_does_not_fit_the_schema_even_outside_dev_mode() {
    let _serial = serial().await;
    let host = host_with(false);
    let boxed = local_type("t.box", object(&[]), vec![("put", method(None, None).into())]);
    let factory = |_: &Json, _: &Json, decl: &mut Declare| {
        let list = decl.state("list", array(number()), json!([]))?;
        // The Rust form of upstream's slip (a change that returns push()'s length): `update` returns a number.
        Ok(Implementation::new()
            .method("put", move |_, ctx| ready(list.update(&ctx, |_| Ok(json!(1))).map(|()| Json::Null))))
    };
    host.serve(&boxed, factory, ServeOptions::default()).unwrap();
    let error = hold(&host, &boxed).call("put", None, None).await.unwrap_err();
    assert!(is(&error, "invalid"), "{error}");
}

#[tokio::test]
async fn validates_first_values_always_and_writes_only_in_dev_mode() {
    let _serial = serial().await;
    let bad = local_type("t.bad-initial", object(&[]), vec![]);
    let host = host_with(false);
    let factory = |_: &Json, _: &Json, decl: &mut Declare| {
        decl.state("x", object(&[req("n", number())]), json!({"n": "no"}))?;
        Ok(Implementation::new())
    };
    host.serve(&bad, factory, ServeOptions::default()).unwrap();
    let error = hold(&host, &bad).ready().await.unwrap_err();
    assert!(error.to_string().contains("initial value of state x"), "{error}");

    let lax = host_with(false);
    serve_list(&lax);
    hold(&lax, &list()).call("set", Some(json!({"items": [], "count": "x"})), None).await.unwrap();

    let dev = host_with(true);
    serve_list(&dev);
    let reference = hold(&dev, &list());
    let error = reference.call("set", Some(json!({"items": [], "count": "x"})), None).await.unwrap_err();
    assert!(is(&error, "invalid") && error.reason() == Some("state"), "{error}");
    assert_eq!(reference.state("list").unwrap().value().unwrap(), json!({"items": [], "count": 0}));
}

#[tokio::test]
async fn rejects_writes_from_inside_a_factory_and_placeholder_use_before_activation() {
    let _serial = serial().await;
    let host = host_with(false);
    let writer = local_type("t.writer", object(&[]), vec![]);
    let captured: Arc<Mutex<Option<Ctx>>> = Arc::default();
    let capture = captured.clone();
    let run = move |ctx: Ctx| async move {
        *capture.lock().unwrap() = Some(ctx);
        Ok(())
    };
    host.root(run, RootOptions::default()).await.unwrap();
    let placeholder = Log::<String>::default();
    let seen = placeholder.clone();
    let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        let x = decl.state("x", object(&[]), json!({}))?;
        let r = decl.resource("r", ResourceDeclaration::value(1, 1))?;
        seen.push(r.value().map_or_else(|error| error.code().map_or("none", |code| code.as_str()), |_| "value"));
        let ctx = captured.lock().unwrap().clone().unwrap();
        x.replace(&ctx, json!({}))?;
        Ok(Implementation::new())
    };
    host.serve(&writer, factory, ServeOptions::default()).unwrap();
    let error = hold(&host, &writer).ready().await.unwrap_err();
    assert!(error.to_string().contains("read_only"), "{error}");
    assert_eq!(placeholder.all(), ["not_ready"]);
}

#[tokio::test]
async fn listeners_run_each_event_in_a_fresh_root_and_tasks_get_an_abortable_ctx() {
    let _serial = serial().await;
    let host = host_with(false);
    let events = Emitter::default();
    let sum = object(&[req("sum", number())]);
    let watcher = local_type("t.watcher", object(&[]), vec![("seen", sum.clone().into())]);
    let roots: Log = Log::default();
    let task_aborted = Arc::new(AtomicBool::new(false));
    let (emitter, rooted, aborted) = (events.clone(), roots.clone(), task_aborted.clone());
    let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        let seen = decl.state("seen", sum.clone(), json!({"sum": 0}))?;
        let source = decl.resource("source", ResourceDeclaration::value(1, emitter.clone()))?;
        let (target, rooted) = (seen.clone(), rooted.clone());
        let select = |source: &Emitter| Ok(Listenable::Events(source.source()));
        decl.on_select("onEvent", &source, "source => source", select, move |value, ctx| {
            rooted.push(ctx.root_id());
            let result = target.change(&ctx, |draft| {
                let total = draft["sum"].as_f64().unwrap_or_default() + value.as_f64().unwrap_or_default();
                draft.set("sum", total)
            });
            ready(result)
        })?;
        let aborted = aborted.clone();
        decl.task(move |ctx| {
            let aborted = aborted.clone();
            let (sender, receiver) = oneshot::channel::<()>();
            ctx.signal().add_listener(move |_| {
                let _ = sender.send(());
            });
            async move {
                let _ = receiver.await;
                aborted.store(true, Ordering::SeqCst);
                Ok(())
            }
        });
        Ok(Implementation::new().state("seen", seen))
    };
    host.serve(&watcher, factory, ServeOptions::default()).unwrap();
    let reference = hold(&host, &watcher);
    reference.ready().await.unwrap();
    events.emit(json!(2));
    events.emit(json!(3));
    tick().await;
    assert_eq!(reference.state("seen").unwrap().value().unwrap(), json!({"sum": 5}));
    assert_eq!(roots.all().into_iter().collect::<HashSet<_>>().len(), 2);
    reference.release();
    tick().await;
    tick().await;
    assert_eq!(events.size(), 0);
    assert!(task_aborted.load(Ordering::SeqCst));
}

#[tokio::test]
async fn re_entrant_writes_publish_in_adoption_order_and_derivations_never_go_stale() {
    let _serial = serial().await;
    let host = host_with(false);
    let echo = local_type(
        "t.reentrant",
        object(&[]),
        vec![("n", number().into()), ("d", number().into()), ("set", method(number(), None).into())],
    );
    let factory = |_: &Json, _: &Json, decl: &mut Declare| {
        let n = decl.state("n", number(), json!(0))?;
        let d = decl
            .derive(vec![n.clone() as ReadableRef], |values| Ok(json!(values[0].as_f64().unwrap_or_default() * 2.0)));
        // Subscribed first: re-enters the write while revision 1 is being published.
        let bumped = n.clone();
        decl.on("bump", &n, move |value, ctx| {
            ready(if value == json!(1) { bumped.replace(&ctx, json!(2)) } else { Ok(()) })
        })?;
        let set = n.clone();
        Ok(Implementation::new()
            .state("n", n)
            .state("d", d)
            .method("set", move |value, ctx| ready(set.replace(&ctx, value).map(|()| Json::Null))))
    };
    host.serve(&echo, factory, ServeOptions::default()).unwrap();
    let reference = hold(&host, &echo);
    reference.ready().await.unwrap();
    let values: Log<Json> = Log::default();
    let replica: Arc<Mutex<Option<Json>>> = Arc::default();
    let replayed = values.clone();
    let _events = on_event(&*reference.state("n").unwrap(), move |event| {
        let mut replica = replica.lock().unwrap();
        let next = match event {
            StateEvent::Ops { ops, .. } => apply_immutable(replica.as_ref(), ops).unwrap(),
            other => other.value().clone(),
        };
        *replica = Some(next.clone());
        replayed.push(next);
    });
    let doubled: Log<Json> = Log::default();
    let pushed = doubled.clone();
    let _doubled = on_value(&*reference.state("d").unwrap(), move |value| pushed.push(value.clone()));
    reference.call("set", Some(json!(1)), None).await.unwrap();
    tick().await;
    assert_eq!(values.all(), [json!(0), json!(1), json!(2)]);
    assert_eq!(reference.state("d").unwrap().value().unwrap(), json!(4));
    assert_eq!(doubled.all(), [json!(0), json!(2), json!(4)]);
}
