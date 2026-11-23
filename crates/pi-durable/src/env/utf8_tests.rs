//! The kernel against `std::str::from_utf8`. The tests call the kernel itself, because an unoptimized build chooses
//! `std` at first use.

use std::hint::black_box;
use std::time::Instant;

use super::{is_utf8, kernel};

/// Both ends of every byte class that UTF-8 or a kernel rule tells apart. A byte between two members of a class
/// behaves like them in both validators.
const ALPHABET: [u8; 27] = [
    0x00, 0x0a, 0x41, 0x7f, 0x80, 0x8f, 0x90, 0x9f, 0xa0, 0xbf, 0xc0, 0xc1, 0xc2, 0xdf, 0xe0, 0xe1, 0xec, 0xed, 0xee,
    0xef, 0xf0, 0xf1, 0xf3, 0xf4, 0xf5, 0xfb, 0xff,
];

/// Text of each character width: the widest lead byte in a block selects the rules the kernel applies to it.
const FILLS: [&str; 4] = ["a", "я", "日", "😀"];

fn agrees(bytes: &[u8]) {
    assert_eq!(kernel::is_utf8(bytes), std::str::from_utf8(bytes).is_ok(), "{bytes:02x?}");
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
fn sequences_across_a_block_edge_match_std() {
    // The second block starts at byte 1027, and the three bytes before it take part in choosing its rules.
    for fill in FILLS {
        // Unoptimized, a block of wider text takes longer, so wider text gets sequences of up to two bytes.
        let longest = if fill.len() == 1 { 3 } else { 2 };
        for start in 1024..=1027 {
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
fn damaged_mixed_text_over_many_blocks_matches_std() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut text = Vec::new();
    while text.len() < 16 * 1024 {
        text.extend(FILLS[rng.below(FILLS.len())].repeat(1 + rng.below(600)).bytes());
    }
    for _ in 0..1000 {
        // Cut at character starts, so that undamaged pieces are valid.
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
        agrees(&bytes);
    }
}

#[test]
fn is_utf8_matches_std_on_both_sides_of_the_switch_to_the_chosen_validator() {
    for fill in FILLS {
        let text = fill.repeat(100);
        for length in 56..=72 {
            let mut bytes = text.as_bytes()[..length].to_vec();
            assert_eq!(is_utf8(&bytes), std::str::from_utf8(&bytes).is_ok(), "{bytes:02x?}");
            bytes[length / 2] = 0xff;
            assert!(!is_utf8(&bytes), "{bytes:02x?}");
        }
    }
}

/// GB/s of `std`, the kernel, and `is_utf8` on 8 MiB of each input in 64 KiB pieces, the median of 15 runs. The inputs
/// are those of the review's `xbench`, plus CJK with a bad byte every 70 KB.
#[test]
#[ignore = "benchmark for an optimized build: cargo xtask utf8-bench"]
fn benchmark() {
    const SIZE: usize = 8 << 20;
    let start = Instant::now();
    let kernel_chosen = std::ptr::fn_addr_eq(kernel::chosen(), kernel::is_utf8 as fn(&[u8]) -> bool);
    let elapsed = start.elapsed();
    let (arch, os) = (std::env::consts::ARCH, std::env::consts::OS);
    println!("{arch} {os}: is_utf8 chose {} in {elapsed:?}", if kernel_chosen { "the kernel" } else { "std" });

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
    let inputs = [
        ("ASCII log", log.bytes().filter(u8::is_ascii).collect()),
        ("log with marks", log.into_bytes()),
        ("CJK", cjk),
        ("Cyrillic", words(&["Привет", "мир", "файл", "строка", "ошибка", "сборка", "тест", "и", "в", "не"], " ", 70)),
        ("Korean", words(&["한국어", "텍스트", "를", "표시", "합니다", "파일", "오류", "빌드"], " ", 50)),
        ("emoji in every KiB", "emoji 😄".repeat(SIZE / 10).into_bytes()),
        ("CJK, bad byte per 70 KB", damaged),
    ];

    let validators: [fn(&[u8]) -> bool; 3] = [super::std_is_utf8, kernel::is_utf8, is_utf8];
    println!("{:<24}{:>10}{:>10}{:>10}", "GB/s", "std", "kernel", "is_utf8");
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
        let expected: Vec<bool> = pieces.iter().map(|piece| super::std_is_utf8(piece)).collect();
        for validator in validators {
            assert_eq!(pieces.iter().map(|piece| validator(piece)).collect::<Vec<_>>(), expected, "{name}");
        }
        let mut seconds = [const { Vec::new() }; 3];
        for round in 0..15 {
            for offset in 0..validators.len() {
                let index = (round + offset) % validators.len();
                let start = Instant::now();
                for piece in &pieces {
                    black_box(validators[index](black_box(piece)));
                }
                seconds[index].push(start.elapsed().as_secs_f64());
            }
        }
        print!("{name:<24}");
        for mut runs in seconds {
            runs.sort_by(f64::total_cmp);
            print!("{:>10.2}", data.len() as f64 / runs[runs.len() / 2] / 1e9);
        }
        println!();
    }
}
