//! Port of upstream `test/state-init.test.ts`.

mod chord2_support;

use std::future::ready;
use std::sync::{Arc, Mutex};

use chord2_support::*;
use pi_chord::host2::cell::Cell;
use pi_chord::host2::declare::{OpenOptions, ResourceDeclaration};
use pi_chord::host2::{
    Contract, Ctx, Declare, GetOptions, Host, HostOptions, Implementation, Readable, ReadableRef, RefStatus,
    RootOptions, ServeOptions, create_host, method, on_value,
};
use pi_chord::{ChordError, ChordResult, Json, json};

fn host() -> Host {
    create_host(HostOptions {
        principal: json!({"kind": "host"}),
        on_error: Some(Arc::new(|_| {})),
        ..HostOptions::default()
    })
}

fn notes_type() -> Contract {
    local_type(
        "t.notes",
        object(&[]),
        vec![
            ("notes", array(string()).into()),
            ("count", number().into()),
            ("has", method(string(), boolean()).into()),
            ("add", method(string(), None).into()),
        ],
    )
}

fn hold(host: &Host, contract: &Contract) -> pi_chord::host2::Ref {
    host.hold(contract, json!({}), GetOptions::default()).unwrap()
}

fn joined(list: &Json) -> String {
    let items = list.as_array().map(|items| items.iter().filter_map(Json::as_str).collect::<Vec<_>>());
    items.unwrap_or_default().join(",")
}

/// The members of `Notes` over `notes`: `count` derived, `has` and `add` reading and writing it.
fn notes_members(decl: &mut Declare, notes: Arc<Cell>) -> ChordResult<Implementation> {
    let count =
        decl.derive(vec![notes.clone() as ReadableRef], |values| Ok(json!(values[0].as_array().map_or(0, Vec::len))));
    let (read, write) = (notes.clone(), notes.clone());
    Ok(Implementation::new()
        .state("notes", notes)
        .state("count", count)
        .method("has", move |note, _| {
            let found = read.value().map(|list| list.as_array().is_some_and(|list| list.contains(&note)));
            ready(found.map(Json::from))
        })
        .method("add", move |note, ctx| {
            let result = write.change(&ctx, |draft| draft.push(note));
            ready(result.map(|()| Json::Null))
        }))
}

#[tokio::test]
async fn runs_after_the_resources_are_open_and_before_the_object_goes_live() {
    let _serial = serial().await;
    let host = host();
    let loaded = Latch::default();
    let log: Log = Log::default();
    let (logged, latch) = (log.clone(), loaded.clone());
    let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        let (opened, latch) = (logged.clone(), latch.clone());
        let open = move |_: OpenOptions| {
            opened.push("open");
            ready(Ok(latch.clone()))
        };
        let db = decl.resource("db", ResourceDeclaration::new(1, open))?;
        let initialized = logged.clone();
        let notes = decl.state_init("notes", array(string()), move |_| {
            initialized.push("init");
            let db = db.value();
            async move { Ok(db?.wait().await) }
        })?;
        let listened = logged.clone();
        decl.on("notes", &notes, move |list, _| {
            listened.push(format!("listener {}", joined(&list)));
            ready(Ok(()))
        })?;
        let (tasked, read) = (logged.clone(), notes.clone());
        decl.task(move |_| {
            let pushed = read.value().map(|list| tasked.push(format!("task {}", joined(&list))));
            ready(pushed)
        });
        notes_members(decl, notes)
    };
    host.serve(&notes_type(), factory, ServeOptions::default()).unwrap();
    let reference = hold(&host, &notes_type());
    let seen: Log<Json> = Log::default();
    let valued = seen.clone();
    let _seen = on_value(&*reference.state("notes").unwrap(), move |value| valued.push(value.clone()));
    // A call made while init runs waits until the object is live, then reads the loaded value.
    let asked = reference.clone();
    let has = tokio::spawn(host.root(
        move |ctx: Ctx| async move { asked.call_json("has", json!("a"), Some(&ctx)).await },
        RootOptions::default(),
    ));
    until(|| log.all().iter().any(|entry| entry == "init")).await;
    assert_eq!(reference.status(), RefStatus::Loading);
    loaded.open(json!(["a", "b"]));
    assert_eq!(has.await.unwrap().unwrap(), json!(true));
    assert_eq!(reference.state("count").unwrap().value().unwrap(), json!(2));
    until(|| log.len() == 4).await;
    assert_eq!(log.all(), ["open", "init", "listener a,b", "task a,b"]);
    // Before live, a reference's member has no value; its first value is the loaded one.
    let values: Vec<Json> = seen.all().into_iter().filter(|value| !value.is_null()).collect();
    assert_eq!(values, [json!(["a", "b"])]);
}

#[tokio::test]
async fn an_init_cell_cannot_be_read_inside_the_factory() {
    let _serial = serial().await;
    let host = host();
    let factory = |_: &Json, _: &Json, decl: &mut Declare| {
        let notes = decl.state_init("notes", array(string()), |_| ready(Ok(json!([]))))?;
        notes.value()?;
        notes_members(decl, notes)
    };
    host.serve(&notes_type(), factory, ServeOptions::default()).unwrap();
    let error = hold(&host, &notes_type()).ready().await.unwrap_err();
    assert!(is(&error, "not_ready"), "{error}");
}

#[tokio::test]
async fn a_failing_or_invalid_init_fails_the_activation_and_closes_its_resources() {
    let _serial = serial().await;
    let host = host();
    let result: Arc<Mutex<Result<Json, String>>> = Arc::new(Mutex::new(Err("db down".into())));
    let closed: Log = Log::default();
    let (outcome, closing) = (result.clone(), closed.clone());
    let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        let closing = closing.clone();
        let close = move |db: String| {
            closing.push(db);
            ready(Ok(()))
        };
        decl.resource("db", ResourceDeclaration::value(1, "db".to_string()).with_close(close))?;
        let outcome = outcome.clone();
        let notes = decl.state_init("notes", array(string()), move |_| {
            ready(outcome.lock().unwrap().clone().map_err(ChordError::error))
        })?;
        notes_members(decl, notes)
    };
    host.serve(&notes_type(), factory, ServeOptions::default()).unwrap();
    let error = hold(&host, &notes_type()).ready().await.unwrap_err();
    assert!(error.to_string().contains("db down"), "{error}");
    assert_eq!(closed.all(), ["db"]);
    *result.lock().unwrap() = Ok(json!([1]));
    let error = hold(&host, &notes_type()).ready().await.unwrap_err();
    assert!(is(&error, "invalid"), "{error}");
    assert_eq!(closed.all(), ["db", "db"]);
    *result.lock().unwrap() = Ok(json!(["ok"]));
    let reference = hold(&host, &notes_type());
    reference.ready().await.unwrap();
    assert_eq!(reference.state("notes").unwrap().value().unwrap(), json!(["ok"]));
}

#[tokio::test]
async fn init_runs_in_a_root_and_can_call_other_objects() {
    let _serial = serial().await;
    let host = host();
    let source = local_type("t.source", object(&[]), vec![("all", method(None, array(string())).into())]);
    let all = |_: &Json, _: &Json, _: &mut Declare| {
        Ok(Implementation::new().method("all", |_, _| ready(Ok(json!(["from source"])))))
    };
    host.serve(&source, all, ServeOptions::default()).unwrap();
    let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        let source = source.clone();
        let notes = decl.state_init("notes", array(string()), move |ctx: Ctx| {
            let source = source.clone();
            async move {
                let reference = ctx.get(&source, json!({}), GetOptions::default()).await?;
                reference.call_json("all", Json::Null, Some(&ctx)).await
            }
        })?;
        notes_members(decl, notes)
    };
    host.serve(&notes_type(), factory, ServeOptions::default()).unwrap();
    let reference = hold(&host, &notes_type());
    reference.ready().await.unwrap();
    assert_eq!(reference.state("notes").unwrap().value().unwrap(), json!(["from source"]));
}

type More = Arc<dyn Fn() -> ChordResult<Json> + Send + Sync>;

/// The notes facet of the reload cases: `more` adds an init cell, `count_version` the counter's version.
fn reload_facet(inits: &Log, more: Option<More>, count_version: i64) -> pi_chord::host2::Facet {
    let inits = inits.clone();
    pi_chord::host2::define_facet("notes", move |api| {
        let (inits, more) = (inits.clone(), more.clone());
        let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
            let loaded = inits.clone();
            let notes = decl.state_init("notes", array(string()), move |_| {
                loaded.push("notes");
                ready(Ok(json!(["loaded"])))
            })?;
            let options = pi_chord::host2::StateOptions { version: Some(count_version), migrate: None };
            let counter = decl.state_with("counter", number(), pi_chord::host2::Initial::Value(json!(0)), options)?;
            let mut sources: Vec<ReadableRef> = vec![notes.clone(), counter];
            if let Some(more) = more.clone() {
                let loaded = inits.clone();
                sources.push(decl.state_init("more", array(string()), move |_| {
                    loaded.push("more");
                    ready(more())
                })?);
            }
            let sum = |values: &[Json]| {
                let size =
                    |value: &Json| value.as_f64().unwrap_or_else(|| value.as_array().map_or(0, |a| a.len()) as f64);
                Ok(json!(values.iter().map(size).sum::<f64>()))
            };
            let count = decl.derive(sources, sum);
            let (has, add) = (notes.clone(), notes.clone());
            Ok(Implementation::new()
                .state("notes", notes)
                .state("count", count)
                .method("has", move |note, _| {
                    let found = has.value().map(|notes| json!(notes.as_array().is_some_and(|n| n.contains(&note))));
                    ready(found)
                })
                .method("add", move |note, ctx| {
                    let changed = add.change(&ctx, |draft| draft.push(note));
                    ready(changed.map(|()| Json::Null))
                }))
        };
        api.serve(&notes_type(), factory, ServeOptions::default())
    })
}

async fn reload_with(host: &Host, facet: pi_chord::host2::Facet) -> ChordResult<pi_chord::host2::ReloadReport> {
    host.reload(vec![facet.into()], Default::default()).await
}

fn value(reference: &pi_chord::host2::Ref, name: &str) -> Json {
    reference.state(name).unwrap().value().unwrap()
}

#[tokio::test]
async fn reload_keeps_an_existing_cell_without_init_and_runs_init_for_an_added_one() {
    let _serial = serial().await;
    let host = host();
    let inits: Log = Log::default();
    host.install(&reload_facet(&inits, None, 1), Default::default()).await.unwrap();
    let reference = hold(&host, &notes_type());
    reference.ready().await.unwrap();
    let adding = reference.clone();
    let run = move |ctx: Ctx| async move { adding.call("add", Some(json!("written")), Some(&ctx)).await };
    host.root(run, RootOptions::default()).await.unwrap();
    let more: More = Arc::new(|| Ok(json!(["x", "y"])));
    reload_with(&host, reload_facet(&inits, Some(more), 1)).await.unwrap();
    assert_eq!(inits.all(), ["notes", "more"]);
    assert_eq!(value(&reference, "notes"), json!(["loaded", "written"]));
    // The new derivation counts the kept cell and the added one: 2 + 0 + 2.
    assert_eq!(value(&reference, "count"), json!(4));
}

#[tokio::test]
async fn reload_with_a_failing_init_of_an_added_cell_is_refused_and_the_old_code_keeps_running() {
    let _serial = serial().await;
    let host = host();
    let inits: Log = Log::default();
    host.install(&reload_facet(&inits, None, 1), Default::default()).await.unwrap();
    let reference = hold(&host, &notes_type());
    reference.ready().await.unwrap();
    let more: More = Arc::new(|| Err(ChordError::error("cannot load more")));
    let error = reload_with(&host, reload_facet(&inits, Some(more), 1)).await.unwrap_err();
    assert!(error.to_string().contains("cannot load more"), "{error}");
    assert_eq!(value(&reference, "count"), json!(1));
    let run = move |ctx: Ctx| async move { reference.call("has", Some(json!("loaded")), Some(&ctx)).await };
    assert_eq!(host.root(run, RootOptions::default()).await.unwrap(), Some(json!(true)));
}

#[tokio::test]
async fn in_a_gated_reload_an_added_cell_runs_init_at_the_gate() {
    let _serial = serial().await;
    let host = host();
    let inits: Log = Log::default();
    host.install(&reload_facet(&inits, None, 1), Default::default()).await.unwrap();
    let reference = hold(&host, &notes_type());
    reference.ready().await.unwrap();
    let more: More = Arc::new(|| Ok(json!(["x"])));
    let report = reload_with(&host, reload_facet(&inits, Some(more), 2)).await.unwrap();
    assert_eq!(report.gated.len(), 1);
    assert_eq!(inits.all(), ["notes", "more"]);
    assert_eq!(value(&reference, "count"), json!(2));
}
