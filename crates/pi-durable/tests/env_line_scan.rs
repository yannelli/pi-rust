//! Port of upstream `test/env-line-scan.test.ts`.

use pi_durable::env::{LineScanner, StreamDecoder};

/// mulberry32, as upstream seeds it.
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

// Newlines, ASCII, a byte-order mark, valid multi-byte sequences, and bytes that form invalid or truncated sequences.
const PIECES: &[&[u8]] = &[
    &[0x0a],
    &[0x0a],
    &[0x61],
    &[0x62, 0x63],
    &[0xef, 0xbb, 0xbf],
    &[0xc3, 0xa9],
    &[0xe2, 0x82, 0xac],
    &[0xf0, 0x9f, 0x98, 0x80],
    &[0xe2, 0x82],
    &[0xff],
    &[0x80],
    &[0xf0, 0x9f],
    &[0x0d, 0x0a],
];

fn pick(next: &mut impl FnMut() -> f64, count: usize) -> usize {
    (next() * count as f64).floor() as usize
}

fn random_file(next: &mut impl FnMut() -> f64) -> Vec<u8> {
    let pieces = pick(next, 60);
    (0..pieces).flat_map(|_| PIECES[pick(next, PIECES.len())].iter().copied()).collect()
}

/// `new TextDecoder("utf-8", { ignoreBOM: !strip_bom }).decode(bytes)`.
fn decode_whole(bytes: &[u8], strip_bom: bool) -> String {
    let text = String::from_utf8_lossy(bytes).into_owned();
    match text.strip_prefix('\u{feff}') {
        Some(rest) if strip_bom => rest.to_string(),
        _ => text,
    }
}

#[test]
fn stream_decoder_keeps_a_u_feff_that_does_not_start_the_stream() {
    // Node's streaming TextDecoder with BOM handling dropped this U+FEFF, which follows an invalid sequence.
    let bytes = [0xe2, 0x82, 0xef, 0xbb, 0xbf, 0x61];
    let mut decoder = StreamDecoder::new();
    let mut text: String = bytes.iter().map(|byte| decoder.decode(&[*byte])).collect();
    text.push_str(&decoder.finish());
    assert_eq!(text, decode_whole(&bytes, true));
    assert_eq!(text, "\u{fffd}\u{feff}a");
}

#[test]
fn stream_decoder_decodes_any_chunking_like_decoding_the_whole_stream() {
    for seed in 1..=5000 {
        let mut next = random(seed);
        let bytes = random_file(&mut next);
        let mut decoder = StreamDecoder::new();
        let mut text = String::new();
        let mut offset = 0;
        while offset < bytes.len() {
            let size = 1 + pick(&mut next, 7);
            text.push_str(&decoder.decode(&bytes[offset..(offset + size).min(bytes.len())]));
            offset += size;
        }
        text.push_str(&decoder.finish());
        assert_eq!(text, decode_whole(&bytes, true), "seed {seed}");
    }
}

#[test]
fn line_scanner_agrees_with_decoding_and_splitting_the_whole_file() {
    for seed in 1..=5000 {
        let mut next = random(seed);
        let file = random_file(&mut next);
        let whole = decode_whole(&file, true);
        let lines: Vec<&str> = whole.split('\n').collect();
        let start_line = pick(&mut next, lines.len() + 2);
        let end_line = if next() < 0.3 { None } else { Some(start_line + 1 + pick(&mut next, lines.len() + 1)) };
        let mut scanner = LineScanner::new(start_line as u64, end_line.map(|end| end as u64)).unwrap();
        let mut offset = 0;
        while offset < file.len() {
            let size = 1 + pick(&mut next, 7);
            scanner.push(&file[offset..(offset + size).min(file.len())]);
            offset += size;
        }
        let scan = scanner.finish();
        let message = format!("seed {seed}");
        assert_eq!(scan.newlines as usize, lines.len() - 1, "{message}");
        let end = end_line.unwrap_or(usize::MAX).min(lines.len());
        let selected = if start_line < lines.len() { lines[start_line..end].join("\n") } else { String::new() };
        let decode = |from: u64, to: u64| decode_whole(&file[from as usize..to as usize], from == 0);
        assert_eq!(decode(scan.start, scan.end), selected, "{message}");
        assert_eq!(scan.selected_bytes as usize, selected.len(), "{message}");
        if start_line < lines.len() {
            assert_eq!(decode(scan.start, scan.first_line_end), lines[start_line], "{message}");
            assert_eq!(scan.first_line_bytes as usize, lines[start_line].len(), "{message}");
            let last_line = end - 1;
            let last_end = if last_line + 1 < lines.len() {
                let from = scan.last_line_start as usize;
                (from + file[from..].iter().position(|&byte| byte == 0x0a).unwrap()) as u64
            } else {
                file.len() as u64
            };
            assert_eq!(decode(scan.last_line_start, last_end), lines[last_line], "{message}");
        } else {
            let size = file.len() as u64;
            assert_eq!((scan.start, scan.end, scan.selected_bytes), (size, size, 0), "{message}");
        }
    }
}

/// Negative and fractional lines cannot be expressed as `u64`; an empty range remains to reject.
#[test]
fn line_scanner_rejects_empty_ranges() {
    assert!(LineScanner::new(2, Some(2)).is_err());
    assert!(LineScanner::new(3, Some(1)).is_err());
}
