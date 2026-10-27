//! Port of upstream `test/chord-guide.test.ts`: the code of the Chord usage guide, run against a real Harness.
//! Rust Chord services are untyped: members are named methods over JSON and named replicated states, so the
//! guide's typed service interfaces become `ServiceImpl`s and calls go through `ServiceHandle::call`.

mod support;

use std::sync::{Arc, LazyLock, Mutex};

use pi_chord::facets::{Facet, FacetHost, FacetOptions, create_facet_host, define_facet};
use pi_chord::services::{
    RemoteServiceBindingOptions, RemoteServices, Service, ServiceImpl, create_loopback_service_transport,
    create_remote_service_binding, define_service, observe_handler,
};
use pi_chord::{BACKGROUND_CONTEXT, ChordError, ChordResult, Context, DeliveryKind, Json, StateListener};
use pi_durable::documents::*;
use pi_durable::harness::*;
use pi_durable::session::{Session, Tx};
use pi_durable::storage::MemoryStorage;
use pi_durable::tasks::TaskOptions;
use pi_durable::{ConversationId, TaskId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use support::task_support::{completed, eventually, open_tasks};
use support::{Deferred, context};

type Log = Arc<Mutex<Vec<Value>>>;

fn kind(delivery: DeliveryKind) -> &'static str {
    match delivery {
        DeliveryKind::Hydrate => "hydrate",
        DeliveryKind::Update => "update",
    }
}

// ─── 1. A Session-wide canvas ────────────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct CanvasState {
    strokes: Vec<Value>,
}

static CANVAS_DOC: LazyLock<DocToken<CanvasState, SessionScope>> =
    LazyLock::new(|| {
        // Store a complete base after at most 99 replayed deltas.
        let spec = DocSpec::new("app.canvas", 1, SessionScope, CanvasState::default, |_, _, info| {
            Ok(info.deltas_since_base >= 99)
        });
        define_doc(spec).unwrap()
    });

struct Canvas;

fn canvas() -> Service<Canvas> {
    define_service("app.canvas").unwrap()
}

async fn create_canvas_facet(session: &Session, context: &Context) -> ChordResult<Arc<dyn Facet>> {
    // Creation is explicit; observation never writes.
    session.commit(|tx: Tx| async move { tx.doc(&CANVAS_DOC, ()).await.map(drop) }, context).await?;
    let state = session.document_state(&CANVAS_DOC, (), context).await?;
    let state = Arc::new(state.ok_or_else(|| ChordError::error("canvas was retired during setup"))?);
    let session = session.clone();
    Ok(define_facet("app.canvas/session", move |env| {
        let owned = state.clone();
        env.own(move || owned.dispose())?;
        let session = session.clone();
        let add_stroke = move |args: Vec<Json>, context: Context| {
            let (session, stroke) = (session.clone(), args.first().map(Json::to_serde).unwrap_or_default());
            async move {
                let change =
                    |tx: Tx| async move { tx.doc(&CANVAS_DOC, ()).await?.edit(|canvas| canvas.strokes.push(stroke)) };
                session.commit(change, &context).await?;
                Ok(None)
            }
        };
        env.provide(&canvas(), ServiceImpl::new().state("state", state.member()).method("addStroke", add_stroke))
    }))
}

fn canvas_consumer(log: &Log) -> Arc<dyn Facet> {
    let log = log.clone();
    define_facet("app.canvas/consumer", move |env| {
        let canvas = env.use_service(&canvas())?; // Declare now; access only after activation.
        let (env2, log) = (env.clone(), log.clone());
        env.on_activate(move || async move {
            // The subscription delivers the current hydrated value, then updates.
            let stop = canvas.state("state")?.subscribe(StateListener::sync(move |value, _, delivery| {
                let strokes = value
                    .get("strokes")
                    .map_or(json!("retired"), |s| json!(s.to_serde().as_array().map_or(0, Vec::len)));
                log.lock().unwrap().push(json!([kind(delivery.kind), delivery.sequence, strokes]));
                Ok(())
            }))?;
            env2.own(move || stop.close())
        })
    })
}

fn stroke(color: &str, points: Value) -> Json {
    Json::from(json!({ "color": color, "points": points }))
}

async fn run_canvas_example(session: &Session, log: &Log) -> ChordResult<()> {
    let context = BACKGROUND_CONTEXT.clone();
    let provider = create_canvas_facet(session, &context).await?;
    let host = create_facet_host(FacetOptions::new(vec![provider, canvas_consumer(log)])).await?;
    let points = json!([{ "x": 10, "y": 20 }, { "x": 30, "y": 40 }]);
    let added =
        host.services().use_service(&canvas())?.call("addStroke", vec![stroke("black", points)], &context).await;
    // Disposal unsubscribes; it neither deletes the canvas nor closes the Session.
    host.dispose().await?;
    added.map(drop)
}

type Detach = Box<dyn FnOnce() -> futures::future::BoxFuture<'static, ChordResult<()>> + Send>;

async fn connect_canvas(host: &FacetHost, log: &Log, context: &Context) -> ChordResult<Detach> {
    let options =
        RemoteServiceBindingOptions::new(vec![canvas().erase()], create_loopback_service_transport(host.services()));
    let services = Arc::new(create_remote_service_binding(options)?);
    let log = log.clone();
    let stop = services.use_service(&canvas())?.state("state")?.subscribe(StateListener::sync(
        move |value, _, delivery| {
            let strokes = value.get("strokes").map_or(json!("retired"), Json::to_serde);
            log.lock().unwrap().push(json!([kind(delivery.kind), delivery.sequence, strokes]));
            Ok(())
        },
    ))?;
    // Ready once the first snapshot is installed; later updates keep arriving after it.
    if let Err(error) = services.ready(context).await {
        stop.close()?;
        services.dispose(&BACKGROUND_CONTEXT).await?;
        return Err(error);
    }
    Ok(Box::new(move || {
        Box::pin(async move {
            stop.close()?;
            services.dispose(&BACKGROUND_CONTEXT).await
        })
    }))
}

#[tokio::test]
async fn runs_the_canvas_facet_host_a_remote_client_withdrawal_and_detach_before_harness_close() {
    let log = Log::default();
    let harness = open_tasks(Arc::new(MemoryStorage::new()), Vec::new()).await.harness;
    run_canvas_example(&harness, &log).await.unwrap();
    assert_eq!(*log.lock().unwrap(), [json!(["hydrate", 0, 0]), json!(["update", 1, 1])]);
    let stored = harness.snapshot(&CANVAS_DOC, (), &context()).await.unwrap().unwrap().to_serde();
    assert_eq!(stored["strokes"].as_array().map(Vec::len), Some(1));

    // A worker installs the canvas facet again; a late remote client hydrates the stroke, then follows updates.
    log.lock().unwrap().clear();
    let host = create_facet_host(FacetOptions::new(vec![create_canvas_facet(&harness, &context()).await.unwrap()]))
        .await
        .unwrap();
    let detach = connect_canvas(&host, &log, &context()).await.unwrap();
    let red = stroke("red", json!([]));
    host.services().use_service(&canvas()).unwrap().call("addStroke", vec![red], &context()).await.unwrap();
    let seen = log.clone();
    eventually(move || {
        let done = seen.lock().unwrap().len() == 2;
        async move { done }
    })
    .await;
    let frames: Vec<(String, usize)> = log
        .lock()
        .unwrap()
        .iter()
        .map(|frame| (frame[0].as_str().unwrap().to_owned(), frame[2].as_array().unwrap().len()))
        .collect();
    assert_eq!(frames, [("hydrate".to_owned(), 1), ("update".to_owned(), 2)]);
    // Shutdown order: clients detach and services withdraw while the Harness is still open.
    detach().await.unwrap();
    host.dispose().await.unwrap();
    harness.close(&context()).await.unwrap();
    let disposed = host.services().use_service(&canvas()).err().unwrap().to_string();
    assert!(disposed.contains("disposed"), "{disposed}");
    let closed = harness.commit(|_: Tx| async { Ok(()) }, &context()).await.unwrap_err().to_string();
    assert!(closed.contains("closed"), "{closed}");
}

// ─── 2. Conversation-scoped diff reviews ─────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct ReviewInput {
    path: String,
    patch: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct ReviewState {
    path: String,
    patch: String,
    comments: Vec<Value>,
}

static REVIEW_DOC: LazyLock<DocFamilyToken<ReviewState, ReviewInput, LatestConversation>> = LazyLock::new(|| {
    let initial =
        |seed: ReviewInput| ReviewState { path: seed.path.clone(), patch: seed.patch.clone(), comments: Vec::new() };
    let spec =
        DocFamilySpec::new("app.diff-review", 1, LatestConversation(LatestFork::Current), initial, |_, _, info| {
            Ok(info.deltas_since_base >= 49)
        });
    define_doc_family(spec).unwrap()
});

struct DiffReviews;

fn diff_reviews() -> Service<DiffReviews> {
    define_service("app.diff-reviews").unwrap()
}

fn review_facet(
    session: &Session,
    conversation_id: ConversationId,
    reviews: Vec<(String, ReviewInput)>,
) -> Arc<dyn Facet> {
    let session = session.clone();
    define_facet("app.diff-reviews/session", move |env| {
        let instances = env.provide_many(&diff_reviews())?;
        let (env2, session, reviews) = (env.clone(), session.clone(), reviews.clone());
        env.on_activate(move || async move {
            for (key, seed) in reviews {
                let (create_key, create_seed) = (key.clone(), seed.clone());
                let create = move |tx: Tx| async move {
                    tx.family_doc(&REVIEW_DOC, conversation_id, &create_key, &create_seed).await.map(drop)
                };
                session.commit(create, &BACKGROUND_CONTEXT).await?;
                let state =
                    session.family_document_state(&REVIEW_DOC, conversation_id, &key, &BACKGROUND_CONTEXT).await?;
                let state = Arc::new(state.ok_or_else(|| ChordError::error("review was retired during setup"))?);
                let owned = state.clone();
                env2.own(move || owned.dispose())?;
                let identity_key = key.clone();
                let identity = move |_: Vec<Json>, _: Context| {
                    let identity = json!({ "conversationId": conversation_id, "key": identity_key });
                    async move { Ok(Some(Json::from(identity))) }
                };
                let (session, comment_key) = (session.clone(), key.clone());
                let add_comment = move |args: Vec<Json>, context: Context| {
                    let (session, key, seed) = (session.clone(), comment_key.clone(), seed.clone());
                    let comment = args.first().map(Json::to_serde).unwrap_or_default();
                    async move {
                        let change = move |tx: Tx| async move {
                            let draft = tx.family_doc(&REVIEW_DOC, conversation_id, &key, &seed).await?;
                            draft.edit(|review| review.comments.push(comment))
                        };
                        session.commit(change, &context).await?;
                        Ok(None)
                    }
                };
                // Chord instance keys route services; they are not numeric document incarnation IDs.
                let service = ServiceImpl::new()
                    .state("state", state.member())
                    .method("identity", identity)
                    .method("addComment", add_comment);
                let instance = serde_json::to_string(&json!([conversation_id, key])).unwrap();
                instances.spawn(&instance, service)?; // The facet owns spawned service lifetimes.
            }
            Ok(())
        })
    })
}

#[tokio::test]
async fn runs_the_diff_reviews_with_a_keyed_consumer() {
    let harness = open_tasks(Arc::new(MemoryStorage::new()), Vec::new()).await.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let reviews = vec![("review-7".to_owned(), ReviewInput { path: "a.ts".into(), patch: "-old\n+new".into() })];
    let seen = Log::default();
    let sink = seen.clone();
    let consumer = define_facet("app.diff-reviews/consumer", move |env| {
        let sink = sink.clone();
        env.observe(
            &diff_reviews(),
            observe_handler(move |review, context| {
                let sink = sink.clone();
                async move {
                    let identity = review.call("identity", Vec::new(), &context).await?.unwrap().to_serde();
                    let key = identity["key"].clone();
                    let stop = review.state("state")?.subscribe(StateListener::sync(move |value, _, _| {
                        let comments = value
                            .get("comments")
                            .map_or(-1, |c| c.to_serde().as_array().map_or(-1, |c| c.len() as i64));
                        sink.lock().unwrap().push(json!({ "key": key, "comments": comments }));
                        Ok(())
                    }))?;
                    std::mem::forget(stop);
                    let comment = Json::from(json!({ "id": "c1", "line": 1, "text": "why?" }));
                    review.call("addComment", vec![comment], &context).await.map(drop)
                }
            }),
        )
    });
    let host =
        create_facet_host(FacetOptions::new(vec![review_facet(&harness, root.id, reviews), consumer])).await.unwrap();
    let frames = seen.clone();
    eventually(move || {
        let done = frames.lock().unwrap().iter().any(|frame| frame["comments"] == 1);
        async move { done }
    })
    .await;
    let expected = [json!({ "key": "review-7", "comments": 0 }), json!({ "key": "review-7", "comments": 1 })];
    assert_eq!(*seen.lock().unwrap(), expected);
    let stored =
        harness.family_snapshot(&REVIEW_DOC, root.id, "review-7", &context()).await.unwrap().unwrap().to_serde();
    assert_eq!((&stored["path"], &stored["comments"][0]["id"]), (&json!("a.ts"), &json!("c1")));
    host.dispose().await.unwrap();
    harness.close(&context()).await.unwrap();
}

// ─── 3. Task-scoped output and a tool/task watch ─────────────────────────────

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct JobOutput {
    stdout: String,
    chunks: u32,
}

static JOB_OUTPUT_DOC: LazyLock<DocToken<JobOutput, TaskScope>> = LazyLock::new(|| {
    let spec =
        DocSpec::new("app.job-output", 1, TaskScope, JobOutput::default, |_, _, info| Ok(info.deltas_since_base >= 99));
    define_doc(spec).unwrap()
});

async fn append_job_output(runtime: &TaskRuntime, chunk: &str, context: &Context) -> pi_durable::DurableResult<()> {
    // Read process output outside this callback. The runtime gates the live task.
    let (id, chunk) = (runtime.task_id(), chunk.to_owned());
    let change = move |tx: Tx, _| async move {
        tx.doc(&JOB_OUTPUT_DOC, id).await?.edit(|output| {
            let stdout = format!("{}{chunk}", output.stdout);
            output.stdout = stdout.chars().rev().take(50_000).collect::<Vec<_>>().into_iter().rev().collect();
            output.chunks += 1;
        })?;
        Ok(None)
    };
    runtime.commit(change, context).await
}

async fn observe_job(
    session: &Session,
    producer: TaskId,
    finished: tokio::task::JoinHandle<()>,
    log: &Log,
    rendered: &Log,
) {
    let Some(watch) = session.watch_doc(&JOB_OUTPUT_DOC, producer, &context()).await.unwrap() else { return };
    let value = watch.value();
    log.lock().unwrap().push(json!([value.get("stdout").map_or(json!("retired"), Json::to_serde)]));
    let rendered = rendered.clone();
    watch
        .start(move |pi_chord::WatchChange { value, ops: _ops, .. }, _| {
            let text = value.get("stdout").map_or(json!("retired"), Json::to_serde);
            rendered.lock().unwrap().push(text);
            async { Ok(()) }
        })
        .unwrap(); // Serialized callbacks never overlap.
    finished.await.unwrap(); // Caller-supplied observation lifetime; outside any commit.
    watch.stop().await; // Idempotent; prevents another callback from starting.
}

#[tokio::test]
async fn runs_the_job_output_watch_until_the_producer_retires_its_document() {
    let (log, rendered) = (Log::default(), Log::default());
    let gate = Deferred::default();
    let held = gate.clone();
    let job = define_task(
        TaskDefinition::new("app.job", 1, |_| Ok(Json::from(json!({ "phase": "running" }))))
            .phase("running", move |task, runtime, ctx| {
                let held = held.clone();
                async move {
                    let command = task.input.get("command").and_then(Json::as_str).unwrap_or_default().to_owned();
                    append_job_output(&runtime, &format!("$ {command}\n"), &ctx).await?;
                    held.wait().await;
                    append_job_output(&runtime, "ok\n", &ctx).await?;
                    runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &ctx).await
                }
            })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let harness = open_tasks(Arc::new(MemoryStorage::new()), vec![job.clone()]).await.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let id = root.id;
    let create = move |tx: Tx| async move {
        tx.create_task(job, Json::from(json!({ "command": "make" })), TaskOptions::conversation(id)).await
    };
    let task = root.commit(create, &context()).await.unwrap();
    harness.resume().unwrap();
    let probe = harness.clone();
    eventually(move || {
        let harness = probe.clone();
        async move {
            let output = harness.snapshot(&JOB_OUTPUT_DOC, task, &context()).await.unwrap();
            output.and_then(|output| output.get("chunks").and_then(Json::as_u64)) == Some(1)
        }
    })
    .await;
    let waiter = harness.clone();
    let finished = tokio::spawn(async move { waiter.wait_for_task(task, &context()).await.map(drop).unwrap() });
    let (session, watch_log, watch_rendered) = (harness.clone(), log.clone(), rendered.clone());
    let observing =
        tokio::spawn(async move { observe_job(&session, task, finished, &watch_log, &watch_rendered).await });
    let seen = log.clone();
    eventually(move || {
        let done = seen.lock().unwrap().len() == 1;
        async move { done }
    })
    .await;
    gate.resolve();
    observing.await.unwrap();
    assert_eq!(*log.lock().unwrap(), [json!(["$ make\n"])]);
    // Stopped once the task finished; the retirement frame may or may not have been delivered before.
    assert_eq!(rendered.lock().unwrap().first(), Some(&json!("$ make\nok\n")));
    assert!(harness.watch_doc(&JOB_OUTPUT_DOC, task, &context()).await.unwrap().is_none());
    harness.close(&context()).await.unwrap();
}
