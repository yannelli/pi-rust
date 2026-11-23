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

/// UTF-8 validation from byte comparisons that the compiler turns into vector code on these architectures. The input
/// runs in 1 KiB blocks. A block of ASCII costs one pass for its largest byte. A block with a few parts that are not
/// ASCII runs the rules on those parts only. A block full of other text runs only the rules that its largest and
/// smallest lead bytes allow; the smallest lead byte is known after the rules run, so the block before picks them and
/// the block's own bounds confirm them.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
mod kernel {
    /// One exit test and one choice of rules per this many bytes.
    const BLOCK: usize = 1024;
    /// A block has 16 parts. A block where few parts have a byte over 7f runs the rules on those parts only.
    const PART: usize = 64;
    /// Three- and four-byte text does that in pieces of a part.
    const PIECE: usize = 16;
    /// The pass for the largest byte keeps this many bytes per part: the largest byte of every fourth position.
    const LANES: usize = PART / 4;
    const HIGH: u64 = 0x8080_8080_8080_8080;
    /// The most parts with a byte over 7f that still take the short way, for two-, three- and four-byte text.
    const FEW: [u32; 3] = [6, 13, 13];
    /// The most blocks that follow a crowded block without a look at their parts.
    const AGAIN: u32 = 15;

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
        // `crowded`: blocks still to take as crowded. `run`: crowded blocks in a row. `width`: the widest character of
        // the last crowded block.
        let (mut crowded, mut run, mut width) = (0u32, 0u32, 0usize);
        // The smallest lead byte XOR c0 of the last crowded block of three- or four-byte text, which picks the rules
        // for the next one. 02 picks the rules that fit any block.
        let mut last_lowest = 0x02;
        for (index, (((c, p1), p2), p3)) in blocks.enumerate() {
            let before = p1[0].max(p2[0]).max(p3[0]);
            let largest_of = |bytes: &[u8]| bytes.iter().fold(before, |largest, byte| largest.max(*byte));
            let full = c.len() == BLOCK;
            let mut plain = !full;
            if full && crowded > 0 {
                crowded -= 1;
                if width > 2 {
                    plain = true;
                } else {
                    // The two-byte rules can flag every lead byte of a longer character, so a block that passes them
                    // needs no pass for its largest byte. A flag only sends the block the long way.
                    if !(p2[0].max(p3[0]) > 0xdf || two_byte_breaks(c, p1)) {
                        continue;
                    }
                    (crowded, run) = (0, 0);
                }
            }
            // The parts with a byte over 7f, when the pass looked at parts.
            let mut busy = None;
            let largest = if plain {
                largest_of(c)
            } else {
                // Per part, the largest byte of each of 16 lanes: the vector that the pass has before it reduces it
                // to one byte. A block of ASCII still costs one reduction for its 16 parts.
                let mut lanes = [[0u8; LANES]; BLOCK / PART];
                let mut all = [0u8; LANES];
                for (lanes, part) in lanes.iter_mut().zip(c.chunks_exact(PART)) {
                    for lane in 0..LANES {
                        lanes[lane] =
                            part[lane].max(part[LANES + lane]).max(part[2 * LANES + lane]).max(part[3 * LANES + lane]);
                        all[lane] = all[lane].max(lanes[lane]);
                    }
                }
                let largest = largest_of(&all);
                if largest >= 0x80 {
                    busy = Some(survey(&lanes));
                }
                largest
            };
            let wide = match largest {
                0..0x80 => {
                    (crowded, run) = (0, 0);
                    continue;
                }
                0x80..0xe0 => 2,
                0xe0..0xf0 => 3,
                0xf0..0xf5 => 4,
                // f5..ff is in no character. The bytes before the block passed, so it is in the block.
                _ => return last_start_before(bytes, 3 + index * BLOCK),
            };
            let broken = match busy {
                Some(mask) if mask.count_ones() <= FEW[wide - 2] => {
                    run = 0;
                    match wide {
                        2 => parts_break(mask, c, p1, p2, p3),
                        3 => pieces_break::<3>(mask, c, p1, p2, p3),
                        _ => pieces_break::<4>(mask, c, p1, p2, p3),
                    }
                }
                _ => {
                    // The next blocks are likely crowded too. Two-byte text then takes the next blocks on trust: the
                    // second crowded block in a row vouches for 2 blocks, the third for 4, and so on to `AGAIN`.
                    if busy.is_some() {
                        run += 1;
                        width = wide;
                        crowded = if wide > 2 { AGAIN } else { ((1 << run.min(5)) >> 1 & !1).min(AGAIN) };
                    }
                    if wide == 2 {
                        two_byte_breaks(c, p1)
                    } else {
                        let broken;
                        (broken, last_lowest) = crowded_breaks(largest, last_lowest, c, p1, p2, p3);
                        broken
                    }
                }
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

    /// Whether a block full of three- or four-byte text breaks a rule, and the block's smallest lead byte XOR c0. XOR c0
    /// maps every lead byte (c0..ff) to 00..3f and every other byte above 3f. The block's largest and smallest lead
    /// bytes say which rules can apply: c0, c1, and f5..ff are in no character; when every lead byte starts a character
    /// of the same length, a lead byte one, two, or three bytes back needs a continuation byte with no test of which lead
    /// it is; and the tests of the byte after e0, ed, f0, or f4 run only when that lead byte can be in the block.
    ///
    /// The largest byte is known; the smallest lead byte comes out of the rules' own loop. So the rules are picked with
    /// `guess`, the smallest lead byte of the last such block, and the result counts only when those rules fit this
    /// block too, or fit any block. Otherwise the block runs again with the rules that fit it.
    #[inline(never)]
    fn crowded_breaks(largest: u8, guess: u8, c: &[u8], p1: &[u8], p2: &[u8], p3: &[u8]) -> (bool, u8) {
        let guessed = rule_set(largest, guess);
        let (broken, lowest) = run_rules(guessed, c, p1, p2, p3);
        // The loop saw each byte of the block as `p1` but the last, and the first of the three bytes before it.
        let lowest = [c[c.len() - 1], p2[0], p3[0]].iter().fold(lowest, |lowest, byte| lowest.min(byte ^ 0xc0));
        if lowest < 0x02 {
            return (true, lowest);
        }
        let fits = rule_set(largest, lowest);
        if fits == guessed || FIT_ANY.contains(&guessed) {
            return (broken, lowest);
        }
        (run_rules(fits, c, p1, p2, p3).0, lowest)
    }

    /// The rule sets of [`rule_set`] that fit any block with their largest byte: they test every lead byte.
    const FIT_ANY: [u8; 3] = [3, 6, 7];

    /// The rule set for a block of three- or four-byte text with this largest byte and smallest lead byte XOR c0.
    fn rule_set(largest: u8, lowest: u8) -> u8 {
        match largest {
            0xe0..0xed if lowest > 0x20 => 0,
            0xed..0xf0 if lowest > 0x20 => 1,
            0xe0..0xf0 if lowest == 0x20 => 2,
            0xe0..0xf0 => 3,
            0xf0..0xf4 if lowest >= 0x30 => 4,
            0xf4 if lowest >= 0x30 => 5,
            0xf0..0xf4 => 6,
            _ => 7,
        }
    }

    fn run_rules(set: u8, c: &[u8], p1: &[u8], p2: &[u8], p3: &[u8]) -> (bool, u8) {
        match set {
            0 => rules::<3, 3, false, false, false, false>(c, p1, p2, p3),
            1 => rules::<3, 3, false, true, false, false>(c, p1, p2, p3),
            2 => rules::<3, 3, true, true, false, false>(c, p1, p2, p3),
            3 => rules::<2, 3, true, true, false, false>(c, p1, p2, p3),
            4 => rules::<4, 4, false, false, true, false>(c, p1, p2, p3),
            5 => rules::<4, 4, false, false, true, true>(c, p1, p2, p3),
            6 => rules::<2, 4, true, true, true, false>(c, p1, p2, p3),
            _ => rules::<2, 4, true, true, true, true>(c, p1, p2, p3),
        }
    }

    /// The rules for a block whose lead bytes start characters of `SHORTEST` to `LONGEST` bytes and are none of f5..ff,
    /// with the smallest `p1` XOR c0, from which the caller finds c0 and c1. `E0`, `ED`, `F0`, `F4`: whether that lead
    /// byte can be in the block. Not inlined, so each rule set is a function of its own that `cargo xtask utf8-asm` can
    /// find.
    #[inline(never)]
    fn rules<
        const SHORTEST: usize,
        const LONGEST: usize,
        const E0: bool,
        const ED: bool,
        const F0: bool,
        const F4: bool,
    >(
        c: &[u8],
        p1: &[u8],
        p2: &[u8],
        p3: &[u8],
    ) -> (bool, u8) {
        // x86_64 has no cheap select, so there the tests of both e0 and ed, and of both f0 and f4, keep running minimums
        // in place of a select and a compare per byte (forms found by a search over short programs).
        const X86: bool = cfg!(target_arch = "x86_64");
        let (mut broken, mut lowest, mut e0_ed, mut f0_f4) = (false, 0xff_u8, 0xff_u8, 0xff_u8);
        for (((c, p1), p2), p3) in c.iter().zip(p1).zip(p2).zip(p3) {
            let (c, p1) = (*c, *p1);
            lowest = lowest.min(p1 ^ 0xc0);
            // A lead byte that is still owed a continuation byte here.
            let mut demand = p1;
            if LONGEST >= 3 {
                demand = demand.max(if SHORTEST >= 3 { *p2 } else { p2.saturating_sub(0x20) });
            }
            if LONGEST >= 4 {
                demand = demand.max(if SHORTEST >= 4 { *p3 } else { p3.saturating_sub(0x30) });
            }
            broken |= ((c as i8) < -64) ^ (demand >= 0xc0);
            // e0 takes a0..bf next and ed takes 80..9f; f0 takes 90..bf next and f4 takes 80..8f.
            if E0 && ED && X86 {
                // `(p1 + 19) ^ 06` is ff for e0 and 00 for ed, and neither for any other byte; XOR with ff when `c` is
                // 80..9f gives 00 exactly when the pair is wrong.
                let low = if (c as i8) < -96 { 0xff } else { 0x00 };
                e0_ed = e0_ed.min((p1.wrapping_add(0x19) ^ 0x06) ^ low);
            } else if E0 && ED {
                broken |= p1 == if (c as i8) < -96 { 0xe0 } else { 0xed };
            } else if ED {
                broken |= (p1 == 0xed) & ((c as i8) >= -96);
            }
            if F0 && F4 && X86 {
                // `p1 ^ f4` is 0 for f4, 4 for f0, and at least 5 for any other byte here; minus 1 when `c` is 80..8f
                // it is under 4 exactly when the pair is wrong.
                let low = if (c as i8) < -112 { 0xff } else { 0x00 };
                f0_f4 = f0_f4.min((p1 ^ 0xf4).wrapping_add(low));
            } else if F0 && F4 {
                broken |= p1 == if (c as i8) < -112 { 0xf0 } else { 0xf4 };
            } else if F0 {
                broken |= (p1 == 0xf0) & ((c as i8) < -112);
            }
        }
        (broken | (e0_ed == 0) | (f0_f4 < 4), lowest)
    }

    /// The rules for a block of two-byte text, also when it is taken on trust: a lead byte one byte back needs a
    /// continuation byte, and `p1 ^ e0` is 00..21 for c0, c1, and e0..ff, so the block's smallest such number flags the
    /// lead bytes in no character and every lead byte of a longer character, with one operation per byte. The two bytes
    /// before the first `p1` are tested by the caller, or passed in the block before.
    #[inline(never)]
    fn two_byte_breaks(c: &[u8], p1: &[u8]) -> bool {
        let (mut broken, mut lowest) = (false, 0xff_u8);
        for (c, p1) in c.iter().zip(p1) {
            broken |= ((*c as i8) < -64) ^ (*p1 >= 0xc0);
            lowest = lowest.min(p1 ^ 0xe0);
        }
        broken | (lowest <= 0x21)
    }

    /// Whether byte `c` cannot follow `p3 p2 p1` in valid UTF-8, when none of them leads a character longer than
    /// `WIDTH` bytes and none is f5..ff. A bad lead byte (c0, c1) is reported on the byte after it.
    #[inline(always)]
    fn breaks<const WIDTH: usize>(c: u8, p1: u8, p2: u8, p3: u8) -> bool {
        let mut demand = p1;
        if WIDTH >= 3 {
            demand = demand.max(p2.saturating_sub(0x20));
        }
        if WIDTH >= 4 {
            demand = demand.max(p3.saturating_sub(0x30));
        }
        let mut broken = (((c as i8) < -64) ^ (demand >= 0xc0)) | ((p1 & 0xfe) == 0xc0);
        if WIDTH >= 3 {
            // e0 takes a0..bf next and ed takes 80..9f.
            broken |= p1 == if (c as i8) < -96 { 0xe0 } else { 0xed };
        }
        if WIDTH >= 4 {
            // f0 takes 90..bf next and f4 takes 80..8f.
            broken |= p1 == if (c as i8) < -112 { 0xf0 } else { 0xf4 };
        }
        broken
    }

    /// One bit per part with a byte over 7f. This runs once per block that is not ASCII.
    #[inline(never)]
    fn survey(lanes: &[[u8; LANES]; BLOCK / PART]) -> u32 {
        let mut mask = 0;
        for (part, lanes) in lanes.iter().enumerate() {
            let (low, high) = lanes.split_at(8);
            let both = u64::from_le_bytes(low.try_into().expect("8 bytes"))
                | u64::from_le_bytes(high.try_into().expect("8 bytes"));
            mask |= u32::from(both & HIGH != 0) << part;
        }
        mask
    }

    /// Whether a character that began in the three bytes before `c[at]` needs `c[at]`. When `c[at]` begins a run of
    /// ASCII, this is the only way the run can break a rule.
    #[inline(always)]
    fn hangs(at: usize, p1: &[u8], p2: &[u8], p3: &[u8]) -> bool {
        (p1[at] >= 0xc0) | (p2[at] >= 0xe0) | (p3[at] >= 0xf0)
    }

    #[inline(always)]
    fn piece_breaks<const WIDTH: usize, const LEN: usize>(
        at: usize,
        c: &[u8],
        p1: &[u8],
        p2: &[u8],
        p3: &[u8],
    ) -> bool {
        let (c, p1, p2, p3) = (&c[at..at + LEN], &p1[at..at + LEN], &p2[at..at + LEN], &p3[at..at + LEN]);
        let mut broken = false;
        for index in 0..LEN {
            broken |= breaks::<WIDTH>(c[index], p1[index], p2[index], p3[index]);
        }
        broken
    }

    /// A full block of two-byte text: the rules on the 64-byte parts in `mask`, and a look at the three bytes before
    /// each other part that follows one of them. The parts come from the bit mask, so the only branch that depends on
    /// the text is the end of the loop.
    #[inline(never)]
    fn parts_break(mask: u32, c: &[u8], p1: &[u8], p2: &[u8], p3: &[u8]) -> bool {
        let (c, p1, p2, p3) = (&c[..BLOCK], &p1[..BLOCK], &p2[..BLOCK], &p3[..BLOCK]);
        let mut broken = (mask & 1 == 0) & hangs(0, p1, p2, p3);
        let mut rest = mask;
        while rest != 0 {
            let part = rest.trailing_zeros() as usize;
            rest &= rest - 1;
            let at = part * PART;
            broken |= piece_breaks::<2, PART>(at, c, p1, p2, p3);
            // The next block looks at its own start.
            let next = (at + PART).min(BLOCK - 1);
            broken |= (part < BLOCK / PART - 1) & (mask >> (part + 1) & 1 == 0) & hangs(next, p1, p2, p3);
        }
        broken
    }

    /// The same for three- and four-byte text, in the 16-byte pieces that have a byte over 7f.
    #[inline(never)]
    fn pieces_break<const WIDTH: usize>(mask: u32, c: &[u8], p1: &[u8], p2: &[u8], p3: &[u8]) -> bool {
        const PER_PART: usize = PART / PIECE;
        const LAST: usize = BLOCK / PIECE - 1;
        let (c, p1, p2, p3) = (&c[..BLOCK], &p1[..BLOCK], &p2[..BLOCK], &p3[..BLOCK]);
        let mut pieces = 0u64;
        let mut rest = mask;
        while rest != 0 {
            let part = rest.trailing_zeros() as usize;
            rest &= rest - 1;
            for piece in 0..PER_PART {
                let at = part * PART + piece * PIECE;
                let words = c[at..at + PIECE].chunks_exact(8);
                let high = words.fold(0, |all, word| all | u64::from_le_bytes(word.try_into().expect("8 bytes")));
                pieces |= u64::from(high & HIGH != 0) << (part * PER_PART + piece);
            }
        }
        let mut broken = (pieces & 1 == 0) & hangs(0, p1, p2, p3);
        let mut rest = pieces;
        while rest != 0 {
            let piece = rest.trailing_zeros() as usize;
            rest &= rest - 1;
            let at = piece * PIECE;
            broken |= piece_breaks::<WIDTH, PIECE>(at, c, p1, p2, p3);
            let next = (at + PIECE).min(BLOCK - 1);
            broken |= (piece < LAST) & (pieces >> ((piece + 1) & LAST) & 1 == 0) & hangs(next, p1, p2, p3);
        }
        broken
    }
}

#[cfg(test)]
#[path = "utf8_tests.rs"]
mod tests;
