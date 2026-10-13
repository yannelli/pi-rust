//! Port of upstream `test/harness-output.test.ts` (tool output bounds, OutputBuffer, and Progress).

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_durable::DurableError;
use pi_durable::harness::Retain;
use pi_durable::harness::output::{OutputBuffer, OutputChunk, OutputLimits, bound_output, sanitize_output};
use pi_durable::harness::progress::Progress;
use tokio::time::Instant;

fn head(max_lines: usize, max_bytes: usize) -> OutputLimits {
    OutputLimits { max_bytes, max_lines, retain: Retain::Head }
}

fn tail(max_lines: usize, max_bytes: usize) -> OutputLimits {
    OutputLimits { max_bytes, max_lines, retain: Retain::Tail }
}

/// Node's `Buffer` tail slice on a character boundary.
fn buffer_tail(content: &str, max_bytes: usize) -> &str {
    let bytes = content.as_bytes();
    if bytes.len() <= max_bytes {
        return content;
    }
    let mut start = bytes.len() - max_bytes;
    while start < bytes.len() && (bytes[start] & 0xc0) == 0x80 {
        start += 1;
    }
    &content[start..]
}

/// A single line longer than the byte limit is cut like Buffer's tail slice on a character boundary.
fn assert_matches_buffer_tail(input: &str, max_byte_values: Option<Vec<usize>>) {
    let values = max_byte_values.unwrap_or_else(|| (0..input.len() + 5).collect());
    for max_bytes in values {
        let kept = bound_output(input, tail(10, max_bytes)).text;
        assert_eq!(kept, buffer_tail(input, max_bytes), "input={input:?} maxBytes={max_bytes}");
        assert!(kept.len() <= max_bytes, "tail output exceeded {max_bytes} bytes");
    }
}

fn sampled_byte_limits(input: &str) -> Vec<usize> {
    let total = input.len() as i64;
    let candidates = [0, 1, 2, 3, 4, 5, 8, total / 2, total - 4, total - 1, total, total + 1];
    let set: BTreeSet<usize> = candidates.into_iter().filter(|value| *value >= 0).map(|value| value as usize).collect();
    set.into_iter().collect()
}

fn bound(text: &str, limits: OutputLimits) -> (String, u64, u64) {
    let slice = bound_output(text, limits);
    (slice.text, slice.dropped_bytes, slice.dropped_lines)
}

fn kept(text: &str, dropped_bytes: u64, dropped_lines: u64) -> (String, u64, u64) {
    (text.to_owned(), dropped_bytes, dropped_lines)
}

#[test]
fn removes_control_characters_but_keeps_tabs_newlines_and_other_text() {
    assert_eq!(sanitize_output("a\0b\tc\nd\re\u{7}f\u{fff9}g\u{fffb}h😀"), "ab\tc\ndefgh😀");
}

#[test]
fn keeps_output_within_the_limits_unchanged() {
    assert_eq!(bound("a\nb\n", head(2, 1000)), kept("a\nb\n", 0, 0));
    assert_eq!(bound("a\nb", tail(2, 1000)), kept("a\nb", 0, 0));
    assert_eq!(bound("", tail(2, 1000)), kept("", 0, 0));
}

#[test]
fn keeps_nothing_with_a_zero_limit() {
    assert_eq!(bound("ab\ncd\n", head(10, 0)), kept("", 6, 2));
    assert_eq!(bound("ab\ncd\n", tail(0, 1000)), kept("", 6, 2));
}

#[test]
fn keeps_exact_slices_of_whole_lines_trailing_newline_included() {
    assert_eq!(bound("a\nb\nc\n", head(2, 1000)), kept("a\nb\n", 2, 1));
    assert_eq!(bound("a\nb\nc\n", tail(2, 1000)), kept("b\nc\n", 2, 1));
    assert_eq!(bound("a\nb\nc", tail(2, 1000)), kept("b\nc", 2, 1));
    // Blank lines are lines.
    assert_eq!(bound("a\nb\nc\n\n", tail(3, 1000)), kept("b\nc\n\n", 2, 1));
}

#[test]
fn cuts_at_the_byte_limit_on_whole_lines_when_possible() {
    assert_eq!(bound("aa\nbb\ncc\n", head(10, 7)), kept("aa\nbb\n", 3, 1));
    assert_eq!(bound("aa\nbb\ncc\n", tail(10, 7)), kept("bb\ncc\n", 3, 1));
}

#[test]
fn cuts_a_single_line_longer_than_the_byte_limit_on_a_character_boundary() {
    // "é" is two bytes; five bytes hold two whole characters.
    assert_eq!(bound("ééé\n", head(10, 5)), kept("éé", 3, 0));
    assert_eq!(bound("x\néééé", tail(10, 5)), kept("éé", 6, 1));
}

#[test]
fn keeps_a_u_feff_at_the_start_of_a_kept_slice() {
    assert_eq!(bound("x\u{feff}a", tail(10, 4)), kept("\u{feff}a", 1, 0));
}

// Rust strings hold no lone surrogates. Buffer encodes each as U+FFFD, so the surrogate cases use U+FFFD where
// the upstream inputs have a lone surrogate, and keep the paired ones as the characters they form.
#[test]
fn cuts_tails_of_surrogate_edge_cases_exactly_like_buffer() {
    let inputs = ["a\u{fffd}", "\u{fffd}b", "a\u{fffd}b", "\u{fffd}🙂", "🙂\u{fffd}", "👩\u{200d}💻"];
    for input in inputs {
        assert_matches_buffer_tail(input, None);
    }
}

#[test]
fn cuts_tails_exactly_like_buffer_across_deterministic_fuzz_cases() {
    let alphabet = [
        "a", "\u{7f}", "\u{80}", "é", "\u{7ff}", "\u{800}", "中", "\u{d7ff}", "\u{fffd}", "\u{fffd}", "\u{fffd}",
        "\u{fffd}", "🙂", "\u{e000}", "\u{ffff}",
    ];
    fn check_exhaustive(prefix: &str, depth: u32, alphabet: &[&str]) {
        assert_matches_buffer_tail(prefix, Some(sampled_byte_limits(prefix)));
        if depth == 0 {
            return;
        }
        for character in alphabet {
            check_exhaustive(&format!("{prefix}{character}"), depth - 1, alphabet);
        }
    }
    check_exhaustive("", 3, &alphabet);
    let mut seed: u32 = 0x1234_5678;
    let mut random = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        f64::from(seed) / 4_294_967_296.0
    };
    for _ in 0..1_000 {
        let length = (random() * 80.0).floor() as usize;
        let input: String =
            (0..length).map(|_| alphabet[(random() * alphabet.len() as f64).floor() as usize]).collect();
        assert_matches_buffer_tail(&input, Some(sampled_byte_limits(&input)));
    }
}

fn push(buffer: &mut OutputBuffer, text: &str) {
    buffer.push(OutputChunk::Text(text), None).unwrap();
}

fn push_bytes(buffer: &mut OutputBuffer, bytes: &[u8]) {
    buffer.push(OutputChunk::Bytes(bytes), None).unwrap();
}

fn snapshot(buffer: &mut OutputBuffer) -> (String, u64, u64) {
    let snapshot = buffer.snapshot();
    (snapshot.text, snapshot.dropped_bytes, snapshot.dropped_lines)
}

#[test]
fn keeps_exact_totals_across_chunks_and_decodes_utf_8_split_across_byte_chunks() {
    let mut buffer = OutputBuffer::new(tail(2, 1000));
    let bytes = "😀\n".as_bytes();
    push(&mut buffer, "a\nb\n");
    push_bytes(&mut buffer, &bytes[..2]);
    push_bytes(&mut buffer, &bytes[2..]);
    assert_eq!(snapshot(&mut buffer), kept("b\n😀\n", 2, 1));
}

#[test]
fn sanitizes_the_kept_text_but_counts_the_raw_stream() {
    let mut buffer = OutputBuffer::new(tail(1, 1000));
    push(&mut buffer, "a\u{7}\n");
    assert_eq!(snapshot(&mut buffer), kept("a\n", 0, 0));
    push(&mut buffer, "b\u{1b}\n");
    assert_eq!(snapshot(&mut buffer), kept("b\n", 3, 1));
}

#[test]
fn drops_a_byte_order_mark_only_at_the_start_of_the_output() {
    let bom = [0xef, 0xbb, 0xbf];
    let mut buffer = OutputBuffer::new(tail(10, 1000));
    push_bytes(&mut buffer, &bom[..1]);
    push_bytes(&mut buffer, &[0xbb, 0xbf, 0x61]);
    push(&mut buffer, "b");
    // A U+FEFF after a string chunk is text.
    push_bytes(&mut buffer, &[0xef, 0xbb, 0xbf, 0x63]);
    buffer.end();
    assert_eq!(buffer.snapshot().text, "ab\u{feff}c");
}

#[test]
fn flushes_an_incomplete_character_before_a_string_chunk_and_at_the_end() {
    let mut buffer = OutputBuffer::new(tail(10, 1000));
    let euro = "€".as_bytes();
    push_bytes(&mut buffer, &euro[..1]);
    push(&mut buffer, "x");
    push_bytes(&mut buffer, &euro[..2]);
    buffer.end();
    assert_eq!(buffer.snapshot().text, "\u{fffd}x\u{fffd}");
}

#[test]
fn matches_bounding_the_whole_stream_when_several_chunks_arrive_between_snapshots() {
    for limits in [head(3, 40), tail(3, 40), head(50, 25), tail(50, 25)] {
        let mut buffer = OutputBuffer::new(limits);
        let mut stream = String::new();
        for index in 0..300 {
            let chunk =
                if index % 7 == 0 { format!("{}\n", "é".repeat(index % 30)) } else { format!("line {index}\n") };
            stream.push_str(&chunk);
            push(&mut buffer, &chunk);
            if index % 5 != 4 {
                continue;
            }
            assert_eq!(snapshot(&mut buffer), bound(&stream, limits));
        }
    }
}

#[test]
fn stops_storing_head_output_once_the_window_is_full() {
    let mut buffer = OutputBuffer::new(head(2, 1000));
    for index in 0..1000 {
        push(&mut buffer, &format!("line {index}\n"));
    }
    assert!(buffer.stored_bytes() < 20);
    assert_eq!(snapshot(&mut buffer), kept("line 0\nline 1\n", 8876, 998));
}

#[test]
fn stores_only_the_tail_window_after_each_snapshot() {
    let mut buffer = OutputBuffer::new(tail(3, 100));
    let mut stream = String::new();
    for index in 0..2000 {
        let chunk = format!("line {index}\n\n");
        stream.push_str(&chunk);
        push(&mut buffer, &chunk);
        let snapshot = buffer.snapshot();
        assert!(buffer.stored_bytes() <= 100);
        assert_eq!(snapshot.text, bound_output(&stream, tail(3, 100)).text);
    }
}

/// A Progress whose writes record the paused clock in milliseconds and return `size`.
fn recording(size: Arc<AtomicU64>, min_interval_ms: u64) -> (Progress, Arc<Mutex<Vec<u64>>>) {
    let commits = Arc::new(Mutex::new(Vec::new()));
    let (sink, start) = (commits.clone(), Instant::now());
    let progress = Progress::new(
        move || {
            sink.lock().unwrap().push(start.elapsed().as_millis() as u64);
            let size = size.load(Ordering::SeqCst);
            Box::pin(async move { Ok(size) })
        },
        |_| {},
        min_interval_ms,
    );
    (progress, commits)
}

async fn advance(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
    tokio::task::yield_now().await;
}

#[tokio::test(start_paused = true)]
async fn commits_the_first_change_at_once_then_waits_at_least_100_ms_and_the_written_size_at_100_kib_s() {
    let size = Arc::new(AtomicU64::new(50 * 1024));
    let (progress, commits) = recording(size.clone(), 100);
    progress.mark();
    advance(0).await;
    assert_eq!(*commits.lock().unwrap(), [0]);
    // 50 KiB buys 500 ms; changes meanwhile coalesce into one commit.
    size.store(10, Ordering::SeqCst);
    progress.mark();
    progress.mark();
    advance(499).await;
    assert_eq!(*commits.lock().unwrap(), [0]);
    advance(1).await;
    assert_eq!(*commits.lock().unwrap(), [0, 500]);
    // A small commit still waits the minimum 100 ms.
    progress.mark();
    advance(99).await;
    assert_eq!(*commits.lock().unwrap(), [0, 500]);
    advance(1).await;
    assert_eq!(*commits.lock().unwrap(), [0, 500, 600]);
}

#[tokio::test(start_paused = true)]
async fn waits_the_configured_minimum_interval_between_small_commits() {
    let (progress, commits) = recording(Arc::new(AtomicU64::new(10)), 500);
    progress.mark();
    advance(0).await;
    progress.mark();
    advance(499).await;
    assert_eq!(*commits.lock().unwrap(), [0]);
    advance(1).await;
    assert_eq!(*commits.lock().unwrap(), [0, 500]);
}

#[tokio::test]
async fn rejects_the_waiters_of_a_failed_commit_and_reports_its_error() {
    let errors = Arc::new(Mutex::new(Vec::new()));
    let sink = errors.clone();
    let progress = Progress::new(
        || Box::pin(async { Err(DurableError::error("commit failed")) }),
        move |error| sink.lock().unwrap().push(error.to_string()),
        100,
    );
    let error = progress.mark_and_wait().await.unwrap().unwrap_err();
    assert_eq!(error.to_string(), "commit failed");
    assert_eq!(*errors.lock().unwrap(), ["commit failed"]);
}

#[tokio::test(start_paused = true)]
async fn stops_waits_for_the_commit_in_flight_and_hands_back_waiters_no_commit_covered_yet() {
    let in_flight = Arc::new(tokio::sync::Notify::new());
    let release = in_flight.clone();
    let progress = Progress::new(
        move || {
            let in_flight = in_flight.clone();
            Box::pin(async move {
                in_flight.notified().await;
                Ok(0)
            })
        },
        |_| {},
        100,
    );
    let first = progress.mark_and_wait();
    let second = progress.mark_and_wait();
    let stopping = progress.clone();
    let stopped = tokio::spawn(async move { stopping.stop().await });
    advance(0).await;
    release.notify_one();
    let pending = stopped.await.unwrap();
    first.await.unwrap().unwrap();
    assert_eq!(pending.len(), 1);
    for waiter in pending {
        waiter.send(Ok(())).unwrap();
    }
    second.await.unwrap().unwrap();
}
