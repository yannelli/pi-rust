//! `line_tokens::line_changes` against `diff_tokens` on the `split_inclusive('\n')` lines of random texts.

use std::ops::Range;

use super::super::line_tokens::line_changes;
use super::super::{Change, ChangeKind, diff_tokens};
use super::Xorshift;

const PREFIX_LINES: [&str; 4] = ["fn f() {\n", "\n", "q\r\n", "  let a = 1;\n"];
const OLD_LINES: [&str; 6] = ["a\n", "\n", "b\r\n", "  return x;\n", "é中\n", "a\rb\n"];
const NEW_LINES: [&str; 6] = ["x\n", "\r\n", "y\r\n", "\treturn y;\n", "😀\n", "x\ry\n"];

/// The search of `line_changes` that gives the changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Bytes,
    SharedIds,
    DisjointIds,
}

/// The stage `line_changes` reaches for `old` and `new`, from the changes of `diff_tokens`.
fn reached_stage(old: &[&str], new: &[&str], changes: &[Change]) -> Stage {
    let shared = old.iter().zip(new).take_while(|(a, b)| a == b && a.ends_with('\n')).count();
    // Each round of the search adds one removed or added line to a path, so the search ends at round `edits`.
    let edits: usize = changes.iter().filter(|change| change.kind != ChangeKind::Equal).map(|change| change.len).sum();
    if edits <= 16 + 2 * (old.len() + new.len() - 2 * shared).isqrt() {
        return Stage::Bytes;
    }
    match new[shared..].iter().any(|line| old[shared..].contains(line)) {
        true => Stage::SharedIds,
        false => Stage::DisjointIds,
    }
}

fn random_lines(rng: &mut Xorshift, vocab: &[&'static str], len: Range<usize>) -> Vec<&'static str> {
    let len = len.start + rng.pick(len.end - len.start);
    (0..len).map(|_| vocab[rng.pick(vocab.len())]).collect()
}

fn random_line(rng: &mut Xorshift) -> &'static str {
    let vocab = if rng.pick(2) == 0 { &OLD_LINES } else { &NEW_LINES };
    vocab[rng.pick(vocab.len())]
}

/// The lines joined, without the last `\n` in a third of the texts.
fn join(rng: &mut Xorshift, prefix: &[&str], body: &[&str]) -> String {
    let mut text: String = prefix.iter().chain(body).copied().collect();
    if rng.pick(3) == 0 && text.ends_with('\n') {
        text.pop();
    }
    text
}

/// Two texts with a common prefix of whole lines, built so that `line_changes` reaches `stage`.
fn pair(rng: &mut Xorshift, stage: Stage) -> (String, String) {
    let prefix = if rng.pick(4) == 0 { Vec::new() } else { random_lines(rng, &PREFIX_LINES, 1..40) };
    let (old, new) = match stage {
        // At most 6 edits, and each removed `\n` replaces or removes one line: 16 or fewer lines change.
        Stage::Bytes => {
            let old = random_lines(rng, &OLD_LINES, 0..80);
            let mut new = old.clone();
            for _ in 0..rng.pick(7) {
                let line = random_line(rng);
                match rng.pick(3) {
                    0 => new.insert(rng.pick(new.len() + 1), line),
                    _ if new.is_empty() => {}
                    1 => {
                        new.remove(rng.pick(new.len()));
                    }
                    _ => {
                        let at = rng.pick(new.len());
                        new[at] = line;
                    }
                }
            }
            (old, new)
        }
        // Interior matches survive both choices of final newline.
        Stage::SharedIds => {
            let old = random_lines(rng, &OLD_LINES, 40..121);
            let mut new = random_lines(rng, &NEW_LINES, 40..121);
            for _ in 0..1 + rng.pick(4) {
                let at = 1 + rng.pick(new.len() - 2);
                new[at] = old[rng.pick(old.len() - 1)];
            }
            (old, new)
        }
        // Disjoint bodies exceed the byte search budget.
        Stage::DisjointIds => {
            let old = random_lines(rng, &OLD_LINES, 20..61);
            let new = random_lines(rng, &NEW_LINES, 20..61);
            (old, new)
        }
    };
    (join(rng, &prefix, &old), join(rng, &prefix, &new))
}

#[test]
fn line_changes_match_diff_tokens_in_each_stage() {
    let mut rng = Xorshift(0x9e37_79b9);
    for stage in [Stage::Bytes, Stage::SharedIds, Stage::DisjointIds] {
        for case in 0..200 {
            let (old_text, new_text) = pair(&mut rng, stage);
            let old: Vec<&str> = old_text.split_inclusive('\n').collect();
            let new: Vec<&str> = new_text.split_inclusive('\n').collect();
            let label = format!("{stage:?} case {case}: {old_text:?} -> {new_text:?}");
            let (old_lines, new_lines, changes) = line_changes(&old_text, &new_text);
            assert_eq!((0..old_lines.len()).map(|i| old_lines.get(i)).collect::<Vec<_>>(), old, "{label}");
            assert_eq!((0..new_lines.len()).map(|i| new_lines.get(i)).collect::<Vec<_>>(), new, "{label}");
            let expected = diff_tokens(&old, &new, |a, b| a == b);
            assert_eq!(changes, expected, "{label}");
            assert_eq!(reached_stage(&old, &new, &expected), stage, "{label}");
        }
    }
}
