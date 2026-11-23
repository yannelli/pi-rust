//! UTF-8 validation (`std::str::from_utf8(bytes).is_ok()`) and the length of `String::from_utf8_lossy(bytes)`, faster
//! than the methods the decoder used before wherever this build and this machine allow it. Two candidates:
//!
//! - a comparison kernel that the compiler turns into vector code, on x86_64 and aarch64;
//! - a shift DFA, one table load and one 32-bit shift per byte, on every target, which needs no vector code.
//!
//! The first long input times them on four kinds of text against `str::from_utf8` (for validation) and a
//! `utf8_chunks` count (for lengths). A candidate replaces that method only when it is no slower on each kind and at
//! least 10% faster on all of them together, so a build or a machine where a candidate is slow keeps the method it
//! had.

// pi-env-daemon compiles this file through `#[path]` without the rest of pi-durable: use only std paths here.
// `cargo xtask utf8-asm` fails when a release target compiles the kernel's loops to scalar code.

use std::hint::black_box;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Shorter inputs go to the standard library, which is faster on them.
const SHORT: usize = 64;

/// The length of a prefix of `bytes` that is valid UTF-8 and ends on a character boundary: `bytes.len()` exactly when
/// all of `bytes` is valid.
type Prefix = fn(&[u8]) -> usize;

/// Whether `bytes` are valid UTF-8: `std::str::from_utf8(bytes).is_ok()`.
pub fn is_utf8(bytes: &[u8]) -> bool {
    if bytes.len() < SHORT {
        return std::str::from_utf8(bytes).is_ok();
    }
    (choice().validate)(bytes) == bytes.len()
}

/// The byte length of `String::from_utf8_lossy(bytes)`.
pub fn lossy_len(bytes: &[u8]) -> usize {
    if bytes.is_ascii() {
        return bytes.len();
    }
    let valid = match choice().count {
        Some(prefix) if bytes.len() >= SHORT => prefix(bytes),
        _ => 0,
    };
    // A valid prefix ends a character, so the rest decodes on its own.
    valid + counted_len(&bytes[valid..])
}

fn std_prefix(bytes: &[u8]) -> usize {
    std::str::from_utf8(bytes).map_or_else(|error| error.valid_up_to(), |_| bytes.len())
}

/// The length of the lossy text by counting its valid runs and replacements, as the decoder did before.
fn counted_len(bytes: &[u8]) -> usize {
    bytes
        .utf8_chunks()
        .map(|chunk| {
            let replacement = if chunk.invalid().is_empty() { 0 } else { char::REPLACEMENT_CHARACTER.len_utf8() };
            chunk.valid().len() + replacement
        })
        .sum()
}

/// What this process uses, chosen at the first long input.
struct Choice {
    /// For `is_utf8`.
    validate: Prefix,
    /// For `lossy_len`, before counting the rest; `None` counts every input.
    count: Option<Prefix>,
}

fn choice() -> &'static Choice {
    static CHOICE: OnceLock<Choice> = OnceLock::new();
    CHOICE.get_or_init(choose)
}

fn candidates() -> Vec<Prefix> {
    let mut candidates: Vec<Prefix> = vec![dfa::valid_prefix];
    // Only these architectures have been checked for the kernel's vector code.
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    candidates.push(kernel::valid_prefix);
    candidates
}

fn choose() -> Choice {
    let samples = samples();
    let mut functions = candidates();
    let candidate_count = functions.len();
    functions.extend([std_prefix as Prefix, counted_len]);
    let times = fastest(&functions, &samples);
    let (std_times, counted_times) = (&times[candidate_count], &times[candidate_count + 1]);
    let rows = || functions.iter().copied().zip(&times);
    Choice {
        validate: better(rows().take(candidate_count), std_times, samples.len()).unwrap_or(std_prefix),
        // Lengths skip ASCII input before they validate, so they compare on the other kinds, and `std` competes.
        count: better(rows().take(candidate_count + 1), counted_times, samples.len() - 1),
    }
}

/// The fastest candidate that is no slower than `baseline` on each of the first `kinds` samples and at least 10%
/// faster on all of them together, so that timing noise alone does not replace the baseline.
fn better<'a>(
    candidates: impl Iterator<Item = (Prefix, &'a Vec<Duration>)>,
    baseline: &[Duration],
    kinds: usize,
) -> Option<Prefix> {
    let baseline = &baseline[..kinds];
    let total = |times: &[Duration]| times.iter().sum::<Duration>();
    candidates
        .map(|(candidate, times)| (candidate, &times[..kinds]))
        .filter(|(_, times)| {
            times.iter().zip(baseline).all(|(time, base)| time <= base)
                && total(times).as_nanos() * 10 <= total(baseline).as_nanos() * 9
        })
        .min_by_key(|(_, times)| total(times))
        .map(|(candidate, _)| candidate)
}

/// The fastest of three runs of each function on each sample, interleaved, so that a preempted run only ever makes
/// one side slower.
fn fastest(functions: &[Prefix], samples: &[Vec<u8>]) -> Vec<Vec<Duration>> {
    let mut fastest = vec![vec![Duration::MAX; samples.len()]; functions.len()];
    for _ in 0..3 {
        for (function, fastest) in functions.iter().zip(&mut fastest) {
            for (sample, fastest) in samples.iter().zip(fastest.iter_mut()) {
                let start = Instant::now();
                black_box(function(black_box(sample)));
                *fastest = (*fastest).min(start.elapsed());
            }
        }
    }
    fastest
}

/// About 16 KiB of each kind of text on which the candidates and the old methods rank differently: dense 2- and
/// 3-byte text (where both old methods are slowest), a repeated 4-byte character between ASCII words (where the count
/// is fastest), a log with a mark on some lines (where `str::from_utf8` skips most of the text a word at a time), and
/// ASCII (where it is fastest). The candidates' steps over a byte do not branch on its value, so they take about the
/// same time per byte on any non-ASCII text, and a candidate that wins on all four wins between them too. ASCII comes
/// last: lengths skip it. The other text is pseudo-random, as real text is, and all of it is whole characters, so no
/// candidate stops early.
fn samples() -> [Vec<u8>; 4] {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let dense = words(&mut rng, &["Привет ", "мир ", "файл", "日本語", "の", "テキスト", "。", "한국어 ", "텍스트 "]);
    // Repeated, unlike the others: the count is fastest when it can predict every character.
    let emoji = "emoji 😄".repeat(SAMPLE / 10).into_bytes();
    let mut log = String::new();
    while log.len() < SAMPLE {
        let mark = ["", "", "", "✓ ", "→ ", "🚀 ", "é "][rng.below(7)];
        let (minute, task, ms) = (rng.below(60), rng.below(100_000), rng.below(5000));
        log.push_str(&format!("2026-11-22T21:{minute:02}:17Z INFO {mark}task {task} finished in {ms} ms\n"));
    }
    let ascii =
        words(&mut rng, &["let ", "total ", "= ", "count ", "+ ", "1; ", "// ", "ok\n", "fn ", "main() ", "{ "]);
    [dense, emoji, log.into_bytes(), ascii]
}

const SAMPLE: usize = 16 * 1024;

fn words(rng: &mut Rng, list: &[&str]) -> Vec<u8> {
    let mut text = String::new();
    while text.len() < SAMPLE {
        text.push_str(list[rng.below(list.len())]);
    }
    text.into_bytes()
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

/// UTF-8 validation by a shift DFA: each byte's table row holds every state's next state, as the bit offset of that
/// state's own 5-bit field, so a step is one load and one shift. The offsets were found by search so that all nine
/// states fit in 32 bits, which keeps a step to one shift on 32-bit targets. The error state is 0, so it is never
/// left, and a stream needs no check until its end. Independent streams run interleaved to overlap their steps.
mod dfa {
    const ERR: u32 = 0;
    /// Between characters.
    const BGN: u32 = 6;
    /// Waiting for 1, 2, or 3 more continuation bytes.
    const CS1: u32 = 16;
    const CS2: u32 = 1;
    const CS3: u32 = 11;
    /// After e0, ed, f0, f4, whose next byte has a narrower range.
    const E0: u32 = 19;
    const ED: u32 = 25;
    const F0: u32 = 24;
    const F4: u32 = 30;
    const STATES: [u32; 9] = [ERR, BGN, CS1, CS2, CS3, E0, ED, F0, F4];

    /// Blocks of non-ASCII text run as two streams; a block with fewer non-ASCII bytes than 1/`SPARSE` of it runs as
    /// one stream that skips its ASCII 16-byte chunks.
    const BLOCK: usize = 1024;
    const SPARSE: usize = 8;

    const WORD: usize = size_of::<usize>();
    const ONES: usize = usize::MAX / 0xff;

    const fn next(state: u32, byte: u8) -> u32 {
        match byte {
            0x00..=0x7f if state == BGN => BGN,
            0x80..=0xbf => match state {
                CS1 => BGN,
                CS2 => CS1,
                CS3 => CS2,
                E0 if byte >= 0xa0 => CS1,
                ED if byte <= 0x9f => CS1,
                F0 if byte >= 0x90 => CS2,
                F4 if byte <= 0x8f => CS2,
                _ => ERR,
            },
            _ if state != BGN => ERR,
            0xc2..=0xdf => CS1,
            0xe0 => E0,
            0xe1..=0xec | 0xee..=0xef => CS2,
            0xed => ED,
            0xf0 => F0,
            0xf1..=0xf3 => CS3,
            0xf4 => F4,
            _ => ERR,
        }
    }

    const fn table() -> [u32; 256] {
        let mut table = [0; 256];
        let mut byte = 0;
        while byte < 256 {
            let mut index = 0;
            while index < STATES.len() {
                table[byte] |= next(STATES[index], byte as u8) << STATES[index];
                index += 1;
            }
            byte += 1;
        }
        table
    }

    static TABLE: [u32; 256] = table();

    /// The state after `byte`; only the low 5 bits of a state count.
    #[inline(always)]
    fn step(state: u32, byte: u8) -> u32 {
        TABLE[byte as usize].wrapping_shr(state)
    }

    /// Whether every state's field of every row reads back that state's next state.
    #[cfg(test)]
    pub(super) fn table_is_exact() -> bool {
        (0..=255).all(|byte| STATES.iter().all(|&state| step(state, byte) & 31 == next(state, byte)))
    }

    fn word(bytes: &[u8]) -> usize {
        usize::from_le_bytes(bytes.try_into().expect("a word"))
    }

    /// Bytes of 0x80 or more, a machine word at a time.
    fn non_ascii(bytes: &[u8]) -> usize {
        let mut total = 0;
        // Each byte lane counts to at most 255 before the lanes are added up.
        for run in bytes.chunks(255 * WORD) {
            let mut words = run.chunks_exact(WORD);
            let mut lanes = 0;
            for word in &mut words {
                lanes += (self::word(word) >> 7) & ONES;
            }
            let low = usize::MAX / 0xffff * 0xff;
            let pairs = (lanes & low) + ((lanes >> 8) & low);
            total += pairs.wrapping_mul(usize::MAX / 0xffff) >> (usize::BITS - 16);
            total += words.remainder().iter().filter(|byte| **byte >= 0x80).count();
        }
        total
    }

    /// Runs `bytes` from `state` as two streams split at a character start: the state after them, or ERR.
    fn run_dense(state: u32, bytes: &[u8]) -> u32 {
        let middle = bytes.len() / 2;
        let Some(cut) = (middle..bytes.len().min(middle + 4)).find(|&index| (bytes[index] as i8) >= -64) else {
            // Four continuation bytes in a row are invalid; fewer than four left run as one stream.
            return if middle + 4 > bytes.len() {
                bytes.iter().fold(state, |state, &byte| step(state, byte))
            } else {
                ERR
            };
        };
        let (first, second) = bytes.split_at(cut);
        let shared = first.len().min(second.len());
        let (mut one, mut two) = (state, BGN);
        for (&a, &b) in first[..shared].iter().zip(&second[..shared]) {
            one = step(one, a);
            two = step(two, b);
        }
        one = first[shared..].iter().fold(one, |state, &byte| step(state, byte));
        two = second[shared..].iter().fold(two, |state, &byte| step(state, byte));
        if one & 31 == BGN { two } else { ERR }
    }

    /// Runs `bytes` from `state` as one stream that skips 16-byte chunks of ASCII between characters.
    fn run_sparse(mut state: u32, bytes: &[u8]) -> u32 {
        let mut chunks = bytes.chunks_exact(16);
        for chunk in &mut chunks {
            let any = chunk.chunks_exact(WORD).fold(0, |any, bytes| any | word(bytes));
            if state & 31 == BGN && any & (ONES << 7) == 0 {
                continue;
            }
            for &byte in chunk {
                state = step(state, byte);
            }
        }
        chunks.remainder().iter().fold(state, |state, &byte| step(state, byte))
    }

    /// The end of the last block that ended on a character boundary with no error before it.
    pub(super) fn valid_prefix(bytes: &[u8]) -> usize {
        let (mut state, mut valid, mut end) = (BGN, 0, 0);
        for block in bytes.chunks(BLOCK) {
            let count = non_ascii(block);
            if count > 0 || state & 31 != BGN {
                state = if count * SPARSE < block.len() { run_sparse(state, block) } else { run_dense(state, block) };
                if state & 31 == ERR {
                    return valid;
                }
            }
            end += block.len();
            if state & 31 == BGN {
                valid = end;
            }
        }
        valid
    }
}

/// UTF-8 validation from byte comparisons that the compiler turns into vector code on these architectures.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
mod kernel {
    /// One exit test and one choice of rules per this many bytes.
    const BLOCK: usize = 1024;

    /// The bytes before the first block that breaks a rule, cut back to a character start that the checks accepted.
    pub(super) fn valid_prefix(bytes: &[u8]) -> usize {
        let n = bytes.len();
        if n < 4 {
            return super::std_prefix(bytes);
        }
        let starts_badly = breaks::<4>(bytes[0], 0, 0, 0)
            | breaks::<4>(bytes[1], bytes[0], 0, 0)
            | breaks::<4>(bytes[2], bytes[1], bytes[0], 0)
            | bytes[..3].iter().any(|byte| *byte > 0xf4);
        if starts_badly {
            return 0;
        }
        // Byte `c[i]` follows `p1[i]`, `p2[i]`, and `p3[i]`.
        let (c, p1, p2, p3) = (&bytes[3..], &bytes[2..n - 1], &bytes[1..n - 2], &bytes[..n - 3]);
        let blocks = c.chunks(BLOCK).zip(p1.chunks(BLOCK)).zip(p2.chunks(BLOCK)).zip(p3.chunks(BLOCK));
        for (index, (((c, p1), p2), p3)) in blocks.enumerate() {
            // The largest byte of the block and of the three bytes before it says which rules can fire.
            let largest = c.iter().fold(p1[0].max(p2[0]).max(p3[0]), |largest, byte| largest.max(*byte));
            let broken = match largest {
                0..0x80 => false,
                0x80..0xe0 => block_breaks::<2>(c, p1, p2, p3),
                0xe0..0xf0 => block_breaks::<3>(c, p1, p2, p3),
                0xf0..0xf5 => block_breaks::<4>(c, p1, p2, p3),
                // f5..ff is in no character. The bytes before the block passed, so it is in the block.
                _ => true,
            };
            if broken {
                return last_start_before(bytes, 3 + index * BLOCK);
            }
        }
        let ends_inside = (bytes[n - 1] >= 0xc0) | (bytes[n - 2] >= 0xe0) | (bytes[n - 3] >= 0xf0);
        if ends_inside { last_start_before(bytes, n) } else { n }
    }

    /// The last character start before `checked`, where every byte before `checked` passed. The character before it
    /// is complete, because the start passed as a byte that needs no continuation.
    fn last_start_before(bytes: &[u8], checked: usize) -> usize {
        (checked.saturating_sub(4)..checked).rev().find(|&index| (bytes[index] as i8) >= -64).unwrap_or(0)
    }

    /// Whether a byte of the block breaks a rule, when no byte leads a character longer than `WIDTH` bytes. Not
    /// inlined, so each rule set is a function of its own that `cargo xtask utf8-asm` can find; the call per block
    /// costs nothing measurable.
    #[inline(never)]
    fn block_breaks<const WIDTH: usize>(c: &[u8], p1: &[u8], p2: &[u8], p3: &[u8]) -> bool {
        let mut broken = false;
        for (((c, p1), p2), p3) in c.iter().zip(p1).zip(p2).zip(p3) {
            broken |= breaks::<WIDTH>(*c, *p1, *p2, *p3);
        }
        broken
    }

    /// Whether byte `c` cannot follow `p3 p2 p1` in valid UTF-8, when none of them leads a character longer than
    /// `WIDTH` bytes and none is f5..ff. A bad lead byte (c0, c1) is reported on the byte after it.
    #[inline(always)]
    fn breaks<const WIDTH: usize>(c: u8, p1: u8, p2: u8, p3: u8) -> bool {
        let continuation = (c as i8) < -64;
        let mut demand = p1;
        if WIDTH >= 3 {
            demand = demand.max(p2.saturating_sub(0x20));
        }
        if WIDTH >= 4 {
            demand = demand.max(p3.saturating_sub(0x30));
        }
        let mut broken = (continuation ^ (demand >= 0xc0)) | ((p1 & 0xfe) == 0xc0);
        if WIDTH >= 3 {
            // e0 takes a0..bf next and ed takes 80..9f.
            broken |= p1 == if (c as i8) < -96 { 0xe0 } else { 0xed };
        }
        if WIDTH >= 4 {
            // f0 takes 90..bf next and f4 takes 80..8f. A block with f5..ff does not get here.
            broken |= p1 == if (c as i8) < -112 { 0xf0 } else { 0xf4 };
        }
        broken
    }
}

#[cfg(test)]
#[path = "utf8_tests.rs"]
mod tests;
