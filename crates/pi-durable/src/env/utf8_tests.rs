//! The candidates against `std::str::from_utf8`. The tests call each candidate itself, because which one `is_utf8` uses
//! depends on the build and the machine.

use std::hint::black_box;
use std::time::Instant;

use super::{Prefix, choice, counted_len, dfa, is_utf8, lossy_len, std_prefix};

/// Both ends of every byte class that UTF-8 or a candidate's rules tell apart. A byte between two members of a class
/// behaves like them in every validator.
const ALPHABET: [u8; 27] = [
    0x00, 0x0a, 0x41, 0x7f, 0x80, 0x8f, 0x90, 0x9f, 0xa0, 0xbf, 0xc0, 0xc1, 0xc2, 0xdf, 0xe0, 0xe1, 0xec, 0xed, 0xee,
    0xef, 0xf0, 0xf1, 0xf3, 0xf4, 0xf5, 0xfb, 0xff,
];

/// Text of each character width: the widest lead byte in a block selects the kernel's rules, and the share of
/// non-ASCII bytes selects how the DFA runs the block.
const FILLS: [&str; 4] = ["a", "я", "日", "😀"];

/// More fills for the kernel's other paths: the lead bytes ed and f4 (rules with more tests), and one character of
/// each width among ASCII at several distances (blocks with few parts that are not ASCII).
const MORE_FILLS: [&str; 6] = [
    "한",
    "\u{100000}",
    "0123456789abcdefghijklmnopqrstuvwxyz ok é",
    "0123456789abcdefghijklmnopqrstuvwxyz 0123456789abcdefghijklmnopqrstuvwxyz 0123456789abcdefghijklmnopqrstuvwxyz 😀",
    "0123456789abcdef日",
    "0123456789abcdefghijklmnopqrstuvwxyz 0123456789abcdefghijklmnopqrstuvwxyz 0123456789abcdefghijklmnopqrstuvwxyz \
     0123456789abcdefghijklmnopqrstuvwxyz 0123456789abcdefghijklmnopqrstuvwxyz 0123456789abcdefghijklmnopqrstuvwxyz \
     0123456789abcdefghijklmnopqrstuvwxyz 0123456789abcdefghijklmnopqrstuvwxyz \u{10ffff}",
];

fn validators() -> Vec<(&'static str, Prefix)> {
    let mut validators: Vec<(&'static str, Prefix)> = vec![("dfa", dfa::valid_prefix)];
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    validators.push(("kernel", super::kernel::valid_prefix));
    validators
}

/// Each validator returns a valid prefix that ends a character, all of `bytes` exactly when std accepts them.
fn agrees(bytes: &[u8]) {
    let valid = std::str::from_utf8(bytes).is_ok();
    for (name, validator) in validators() {
        let prefix = validator(bytes);
        assert!(prefix <= bytes.len() && std::str::from_utf8(&bytes[..prefix]).is_ok(), "{name} {prefix} {bytes:02x?}");
        assert_eq!(prefix == bytes.len(), valid, "{name} {bytes:02x?}");
    }
}

/// Every sequence of `length` bytes from `alphabet`.
fn sequences(alphabet: &[u8], length: u32) -> impl Iterator<Item = Vec<u8>> + '_ {
    (0..alphabet.len().pow(length)).map(move |mut code| {
        (0..length)
            .map(|_| {
                let byte = alphabet[code % alphabet.len()];
                code /= alphabet.len();
                byte
            })
            .collect()
    })
}

/// `fill` repeated to at least three bytes, so that a sequence after it is past the kernel's scalar check of the first
/// three bytes.
fn lead(fill: &str) -> Vec<u8> {
    fill.repeat(3usize.div_ceil(fill.len())).into_bytes()
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

/// Runs of each width, cut at character starts, three quarters of them damaged.
fn damaged_pieces(count: usize) -> impl Iterator<Item = Vec<u8>> {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut text = Vec::new();
    while text.len() < 16 * 1024 {
        text.extend(FILLS[rng.below(FILLS.len())].repeat(1 + rng.below(600)).bytes());
    }
    (0..count).map(move |_| {
        let mut start = rng.below(text.len() - 5000);
        let mut end = start + 64 + rng.below(4000);
        while (text[start] as i8) < -64 {
            start += 1;
        }
        while (text[end] as i8) < -64 {
            end -= 1;
        }
        let mut bytes = text[start..end].to_vec();
        let at = rng.below(bytes.len());
        match rng.below(4) {
            0 => {}
            1 => bytes[at] = ALPHABET[rng.below(ALPHABET.len())],
            2 => {
                bytes.remove(at);
            }
            _ => bytes.insert(at, ALPHABET[rng.below(ALPHABET.len())]),
        }
        bytes
    })
}

#[test]
fn every_dfa_row_reads_back_each_next_state() {
    assert!(dfa::table_is_exact());
}

#[test]
fn sequences_of_up_to_three_bytes_match_std() {
    for length in 0..=3 {
        for sequence in sequences(&ALPHABET, length) {
            agrees(&sequence);
            for fill in FILLS {
                let (lead, fill) = (lead(fill), fill.as_bytes());
                agrees(&[lead.as_slice(), &sequence, fill].concat());
                agrees(&[lead.as_slice(), &sequence].concat());
                agrees(&[sequence.as_slice(), fill].concat());
            }
        }
    }
}

#[test]
fn sequences_of_four_and_five_bytes_match_std() {
    // One byte of each class keeps five-byte sequences to a size an unoptimized test runs in a second.
    let classes = [0x41, 0x80, 0x90, 0xa0, 0xc0, 0xc2, 0xe0, 0xe1, 0xed, 0xee, 0xf0, 0xf1, 0xf4, 0xf5];
    for (alphabet, length) in [(&ALPHABET[..], 4), (&classes[..], 5)] {
        for sequence in sequences(alphabet, length) {
            // ASCII lets the sequence select the rules; four-byte text selects the longest ones.
            for fill in ["a", "😀"] {
                agrees(&[lead(fill).as_slice(), &sequence, fill.as_bytes()].concat());
            }
        }
    }
}

#[test]
fn sequences_across_block_and_stream_edges_match_std() {
    // The kernel's second block starts at byte 1027 and reads the three bytes before it. The DFA cuts its first block
    // in two near byte 512 and starts its second block at byte 1024.
    for fill in FILLS {
        // Unoptimized, a block of wider text takes longer, so wider text gets sequences of up to two bytes.
        let longest = if fill.len() == 1 { 3 } else { 2 };
        for start in (510..=514).chain(1022..=1027) {
            let mut prefix = vec![b'a'; start % fill.len()];
            prefix.extend(fill.repeat(start / fill.len()).bytes());
            for length in 1..=longest {
                for sequence in sequences(&ALPHABET, length) {
                    agrees(&[prefix.as_slice(), &sequence, fill.as_bytes()].concat());
                }
            }
        }
    }
}

#[test]
fn sequences_in_crowded_sparse_and_trusted_blocks_match_std() {
    let alphabet = &ALPHABET[..];
    for fill in FILLS.iter().chain(&MORE_FILLS) {
        // Inside the second block, at its start, before a part edge, and after a piece edge; then after 8 KiB of the
        // fill, where blocks of crowded text take the rules of the blocks before them.
        for start in [1500, 1027, 1087, 1073, 8 * 1024 + 500] {
            let mut prefix = vec![b'a'; start % fill.len()];
            prefix.extend(fill.repeat(start / fill.len()).bytes());
            for length in 1..=2 {
                for sequence in sequences(alphabet, length) {
                    agrees(&[prefix.as_slice(), &sequence, fill.as_bytes()].concat());
                }
            }
        }
    }
}

#[test]
fn sequences_cut_at_a_block_edge_before_a_full_block_match_std() {
    // A character cut at the end of a block is finished, or not, by the next block, which takes the sparse path when it
    // is ASCII or nearly so.
    for fill in FILLS.iter().chain(&MORE_FILLS[2..4]) {
        for start in 1025..=1027 {
            let mut prefix = vec![b'a'; start % fill.len()];
            prefix.extend(fill.repeat(start / fill.len()).bytes());
            for tail in ["x", "0123456789abcdef日"] {
                let tail = tail.repeat(1030 / tail.len());
                for length in 1..=2 {
                    for sequence in sequences(&ALPHABET, length) {
                        agrees(&[prefix.as_slice(), &sequence, tail.as_bytes()].concat());
                    }
                }
            }
        }
    }
}

/// Pairs of text whose lead bytes allow different rule sets, so that the rules picked from the first text's blocks do
/// not fit the second's.
const SWITCHES: [(&str, &str); 10] = [
    ("日", "한"),
    ("日", "\u{800}"),
    ("日", "я日"),
    ("한", "日"),
    ("я日", "日"),
    ("😀", "日😀"),
    ("😀", "\u{10ffff}"),
    ("\u{10ffff}", "😀"),
    ("日😀", "😀"),
    ("😀", "\u{10000}"),
];

#[test]
fn rules_picked_from_the_block_before_match_std() {
    for (first, second) in SWITCHES {
        let prefix = first.repeat(2100 / first.len()).into_bytes();
        let next = second.repeat(2400 / second.len()).into_bytes();
        // Inside the first block of the second text, and in the block after it.
        for at in [600, 1500] {
            let at = (0..=4).map(|back| at - back).find(|&at| (next[at] as i8) >= -64).unwrap_or(at);
            for length in 1..=2 {
                for sequence in sequences(&ALPHABET, length) {
                    agrees(&[prefix.as_slice(), &next[..at], &sequence, &next[at..]].concat());
                }
            }
        }
    }
}

#[test]
fn damaged_mixed_text_over_many_blocks_matches_std() {
    for bytes in damaged_pieces(1000) {
        agrees(&bytes);
    }
}

#[test]
fn sparse_text_with_characters_across_chunk_edges_matches_std() {
    // A few characters in ASCII run as one stream over 16-byte chunks; put each width across every chunk offset.
    for fill in &FILLS[1..] {
        for at in 0..32 {
            let mut bytes = vec![b'x'; 2048];
            bytes.splice(at..at, fill.bytes());
            bytes.splice(1500 + at..1500 + at, fill.bytes());
            agrees(&bytes);
            bytes.truncate(at + 1);
            agrees(&bytes);
        }
    }
}

#[test]
fn is_utf8_and_lossy_len_match_std_through_the_choice() {
    for bytes in damaged_pieces(300) {
        assert_eq!(is_utf8(&bytes), std::str::from_utf8(&bytes).is_ok(), "{bytes:02x?}");
        assert_eq!(lossy_len(&bytes), String::from_utf8_lossy(&bytes).len(), "{bytes:02x?}");
    }
    for fill in FILLS {
        let text = fill.repeat(100);
        for length in 56..=72 {
            let mut bytes = text.as_bytes()[..length].to_vec();
            assert_eq!(is_utf8(&bytes), std::str::from_utf8(&bytes).is_ok(), "{bytes:02x?}");
            bytes[length / 2] = 0xff;
            assert!(!is_utf8(&bytes), "{bytes:02x?}");
            assert_eq!(lossy_len(&bytes), String::from_utf8_lossy(&bytes).len(), "{bytes:02x?}");
        }
    }
}

/// About 30 million inputs, too many for an unoptimized build: `cargo xtask utf8-check` runs this in an optimized one.
#[test]
#[ignore = "too slow unoptimized: cargo xtask utf8-check"]
fn exhaustive() {
    let mut count = 0u64;
    let mut agree = |bytes: &[u8]| {
        agrees(bytes);
        count += 1;
    };
    let fills: Vec<&str> = FILLS.iter().chain(&MORE_FILLS).copied().collect();
    let lead_of = |fill: &str, least: usize| fill.repeat(least.div_ceil(fill.len())).into_bytes();
    for length in 0..=3 {
        for sequence in sequences(&ALPHABET, length) {
            agree(&sequence);
            for fill in &fills {
                let (lead, fill) = (lead_of(fill, 3), fill.as_bytes());
                agree(&[lead.as_slice(), &sequence, fill].concat());
                agree(&[lead.as_slice(), &sequence].concat());
                agree(&[sequence.as_slice(), fill].concat());
            }
        }
    }
    // Every sequence of four bytes, and five bytes of one per class, after each fill: once near the start, where every
    // block has a pass for its largest byte, and once after 8 KiB of the fill, where blocks take the rules of the
    // blocks before them.
    let classes = [0x41, 0x80, 0x90, 0xa0, 0xc0, 0xc2, 0xe0, 0xe1, 0xed, 0xee, 0xf0, 0xf1, 0xf4, 0xf5];
    for (alphabet, length) in [(&ALPHABET[..], 4), (&classes[..], 5)] {
        for fill in &fills {
            let (short, long) = (lead_of(fill, 3), lead_of(fill, 8 * 1024 + 500));
            let mut buffer = long.clone();
            for sequence in sequences(alphabet, length) {
                agree(&[short.as_slice(), &sequence, fill.as_bytes()].concat());
                buffer.truncate(long.len());
                buffer.extend_from_slice(&sequence);
                buffer.extend_from_slice(fill.as_bytes());
                agree(&buffer);
            }
        }
    }
    // Sequences across block edges, in a run of alike blocks and at the start, also followed by other text.
    for fill in &fills {
        for start in (1022..=1027).chain(6 * 1024 - 2..=6 * 1024 + 4) {
            let mut prefix = vec![b'a'; start % fill.len()];
            prefix.extend(fill.repeat(start / fill.len()).bytes());
            for length in 1..=3 {
                for sequence in sequences(&ALPHABET, length) {
                    agree(&[prefix.as_slice(), &sequence, fill.as_bytes()].concat());
                    // Long enough that the next block is full, and so takes the sparse path after ASCII.
                    for other in ["a", "😀", "0123456789abcdef日"] {
                        let other = other.repeat(1100 / other.len());
                        agree(&[prefix.as_slice(), &sequence, other.as_bytes()].concat());
                    }
                }
            }
        }
    }
    // Every sequence of up to three bytes in text whose blocks before allowed other rules.
    for (first, second) in SWITCHES {
        let prefix = first.repeat(2100 / first.len()).into_bytes();
        let next = second.repeat(2400 / second.len()).into_bytes();
        for at in (590..610).chain(1490..1510) {
            if (next[at] as i8) < -64 {
                continue;
            }
            for length in 1..=3 {
                for sequence in sequences(&ALPHABET, length) {
                    agree(&[prefix.as_slice(), &next[..at], &sequence, &next[at..]].concat());
                }
            }
        }
    }
    // Mixed runs of every fill and of the pairs, three quarters damaged.
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut text = Vec::new();
    let runs: Vec<&str> = fills.iter().copied().chain(SWITCHES.iter().map(|(_, second)| *second)).collect();
    while text.len() < 256 * 1024 {
        text.extend(runs[rng.below(runs.len())].repeat(1 + rng.below(3000)).bytes());
    }
    for _ in 0..60_000 {
        let mut start = rng.below(text.len() - 40_000);
        let mut end = start + 64 + rng.below(30_000);
        while (text[start] as i8) < -64 {
            start += 1;
        }
        while (text[end] as i8) < -64 {
            end -= 1;
        }
        let mut bytes = text[start..end].to_vec();
        let at = rng.below(bytes.len());
        match rng.below(4) {
            0 => {}
            1 => bytes[at] = ALPHABET[rng.below(ALPHABET.len())],
            2 => {
                bytes.remove(at);
            }
            _ => bytes.insert(at, ALPHABET[rng.below(ALPHABET.len())]),
        }
        agree(&bytes);
    }
    let names: Vec<&str> = validators().into_iter().map(|(name, _)| name).collect();
    println!("{count} inputs: {} agree with std", names.join(" and "));
}

/// The name of a validator `choice()` picked.
fn name(prefix: Prefix) -> &'static str {
    let mut named = validators();
    named.push(("std", std_prefix as Prefix));
    named.into_iter().find(|(_, validator)| std::ptr::fn_addr_eq(*validator, prefix)).map_or("?", |(name, _)| name)
}

/// GB/s of each validator, of `is_utf8`, of the old length count and of `lossy_len` on 8 MiB of each input in 64 KiB
/// pieces, the median of 15 runs. The inputs are those of the review's `xbench`, plus CJK with a bad byte every 70 KB.
#[test]
#[ignore = "benchmark for an optimized build: cargo xtask utf8-bench"]
fn benchmark() {
    const SIZE: usize = 8 << 20;
    let start = Instant::now();
    let chosen = choice();
    let elapsed = start.elapsed();
    let (arch, os) = (std::env::consts::ARCH, std::env::consts::OS);
    let count = chosen.count.map_or("the count alone", name);
    println!("{arch} {os}: is_utf8 chose {}, lossy_len chose {count}, in {elapsed:?}", name(chosen.validate));

    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let mut log = String::new();
    while log.len() < SIZE {
        let mark = ["", "", "", "✓ ", "→ ", "🚀 ", "é "][rng.below(7)];
        let (minute, second, task, ms) = (rng.below(60), rng.below(60), rng.below(100_000), rng.below(5000));
        log.push_str(&format!("2026-11-22T21:{minute:02}:{second:02}Z INFO {mark}task {task} finished in {ms} ms\n"));
    }
    let mut words = |list: &[&str], space: &str, line: usize| {
        let (mut text, mut width) = (String::new(), 0);
        while text.len() < SIZE {
            let word = list[rng.below(list.len())];
            text.push_str(word);
            text.push_str(space);
            width += word.chars().count() + 1;
            if width > line {
                text.push('\n');
                width = 0;
            }
        }
        text.into_bytes()
    };
    let cjk = words(&["日本語", "の", "テキスト", "を", "表示", "します", "。", "中文", "测试", "数据"], "", 40);
    let mut damaged = cjk.clone();
    for at in (40_000..damaged.len()).step_by(70_001) {
        damaged[at] = 0xff;
    }
    let cyrillic = words(&["Привет", "мир", "файл", "строка", "ошибка", "сборка", "тест", "и", "в", "не"], " ", 70);
    let korean = words(&["한국어", "텍스트", "를", "표시", "합니다", "파일", "오류", "빌드"], " ", 50);
    // ASCII with one `mark` after each gap of `gap` bytes, or of a random length around `gap` when `jitter`.
    let mut sparse = |mark: &str, gap: usize, jitter: bool| {
        let line = b"let total = count + 1; // a line of ordinary source code, then more of it\n";
        let mut ascii = line.iter().copied().cycle();
        let mut text = Vec::new();
        while text.len() < SIZE {
            let gap = if jitter { gap / 2 + rng.below(gap) } else { gap - mark.len() };
            text.extend(ascii.by_ref().take(gap));
            text.extend_from_slice(mark.as_bytes());
        }
        text
    };
    let emoji_256 = sparse("😀", 256, false);
    let emoji_1024 = sparse("😀", 1024, false);
    let emoji_about_1024 = sparse("😀", 1024, true);
    let cjk_about_256 = sparse("日", 256, true);
    let mut runs = Vec::new();
    while runs.len() < SIZE {
        // 2 KiB of CJK cut at character starts.
        let mut start = runs.len() % (cjk.len() - 4096);
        while (cjk[start] as i8) < -64 {
            start += 1;
        }
        let mut end = start + 2048;
        while (cjk[end] as i8) < -64 {
            end -= 1;
        }
        runs.extend_from_slice(&cjk[start..end]);
        runs.extend(std::iter::repeat_n(b'x', 2047).chain(*b"\n"));
    }
    let widths = ["a", "b", " ", "\n", "é", "я", "日", "한", "\u{800}", "😀", "\u{10ffff}"];
    let mixed: Vec<u8> = (0..SIZE / 2).flat_map(|_| widths[rng.below(widths.len())].bytes()).collect();
    let inputs = [
        ("ASCII log", log.bytes().filter(u8::is_ascii).collect()),
        ("log with marks", log.into_bytes()),
        ("CJK", cjk),
        ("Cyrillic", cyrillic),
        ("Korean", korean),
        ("emoji every 10 B", "emoji 😄".repeat(SIZE / 10).into_bytes()),
        ("emoji every 256 B", emoji_256),
        ("emoji every 1 KiB", emoji_1024),
        ("emoji every ~1 KiB", emoji_about_1024),
        ("CJK every ~256 B", cjk_about_256),
        ("2 KiB CJK, 2 KiB ASCII", runs),
        ("all widths mixed", mixed),
        ("CJK, bad byte per 70 KB", damaged),
    ];

    // Validation columns report whether the whole piece is valid; length columns report the lossy length.
    let mut columns: Vec<(&str, Prefix)> = vec![("std", |bytes| usize::from(std_prefix(bytes) == bytes.len()))];
    for (name, validator) in validators() {
        columns.push((name, validator));
    }
    columns.extend([
        ("is_utf8", (|bytes| usize::from(is_utf8(bytes))) as Prefix),
        ("count", |bytes| if bytes.is_ascii() { bytes.len() } else { counted_len(bytes) }),
        ("lossy_len", lossy_len),
    ]);
    print!("{:<24}", "GB/s");
    for (name, _) in &columns {
        print!("{name:>11}");
    }
    println!();
    for (name, data) in inputs {
        // 64 KiB pieces that end before a character, as the decoder cuts them.
        let mut pieces: Vec<&[u8]> = Vec::new();
        let mut rest = &data[..];
        while !rest.is_empty() {
            let mut end = rest.len().min(64 * 1024);
            while end < rest.len() && (rest[end] as i8) < -64 {
                end -= 1;
            }
            pieces.push(&rest[..end]);
            rest = &rest[end..];
        }
        for piece in &pieces {
            assert_eq!(is_utf8(piece), std::str::from_utf8(piece).is_ok(), "{name}");
            assert_eq!(lossy_len(piece), String::from_utf8_lossy(piece).len(), "{name}");
        }
        let mut seconds = vec![Vec::new(); columns.len()];
        for round in 0..15 {
            for offset in 0..columns.len() {
                let index = (round + offset) % columns.len();
                let start = Instant::now();
                for piece in &pieces {
                    black_box((columns[index].1)(black_box(piece)));
                }
                seconds[index].push(start.elapsed().as_secs_f64());
            }
        }
        print!("{name:<24}");
        for mut runs in seconds {
            runs.sort_by(f64::total_cmp);
            print!("{:>11.2}", data.len() as f64 / runs[runs.len() / 2] / 1e9);
        }
        println!();
    }
}
