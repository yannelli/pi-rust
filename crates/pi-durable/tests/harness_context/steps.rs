//! The upstream context read cases with random transcripts, owned views, late reads, and retention.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::Duration;

use futures::FutureExt;
use pi_chord::Context;
use pi_durable::{ConversationId, ConversationOwnership};

use super::derivation::{calling, system, tool_result};
use super::reads::{Counting, Lcg, Rows, counting_setup, probe_task};
use super::support::{Deferred, StorageCall};
use super::*;

/// The random transcript writer of the extended-reads case, with the categories it wrote.
#[derive(Default)]
struct Writer {
    lcg: Lcg,
    written: Vec<EntryRecord>,
    calls: Vec<String>,
    next: usize,
    seen: BTreeMap<&'static str, usize>,
}

impl Default for Lcg {
    fn default() -> Self {
        Lcg(7)
    }
}

impl Writer {
    fn next(&mut self, prefix: &str) -> String {
        let label = format!("{prefix}{}", self.next);
        self.next += 1;
        label
    }

    fn count(&mut self, category: &'static str) {
        *self.seen.entry(category).or_default() += 1;
    }

    fn message(model: Message) -> EntryDraft {
        EntryDraft { model: Some(vec![model]), ..EntryDraft::new("message") }
    }

    fn draft(&mut self) -> EntryDraft {
        let pick = self.lcg.below(14);
        if pick < 3 {
            self.count("user");
            return Self::message(user(&self.next("u")));
        }
        if pick < 5 {
            let ids: Vec<String> = (0..self.lcg.below(3)).map(|_| self.next("c")).collect();
            self.calls.extend(ids.iter().cloned());
            let stop = [StopReason::Aborted, StopReason::Error, StopReason::Deferred].get(self.lcg.below(9)).copied();
            self.count(match stop {
                Some(StopReason::Aborted) => "aborted",
                Some(StopReason::Error) => "error",
                Some(_) => "deferred",
                None => "assistant",
            });
            let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
            return Self::message(calling(&self.next("a"), &ids, stop.unwrap_or(StopReason::Stop)));
        }
        if pick < 8 {
            let matched = !self.calls.is_empty() && self.lcg.below(5) > 0;
            self.count(if matched { "result" } else { "stray result" });
            let id = if matched {
                let index = self.lcg.below(self.calls.len());
                self.calls.remove(index)
            } else {
                self.next("stray")
            };
            return Self::message(tool_result(&id));
        }
        if pick < 9 {
            self.count("system");
            return Self::message(system("s", &self.next("v")));
        }
        if pick < 10 {
            self.count("note");
            let n = self.next;
            self.next += 1;
            return EntryDraft { data: Some(Json::from(json!({ "n": n }))), ..EntryDraft::new("note") };
        }
        let index = self.lcg.below(self.written.len());
        let target = self.written.get(index).map(|entry| entry.id);
        if let Some(target) = target.filter(|_| pick < 12) {
            let omit = self.lcg.below(2) == 0;
            self.count(if omit { "omit" } else { "replace" });
            let edit = if omit {
                ContextEdit::omit(target)
            } else {
                let messages = Some(vec![user(&self.next("r"))]);
                ContextEdit { target, action: ContextEditAction::Replace, messages }
            };
            return EntryDraft { edits: Some(vec![edit]), ..EntryDraft::new("edit") };
        }
        if let Some(target) = target.filter(|_| pick < 13) {
            self.count("head");
            let model = Some(vec![user(&self.next("h"))]);
            return EntryDraft { head: Some(DraftHead::Entry(target)), model, ..EntryDraft::new("summary") };
        }
        self.count("user");
        Self::message(user(&self.next("u")))
    }
}

type Shared<T> = Arc<Mutex<T>>;

/// Append one to three random entries to `conversation_id` in one runtime commit.
async fn append_random(writer: &Shared<Writer>, runtime: &TaskRuntime, id: ConversationId, task_context: &Context) {
    let writer = writer.clone();
    let change = move |tx: Tx, _| async move {
        let added = 1 + writer.lock().unwrap().lcg.below(3);
        for _ in 0..added {
            let draft = writer.lock().unwrap().draft();
            let entry = tx.append_entry(id, draft).await?;
            writer.lock().unwrap().written.push(entry);
        }
        Ok(None)
    };
    runtime.commit(change, task_context).await.unwrap();
}

#[tokio::test]
async fn derives_the_same_view_from_extended_reads_as_from_whole_reads() {
    let writer: Shared<Writer> = Arc::default();
    let root_slot: Shared<Option<Conversation>> = Arc::default();
    let (shared, roots) = (writer.clone(), root_slot.clone());
    let steps = probe_task("test.context-steps", move |_, runtime, task_context| {
        let (writer, root) = (shared.clone(), roots.lock().unwrap().clone().unwrap());
        async move {
            let pick = |writer: &Shared<Writer>| {
                let mut writer = writer.lock().unwrap();
                let len = writer.written.len();
                let index = writer.lcg.below(len);
                writer.written[index].id
            };
            for step in 0..150 {
                append_random(&writer, &runtime, root.id, &task_context).await;
                let read = runtime.context(root.id, &task_context, None).await.unwrap();
                assert_eq!(read, root.context(&task_context, None).await.unwrap());
                if step % 10 == 9 {
                    // Overlapping reads, one with an earlier cutoff, share the kept range.
                    let at = pick(&writer);
                    let (whole, cut) = tokio::join!(
                        runtime.context(root.id, &task_context, None),
                        runtime.context(root.id, &task_context, Some(at))
                    );
                    assert_eq!(whole.unwrap(), root.context(&task_context, None).await.unwrap());
                    assert_eq!(cut.unwrap(), root.context(&task_context, Some(at)).await.unwrap());
                }
            }
            // A fork sees its parent's entries through the fork point and extends with its own.
            let options = CreateOptions { ownership: ConversationOwnership::Ownerless, ..CreateOptions::default() };
            let fork = root.fork(pick(&writer), options, &task_context).await.unwrap();
            for _ in 0..20 {
                let read = runtime.context(fork.id, &task_context, None).await.unwrap();
                assert_eq!(read, fork.context(&task_context, None).await.unwrap());
                append_random(&writer, &runtime, fork.id, &task_context).await;
            }
        }
    });
    let counting = counting_setup(vec![steps.clone()], |options| options).await;
    *root_slot.lock().unwrap() = Some(counting.root.clone());
    counting.harness.resume().unwrap();
    counting.run(&steps, "steps").await;
    let seen = writer.lock().unwrap().seen.clone();
    let categories = [
        "user",
        "assistant",
        "aborted",
        "error",
        "deferred",
        "result",
        "stray result",
        "system",
        "note",
        "omit",
        "replace",
        "head",
    ];
    for category in categories {
        assert!(seen.get(category).is_some_and(|count| *count > 0), "{category} was never written");
    }
    counting.harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn keeps_returned_views_and_kept_ranges_apart() {
    let root_slot: Shared<Option<Conversation>> = Arc::default();
    let roots = root_slot.clone();
    let mutate = probe_task("test.context-mutate", move |_, runtime, task_context| {
        let root = roots.lock().unwrap().clone().unwrap();
        async move {
            let mut view = runtime.context(root.id, &task_context, None).await.unwrap();
            view.messages.clear();
            view.entries.pop();
            // Upstream also checks that kept entries are frozen; a Rust view is an owned copy, so it has no such hook.
            let again = runtime.context(root.id, &task_context, None).await.unwrap();
            assert_eq!(again, root.context(&task_context, None).await.unwrap());
        }
    });
    let counting = counting_setup(vec![mutate.clone()], |options| options).await;
    *root_slot.lock().unwrap() = Some(counting.root.clone());
    counting.harness.resume().unwrap();
    counting.run(&mutate, "mutate").await;
    counting.harness.close(&context()).await.unwrap();
}

// Upstream's case that freezes kept entries deeply under a shallowly freezing storage has no Rust counterpart: views
// and kept ranges are owned values, which the case above already checks.

fn retention(ms: f64) -> impl FnOnce(HarnessOptions) -> HarnessOptions {
    move |options| {
        options.with_settings(HarnessSettings { context_retention_ms: Some(ms), ..HarnessSettings::default() })
    }
}

/// A probe task that waits for `gate`, then records the rows its context read scanned under its input name.
fn counting_probe(rows: &Rows, storage: &Shared<Option<Counting>>, gate: Option<Deferred>) -> Task {
    let (rows, storage) = (rows.clone(), storage.clone());
    probe_task("test.context-probe", move |name, runtime, task_context| {
        let (rows, gate) = (rows.clone(), gate.clone());
        let storage = storage.lock().unwrap().as_ref().unwrap().storage.clone();
        async move {
            if let Some(gate) = gate {
                gate.wait().await;
            }
            let before = storage.scanned_rows.load(Ordering::SeqCst);
            runtime.context(runtime.conversation_id(), &task_context, None).await.unwrap();
            rows.lock().unwrap().insert(name, storage.scanned_rows.load(Ordering::SeqCst) - before);
        }
    })
}

fn rows_of(rows: &Rows) -> Vec<(String, usize)> {
    rows.lock().unwrap().clone().into_iter().collect()
}

#[tokio::test]
async fn does_not_keep_a_read_that_finishes_after_its_task_ended() {
    let rows: Rows = Arc::default();
    let setup: Shared<Option<Counting>> = Arc::default();
    let (scans, probe_start) = (Deferred::default(), Deferred::default());
    let late: Shared<Option<tokio::task::JoinHandle<()>>> = Arc::default();
    let (held_scans, late_slot, shared) = (scans.clone(), late.clone(), setup.clone());
    let late_task = probe_task("test.context-late", move |_, runtime, task_context| {
        let (scans, late_slot) = (held_scans.clone(), late_slot.clone());
        let storage = shared.lock().unwrap().as_ref().unwrap().storage.clone();
        async move {
            // Hold the next range scan; bounds probes read one row and pass.
            let armed = Arc::new(Mutex::new(Some(scans)));
            let hook = move |call: StorageCall| {
                let held =
                    if call.method == "scan_entries" && call.limit > 1 { armed.lock().unwrap().take() } else { None };
                async move {
                    if let Some(held) = held {
                        held.wait().await;
                    }
                    Ok(())
                }
                .boxed()
            };
            *storage.call_hook.lock().unwrap() = Some(Arc::new(hook));
            let reading = runtime.clone();
            let read = async move {
                let _ = reading.context(reading.conversation_id(), &task_context, None).await;
            };
            *late_slot.lock().unwrap() = Some(tokio::spawn(read));
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    let probe = counting_probe(&rows, &setup, Some(probe_start.clone()));
    let counting = counting_setup(vec![late_task.clone(), probe.clone()], retention(0.0)).await;
    let (harness, root) = (counting.harness.clone(), counting.root.clone());
    *setup.lock().unwrap() = Some(counting);
    harness.resume().unwrap();
    let create = |task: Task, name: &str| {
        let (id, input) = (root.id, Json::from(json!({ "name": name })));
        move |tx: Tx| async move { tx.create_task(task, input, pi_durable::tasks::TaskOptions::conversation(id)).await }
    };
    let late_id = root.commit(create(late_task, "late"), &context()).await.unwrap();
    harness.wait_for_task(late_id, &context()).await.unwrap();
    // The conversation is busy again when the late read finishes; its range must not be kept.
    let probe_id = root.commit(create(probe, "probe"), &context()).await.unwrap();
    scans.resolve();
    let late = late.lock().unwrap().take().unwrap();
    late.await.unwrap();
    probe_start.resolve();
    harness.wait_for_task(probe_id, &context()).await.unwrap();
    assert_eq!(rows_of(&rows), [("probe".to_owned(), 22)]);
    harness.close(&context()).await.unwrap();
}

async fn probe_twice(retention_ms: f64, pause: Duration) {
    let rows: Rows = Arc::default();
    let setup: Shared<Option<Counting>> = Arc::default();
    let probe = counting_probe(&rows, &setup, None);
    let counting = counting_setup(vec![probe.clone()], retention(retention_ms)).await;
    let (harness, root) = (counting.harness.clone(), counting.root.clone());
    *setup.lock().unwrap() = Some(counting);
    harness.resume().unwrap();
    for name in ["first", "second"] {
        let (id, input) = (root.id, Json::from(json!({ "name": name })));
        let task = probe.clone();
        let create = move |tx: Tx| async move {
            tx.create_task(task, input, pi_durable::tasks::TaskOptions::conversation(id)).await
        };
        let task_id = root.commit(create, &context()).await.unwrap();
        harness.wait_for_task(task_id, &context()).await.unwrap();
        root.wait_for_idle(&context()).await.unwrap();
        tokio::time::sleep(pause).await;
    }
    assert_eq!(rows_of(&rows), [("first".to_owned(), 22), ("second".to_owned(), 22)]);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn drops_an_idle_conversations_context_read_on_a_timer_when_nothing_else_runs() {
    // Upstream counts fake timers; here a short retention passes in real time before the second probe.
    probe_twice(30.0, Duration::from_millis(150)).await;
}

#[tokio::test]
async fn drops_a_conversations_context_read_once_idle_with_zero_retention() {
    probe_twice(0.0, Duration::ZERO).await;
}
