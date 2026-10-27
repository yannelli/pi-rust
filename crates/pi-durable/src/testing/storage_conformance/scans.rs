//! Ordered scans of conversations, entries, tasks, and submissions.

use std::sync::Arc;

use super::records::submission;
use super::{context, create_root, entry, expect_err, ids, mint, pending_task};
use crate::errors::DurableResult;
use crate::ids::{ConversationId, EntryId, SubmissionId, TaskId};
use crate::types::{
    ConversationOwner, ConversationParent, ConversationQuery, ConversationRecord, Cursor, EntryQuery, EntryRecord,
    ScanOrder, Storage, StorageWrite, SubmissionQuery, SubmissionStatus, TaskQuery,
};

fn conversation(id: ConversationId) -> StorageWrite {
    StorageWrite::Conversation { value: ConversationRecord { id, parent: None, owner: None } }
}

fn entry_write(id: EntryId, conversation_id: ConversationId) -> StorageWrite {
    StorageWrite::Entry { value: entry(id, conversation_id, "message") }
}

fn marker(id: EntryId, conversation_id: ConversationId, head: EntryId) -> StorageWrite {
    StorageWrite::Entry { value: EntryRecord { head: Some(head), ..entry(id, conversation_id, "marker") } }
}

/// The cursor after a JSON round trip, as a caller that stores it would hand it back.
fn round_trip(cursor: &Option<Cursor>) -> Option<Cursor> {
    let text = pi_chord::Json::object(cursor.clone().expect("a cursor")).to_string();
    pi_chord::Json::parse(&text).unwrap().as_object().cloned()
}

async fn entry_ids(
    storage: &dyn Storage,
    query: &EntryQuery,
    limit: usize,
    cursor: Option<&Cursor>,
) -> (Vec<u64>, Option<Cursor>) {
    let page = storage.scan_entries(query, limit, cursor, context()).await.unwrap();
    (ids(&page.items, |entry| entry.id.0), page.next)
}

pub async fn indexes_entries_out_of_id_order(storage: Arc<dyn Storage>) {
    let storage = &*storage;
    let root_id = create_root(storage).await;
    let writes = [
        entry_write(EntryId(30), root_id),
        entry_write(EntryId(10), root_id),
        marker(EntryId(20), root_id, EntryId(10)),
    ];
    storage.commit(&writes, &|_| {}, context()).await.unwrap();
    assert_eq!(entry_ids(storage, &EntryQuery::new(root_id), 10, None).await.0, [30, 20, 10]);
    let found = storage.find_latest_head_marker(root_id, None, context()).await.unwrap();
    assert_eq!(found.map(|entry| entry.id), Some(EntryId(20)));
}

pub async fn continues_entry_cursor_after_newer_commit(storage: Arc<dyn Storage>) {
    let storage = &*storage;
    let root_id = create_root(storage).await;
    let oldest: EntryId = mint(storage).await;
    let middle: EntryId = mint(storage).await;
    let newest: EntryId = mint(storage).await;
    let writes = [entry_write(oldest, root_id), entry_write(middle, root_id), entry_write(newest, root_id)];
    storage.commit(&writes, &|_| {}, context()).await.unwrap();
    let (first, next) = entry_ids(storage, &EntryQuery::new(root_id), 2, None).await;
    assert_eq!(first, [newest.0, middle.0]);
    let appended: EntryId = mint(storage).await;
    storage.commit(&[entry_write(appended, root_id)], &|_| {}, context()).await.unwrap();
    let (second, next) = entry_ids(storage, &EntryQuery::new(root_id), 2, next.as_ref()).await;
    assert_eq!(second, [oldest.0]);
    assert_eq!(next, None);
}

pub async fn paginates_conversations_by_cursor(storage: Arc<dyn Storage>) {
    let storage = &*storage;
    let root_id = create_root(storage).await;
    let second_id: ConversationId = mint(storage).await;
    let third_id: ConversationId = mint(storage).await;
    storage.commit(&[conversation(third_id), conversation(second_id)], &|_| {}, context()).await.unwrap();
    let query = ConversationQuery::default();
    let first = storage.scan_conversations(&query, 2, None, context()).await.unwrap();
    assert_eq!(ids(&first.items, |record| record.id.0), [root_id.0, second_id.0]);
    let cursor = round_trip(&first.next);
    let second = storage.scan_conversations(&query, 2, cursor.as_ref(), context()).await.unwrap();
    assert_eq!(ids(&second.items, |record| record.id.0), [third_id.0]);
    assert_eq!(second.next, None);
}

pub async fn filters_conversations_by_owner(storage: Arc<dyn Storage>) {
    let storage = &*storage;
    let root_id = create_root(storage).await;
    let other_owner_id: ConversationId = mint(storage).await;
    let first_task: TaskId = mint(storage).await;
    let second_task: TaskId = mint(storage).await;
    let first_id: ConversationId = mint(storage).await;
    let second_id: ConversationId = mint(storage).await;
    let third_id: ConversationId = mint(storage).await;
    let owned = |id, conversation_id, task_id| StorageWrite::Conversation {
        value: ConversationRecord { id, parent: None, owner: Some(ConversationOwner { conversation_id, task_id }) },
    };
    let writes = [
        conversation(other_owner_id),
        owned(first_id, root_id, first_task),
        owned(second_id, root_id, second_task),
        owned(third_id, other_owner_id, first_task),
    ];
    storage.commit(&writes, &|_| {}, context()).await.unwrap();

    let by_root = ConversationQuery { owner_conversation_id: Some(root_id), ..Default::default() };
    let first = storage.scan_conversations(&by_root, 1, None, context()).await.unwrap();
    assert_eq!(ids(&first.items, |record| record.id.0), [first_id.0]);
    assert!(first.next.is_some());
    let second = storage.scan_conversations(&by_root, 1, first.next.as_ref(), context()).await.unwrap();
    assert_eq!(ids(&second.items, |record| record.id.0), [second_id.0]);
    assert_eq!(second.next, None);
    let by_task = ConversationQuery { owner_task_id: Some(first_task), ..Default::default() };
    let page = storage.scan_conversations(&by_task, 10, None, context()).await.unwrap();
    assert_eq!(ids(&page.items, |record| record.id.0), [first_id.0, third_id.0]);
    let both = ConversationQuery { owner_task_id: Some(first_task), ..by_root };
    let page = storage.scan_conversations(&both, 10, None, context()).await.unwrap();
    assert_eq!(ids(&page.items, |record| record.id.0), [first_id.0]);
}

pub async fn scans_deep_fork_history(storage: Arc<dyn Storage>) {
    let storage = &*storage;
    let ctx = context();
    let root_id = create_root(storage).await;
    let root_first: EntryId = mint(storage).await;
    let root_fork_point: EntryId = mint(storage).await;
    let root_excluded_same_commit: EntryId = mint(storage).await;
    let writes = [
        entry_write(root_first, root_id),
        marker(root_fork_point, root_id, root_first),
        entry_write(root_excluded_same_commit, root_id),
    ];
    let root_entries_seq = storage.commit(&writes, &|_| {}, ctx).await.unwrap();
    let child_id: ConversationId = mint(storage).await;
    let fork = |id, conversation_id, at| StorageWrite::Conversation {
        value: ConversationRecord { id, parent: Some(ConversationParent { conversation_id, at }), owner: None },
    };
    storage.commit(&[fork(child_id, root_id, root_fork_point)], &|_| {}, ctx).await.unwrap();
    let child_fork_point: EntryId = mint(storage).await;
    let child_excluded: EntryId = mint(storage).await;
    let note = StorageWrite::Entry { value: entry(child_fork_point, child_id, "note") };
    storage.commit(&[note, entry_write(child_excluded, child_id)], &|_| {}, ctx).await.unwrap();
    let root_excluded_later: EntryId = mint(storage).await;
    storage.commit(&[entry_write(root_excluded_later, root_id)], &|_| {}, ctx).await.unwrap();
    let grandchild_id: ConversationId = mint(storage).await;
    storage.commit(&[fork(grandchild_id, child_id, child_fork_point)], &|_| {}, ctx).await.unwrap();
    let grandchild_head: EntryId = mint(storage).await;
    let grandchild_tail: EntryId = mint(storage).await;
    let writes = [marker(grandchild_head, grandchild_id, grandchild_head), entry_write(grandchild_tail, grandchild_id)];
    let grandchild_entries_seq = storage.commit(&writes, &|_| {}, ctx).await.unwrap();
    let child_excluded_later: EntryId = mint(storage).await;
    storage.commit(&[entry_write(child_excluded_later, child_id)], &|_| {}, ctx).await.unwrap();

    let all = EntryQuery::new(grandchild_id);
    let (first, next) = entry_ids(storage, &all, 2, None).await;
    assert_eq!(first, [grandchild_tail.0, grandchild_head.0]);
    let (second, next) = entry_ids(storage, &all, 2, next.as_ref()).await;
    assert_eq!(second, [child_fork_point.0, root_fork_point.0]);
    let (third, next) = entry_ids(storage, &all, 2, next.as_ref()).await;
    assert_eq!(third, [root_first.0]);
    assert_eq!(next, None);

    // Oldest first: the root's segment, then each fork's, each capped at the next fork point.
    let ascending = EntryQuery { order: Some(ScanOrder::Ascending), ..EntryQuery::new(grandchild_id) };
    let (first_up, up_next) = entry_ids(storage, &ascending, 2, None).await;
    assert_eq!(first_up, [root_first.0, root_fork_point.0]);
    let (second_up, next) = entry_ids(storage, &ascending, 2, up_next.as_ref()).await;
    assert_eq!(second_up, [child_fork_point.0, grandchild_head.0]);
    let (third_up, next) = entry_ids(storage, &all, 2, next.as_ref()).await;
    assert_eq!(third_up, [grandchild_tail.0]);
    assert_eq!(next, None);
    let bounded = EntryQuery { min_entry_id: Some(root_fork_point), max_entry_id: Some(child_fork_point), ..ascending };
    assert_eq!(entry_ids(storage, &bounded, 10, None).await.0, [root_fork_point.0, child_fork_point.0]);
    let descending = EntryQuery { order: Some(ScanOrder::Descending), ..EntryQuery::new(grandchild_id) };
    expect_err(storage.scan_entries(&descending, 2, up_next.as_ref(), ctx).await, "cursor");

    let current_marker = storage.find_latest_head_marker(grandchild_id, None, ctx).await.unwrap().unwrap();
    assert_eq!((current_marker.id, current_marker.head), (grandchild_head, Some(grandchild_head)));
    let historical =
        storage.find_latest_head_marker(grandchild_id, Some(child_fork_point), ctx).await.unwrap().unwrap();
    assert_eq!((historical.id, historical.head), (root_fork_point, Some(root_first)));
    assert_eq!(storage.find_latest_head_marker(grandchild_id, Some(root_first), ctx).await.unwrap(), None);

    let active = EntryQuery { min_entry_id: current_marker.head, ..EntryQuery::new(grandchild_id) };
    let (active_first, next) = entry_ids(storage, &active, 1, None).await;
    assert_eq!(active_first, [grandchild_tail.0]);
    assert!(next.is_some());
    let (active_second, next) = entry_ids(storage, &active, 1, next.as_ref()).await;
    assert_eq!(active_second, [grandchild_head.0]);
    assert_eq!(next, None);
    let through = EntryQuery {
        min_entry_id: historical.head,
        max_entry_id: Some(child_fork_point),
        ..EntryQuery::new(grandchild_id)
    };
    assert_eq!(entry_ids(storage, &through, 10, None).await.0, [child_fork_point.0, root_fork_point.0, root_first.0]);

    let stored = storage.entry(root_first, ctx).await.unwrap().unwrap();
    assert_eq!((stored.entry, stored.commit_seq), (entry(root_first, root_id, "message"), root_entries_seq));
    let seq_of =
        |result: DurableResult<Option<crate::types::StoredEntry>>| result.unwrap().map(|found| found.commit_seq);
    assert_eq!(seq_of(storage.entry(root_fork_point, ctx).await), Some(root_entries_seq));
    assert_eq!(seq_of(storage.entry(grandchild_head, ctx).await), Some(grandchild_entries_seq));
    assert_eq!(seq_of(storage.entry(grandchild_tail, ctx).await), Some(grandchild_entries_seq));
    assert_eq!(storage.entry(EntryId(999_999), ctx).await.unwrap(), None);

    let visible = storage.visible_entry(grandchild_id, root_first, ctx).await.unwrap().unwrap();
    assert_eq!((visible.entry, visible.commit_seq), (entry(root_first, root_id, "message"), root_entries_seq));
    let visible = storage.visible_entry(grandchild_id, child_fork_point, ctx).await.unwrap().unwrap();
    assert_eq!(visible.entry.conversation_id, child_id);
    assert_eq!(seq_of(storage.visible_entry(grandchild_id, grandchild_tail, ctx).await), Some(grandchild_entries_seq));
    for hidden in
        [root_excluded_same_commit, root_excluded_later, child_excluded, child_excluded_later, EntryId(999_999)]
    {
        assert_eq!(storage.visible_entry(grandchild_id, hidden, ctx).await.unwrap(), None);
    }
    assert_eq!(storage.visible_entry(root_id, grandchild_head, ctx).await.unwrap(), None);
    expect_err(storage.visible_entry(ConversationId(999_999), root_first, ctx).await, "Unknown conversation");
    let unknown = EntryQuery::new(ConversationId(999_999));
    expect_err(storage.scan_entries(&unknown, 10, None, ctx).await, "Unknown conversation");
}

async fn scan_ids(
    storage: &dyn Storage,
    table: usize,
    order: Option<ScanOrder>,
    cursor: Option<&Cursor>,
) -> DurableResult<(Vec<u64>, Option<Cursor>)> {
    let ctx = context();
    Ok(match table {
        0 => {
            let page =
                storage.scan_conversations(&ConversationQuery { order, ..Default::default() }, 2, cursor, ctx).await?;
            (ids(&page.items, |item| item.id.0), page.next)
        }
        1 => {
            let page = storage.scan_tasks(&TaskQuery { order, ..Default::default() }, 2, cursor, ctx).await?;
            (ids(&page.items, |item| item.id.0), page.next)
        }
        _ => {
            let page =
                storage.scan_submissions(&SubmissionQuery { order, ..Default::default() }, 2, cursor, ctx).await?;
            (ids(&page.items, |item| item.id.0), page.next)
        }
    })
}

pub async fn scans_tables_in_either_order(storage: Arc<dyn Storage>) {
    let storage = &*storage;
    let root_id = create_root(storage).await;
    let mut expected: [Vec<u64>; 3] = [vec![root_id.0], Vec::new(), Vec::new()];
    for _ in 0..3 {
        let conversation_id: ConversationId = mint(storage).await;
        let task_id: TaskId = mint(storage).await;
        let submission_id: SubmissionId = mint(storage).await;
        let writes = [
            conversation(conversation_id),
            StorageWrite::Task { value: pending_task(task_id, root_id, "ready") },
            StorageWrite::Submission { value: submission(submission_id, root_id, SubmissionStatus::Queued) },
        ];
        storage.commit(&writes, &|_| {}, context()).await.unwrap();
        expected[0].push(conversation_id.0);
        expected[1].push(task_id.0);
        expected[2].push(submission_id.0);
    }
    for (table, ids) in expected.iter().enumerate() {
        let all = |order: Option<ScanOrder>| async move {
            let (mut found, mut next) = scan_ids(storage, table, order, None).await.unwrap();
            while next.is_some() {
                // A cursor carries its order; the query may omit it.
                let page = scan_ids(storage, table, None, round_trip(&next).as_ref()).await.unwrap();
                found.extend(page.0);
                next = page.1;
            }
            found
        };
        let reversed: Vec<u64> = ids.iter().rev().copied().collect();
        assert_eq!(&all(None).await, ids);
        assert_eq!(&all(Some(ScanOrder::Ascending)).await, ids);
        assert_eq!(all(Some(ScanOrder::Descending)).await, reversed);
        let (_, next) = scan_ids(storage, table, Some(ScanOrder::Descending), None).await.unwrap();
        let (second, _) = scan_ids(storage, table, Some(ScanOrder::Descending), next.as_ref()).await.unwrap();
        assert_eq!(second, reversed[2..reversed.len().min(4)]);
        expect_err(scan_ids(storage, table, Some(ScanOrder::Ascending), next.as_ref()).await, "cursor");
    }
}
