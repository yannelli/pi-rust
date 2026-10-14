//! Port of upstream `test/harness-output-skip.test.ts` (OutputBuffer skipped output).

use pi_durable::env::ShellOutputSkip;
use pi_durable::harness::Retain;
use pi_durable::harness::output::{OutputBuffer, OutputChunk, OutputLimits};

/// Deterministic PRNG (mulberry32) so failures reproduce from the printed seed.
fn random(seed: u32) -> impl FnMut() -> f64 {
    let mut state = seed;
    move || {
        state = state.wrapping_add(0x6d2b_79f5);
        let mut t = state;
        t = (t ^ (t >> 15)).wrapping_mul(t | 1);
        t ^= t.wrapping_add((t ^ (t >> 7)).wrapping_mul(t | 61));
        f64::from(t ^ (t >> 14)) / 4_294_967_296.0
    }
}

// Decoded shell output: ASCII, multi-byte and astral characters, replacement characters, CR/LF, tabs, and control
// characters that sanitizing removes after bounding.
const ALPHABET: [&str; 15] =
    ["a", "b", "z", " ", "\n", "\n", "\n", "\r\n", "\t", "é", "€", "😀", "\u{fffd}", "\x01", "\x1b"];

fn pick(next: &mut impl FnMut() -> f64, count: usize) -> usize {
    (next() * count as f64).floor() as usize
}

fn random_chunk(next: &mut impl FnMut() -> f64) -> String {
    let length = pick(next, 12);
    (0..length).map(|_| ALPHABET[pick(next, ALPHABET.len())]).collect()
}

fn measure(text: &str) -> ShellOutputSkip {
    let newlines = text.chars().filter(|character| *character == '\n').count() as u64;
    ShellOutputSkip { bytes: text.len() as u64, newlines, ends_with_newline: text.ends_with('\n') }
}

/// Whether `text` proves that everything before it is outside the tail window.
fn exceeds_window(text: &str, limits: OutputLimits) -> bool {
    let skip = measure(text);
    skip.bytes > limits.max_bytes as u64 || skip.newlines > limits.max_lines as u64
}

fn tail_limits(next: &mut impl FnMut() -> f64) -> OutputLimits {
    let max_bytes = 1 + pick(next, 40);
    let max_lines = 1 + pick(next, 5);
    OutputLimits { max_bytes, max_lines, retain: Retain::Tail }
}

fn push(buffer: &mut OutputBuffer, text: &str) {
    buffer.push(OutputChunk::Text(text), None).unwrap();
}

/// Feed random output to one buffer in full and to another through a reference skipper that holds undelivered text
/// and, when it flushes, may replace a prefix of it by counts if the rest exceeds the window.
fn check_seed(seed: u32) {
    let mut next = random(seed);
    let limits = tail_limits(&mut next);
    let mut full = OutputBuffer::new(limits);
    let mut skipping = OutputBuffer::new(limits);
    let mut pending = String::new();
    let flush =
        |pending: &mut String, skipping: &mut OutputBuffer, full: &mut OutputBuffer, next: &mut dyn FnMut() -> f64| {
            if pending.is_empty() {
                return;
            }
            // Code-point boundaries, so a cut never splits a character.
            let cuts: Vec<usize> = pending
                .char_indices()
                .map(|(index, character)| index + character.len_utf8())
                .filter(|&cut| exceeds_window(&pending[cut..], limits))
                .collect();
            if !cuts.is_empty() && next() < 0.7 {
                let cut = cuts[(next() * cuts.len() as f64).floor() as usize];
                skipping.push(OutputChunk::Text(&pending[cut..]), Some(measure(&pending[..cut]))).unwrap();
            } else {
                push(skipping, pending);
            }
            pending.clear();
            // Progress snapshots happen between deliveries and compact stored chunks.
            if next() < 0.5 {
                skipping.snapshot();
            }
            assert_eq!(skipping.snapshot(), full.snapshot(), "seed {seed}");
        };
    let chunks = pick(&mut next, 40);
    for _ in 0..chunks {
        let chunk = random_chunk(&mut next);
        push(&mut full, &chunk);
        if next() < 0.3 {
            full.snapshot();
        }
        pending.push_str(&chunk);
        if next() < 0.3 {
            flush(&mut pending, &mut skipping, &mut full, &mut next);
        }
    }
    flush(&mut pending, &mut skipping, &mut full, &mut next);
    full.end();
    skipping.end();
    assert_eq!(skipping.snapshot(), full.snapshot(), "seed {seed}");
}

// Progress commits snapshot at arbitrary moments; snapshots compact what is stored and must never change the window.
// A compaction to exactly the kept window lost the character that decides where a later window's first line starts.
#[test]
fn keeps_the_same_tail_whenever_progress_snapshots_happen() {
    for seed in 1..=3000 {
        let mut next = random(seed);
        let limits = tail_limits(&mut next);
        let mut plain = OutputBuffer::new(limits);
        let mut sampled = OutputBuffer::new(limits);
        let chunks = pick(&mut next, 30);
        for _ in 0..chunks {
            let chunk = random_chunk(&mut next);
            push(&mut plain, &chunk);
            push(&mut sampled, &chunk);
            if next() < 0.4 {
                sampled.snapshot();
            }
        }
        assert_eq!(sampled.snapshot(), plain.snapshot(), "seed {seed}");
    }
}

#[test]
fn matches_the_full_stream_for_every_legal_skip_pattern() {
    for seed in 1..=3000 {
        check_seed(seed);
    }
}

#[test]
fn counts_skipped_bytes_lines_and_the_final_newline_exactly() {
    let limits = OutputLimits { max_bytes: 1000, max_lines: 2, retain: Retain::Tail };
    let mut full = OutputBuffer::new(limits);
    let mut skipping = OutputBuffer::new(limits);
    // The skipped text ends without a newline, so its last line continues in the delivered text.
    let (omitted, kept) = ("one\ntwo\nthr", "ee\nfour\nfive\nsix");
    push(&mut full, &format!("{omitted}{kept}"));
    skipping.push(OutputChunk::Text(kept), Some(measure(omitted))).unwrap();
    let snapshot = skipping.snapshot();
    assert_eq!(snapshot, full.snapshot());
    assert_eq!((snapshot.text.as_str(), snapshot.dropped_bytes, snapshot.dropped_lines), ("five\nsix", 19, 4));
}

#[test]
fn refuses_skips_for_head_retention() {
    let mut buffer = OutputBuffer::new(OutputLimits { max_bytes: 10, max_lines: 2, retain: Retain::Head });
    let skip = ShellOutputSkip { bytes: 3, newlines: 1, ends_with_newline: true };
    let error = buffer.push(OutputChunk::Text("x\ny\nz\n"), Some(skip)).unwrap_err();
    assert!(error.to_string().contains("tail retention"), "{error}");
}
