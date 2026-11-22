use super::*;

/// `(old, new, context, display diff, first changed line, patch)`, as upstream pi and jsdiff 8.0.4 give them.
type Case = (&'static str, &'static str, usize, &'static str, Option<usize>, &'static str);

const CASES: &[Case] = &[
    ("a\n", "b\na\na\n", 4, "+1 b\n 1 a\n+3 a", Some(1), "--- f.txt\n+++ f.txt\n@@ -1,1 +1,3 @@\n+b\n a\n+a\n"),
    ("x\n", "y\n", 4, "-1 x\n+1 y", Some(1), "--- f.txt\n+++ f.txt\n@@ -1,1 +1,1 @@\n-x\n+y\n"),
    (
        "a\nb",
        "a\nc\n",
        4,
        " 1 a\n-2 b\n+2 c",
        Some(2),
        "--- f.txt\n+++ f.txt\n@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+c\n",
    ),
    (
        "a\nb\n",
        "a\nb",
        4,
        " 1 a\n-2 b\n+2 b",
        Some(2),
        "--- f.txt\n+++ f.txt\n@@ -1,2 +1,2 @@\n a\n-b\n+b\n\\ No newline at end of file\n",
    ),
    ("", "a\n", 4, "+1 a", Some(1), "--- f.txt\n+++ f.txt\n@@ -0,0 +1,1 @@\n+a\n"),
    ("a\nb\n", "", 4, "-1 a\n-2 b", Some(1), "--- f.txt\n+++ f.txt\n@@ -1,2 +0,0 @@\n-a\n-b\n"),
    ("a\n", "a\n", 4, "", None, "--- f.txt\n+++ f.txt\n"),
    (
        "a\r\nb\r\n",
        "a\r\nc\r\n",
        4,
        " 1 a\r\n-2 b\r\n+2 c\r",
        Some(2),
        "--- f.txt\n+++ f.txt\n@@ -1,2 +1,2 @@\n a\r\n-b\r\n+c\r\n",
    ),
    ("a\rb\n", "a\n", 4, "-1 a\rb\n+1 a", Some(1), "--- f.txt\n+++ f.txt\n@@ -1,1 +1,1 @@\n-a\rb\n+a\n"),
    (
        "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30\n",
        "l1\nl2\nX\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nl11\nX\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30\n",
        4,
        "  1 l1\n  2 l2\n- 3 l3\n+ 3 X\n  4 l4\n  5 l5\n  6 l6\n  7 l7\n  8 l8\n  9 l9\n 10 l10\n 11 l11\n-12 l12\n+12 X\n 13 l13\n 14 l14\n 15 l15\n 16 l16\n    ...",
        Some(3),
        "--- f.txt\n+++ f.txt\n@@ -1,16 +1,16 @@\n l1\n l2\n-l3\n+X\n l4\n l5\n l6\n l7\n l8\n l9\n l10\n l11\n-l12\n+X\n l13\n l14\n l15\n l16\n",
    ),
    (
        "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30\n",
        "l1\nl2\nX\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nl11\nl12\nX\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30\n",
        4,
        "  1 l1\n  2 l2\n- 3 l3\n+ 3 X\n  4 l4\n  5 l5\n  6 l6\n  7 l7\n    ...\n  9 l9\n 10 l10\n 11 l11\n 12 l12\n-13 l13\n+13 X\n 14 l14\n 15 l15\n 16 l16\n 17 l17\n    ...",
        Some(3),
        "--- f.txt\n+++ f.txt\n@@ -1,7 +1,7 @@\n l1\n l2\n-l3\n+X\n l4\n l5\n l6\n l7\n@@ -9,9 +9,9 @@\n l9\n l10\n l11\n l12\n-l13\n+X\n l14\n l15\n l16\n l17\n",
    ),
    (
        "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30\n",
        "l1\nl2\nX\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nl11\nX\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30\n",
        0,
        "    ...\n- 3 l3\n+ 3 X\n    ...\n-12 l12\n+12 X\n    ...",
        Some(3),
        "--- f.txt\n+++ f.txt\n@@ -3,1 +3,1 @@\n-l3\n+X\n@@ -12,1 +12,1 @@\n-l12\n+X\n",
    ),
    (
        "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30\n",
        "l1\nl2\nX\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nl11\nX\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30\n",
        1,
        "    ...\n  2 l2\n- 3 l3\n+ 3 X\n  4 l4\n    ...\n 11 l11\n-12 l12\n+12 X\n 13 l13\n    ...",
        Some(3),
        "--- f.txt\n+++ f.txt\n@@ -2,3 +2,3 @@\n l2\n-l3\n+X\n l4\n@@ -11,3 +11,3 @@\n l11\n-l12\n+X\n l13\n",
    ),
    (
        "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30\n",
        "l1\nl2\nX\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nl11\nl12\nX\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30\n",
        usize::MAX,
        "  1 l1\n  2 l2\n- 3 l3\n+ 3 X\n  4 l4\n  5 l5\n  6 l6\n  7 l7\n  8 l8\n  9 l9\n 10 l10\n 11 l11\n 12 l12\n-13 l13\n+13 X\n 14 l14\n 15 l15\n 16 l16\n 17 l17\n 18 l18\n 19 l19\n 20 l20\n 21 l21\n 22 l22\n 23 l23\n 24 l24\n 25 l25\n 26 l26\n 27 l27\n 28 l28\n 29 l29\n 30 l30",
        Some(3),
        "--- f.txt\n+++ f.txt\n@@ -1,30 +1,30 @@\n l1\n l2\n-l3\n+X\n l4\n l5\n l6\n l7\n l8\n l9\n l10\n l11\n l12\n-l13\n+X\n l14\n l15\n l16\n l17\n l18\n l19\n l20\n l21\n l22\n l23\n l24\n l25\n l26\n l27\n l28\n l29\n l30\n",
    ),
    ("a\nb\nc\n", "a\nc\n", 0, "   ...\n-2 b\n   ...", Some(2), "--- f.txt\n+++ f.txt\n@@ -2,1 +1,0 @@\n-b\n"),
    ("", "a\n", 0, "+1 a", Some(1), "--- f.txt\n+++ f.txt\n@@ -0,0 +1,1 @@\n+a\n"),
];

#[test]
fn line_diffs_match_upstream() {
    for &(old, new, context, diff, first_changed_line, patch) in CASES {
        assert_eq!(
            generate_diff_string(old, new, context),
            (diff.to_owned(), first_changed_line),
            "{old:?} -> {new:?}"
        );
        assert_eq!(generate_unified_patch("f.txt", old, new, context), patch, "{old:?} -> {new:?}");
        if context == DEFAULT_CONTEXT_LINES {
            let expected = EditDiff { diff: diff.to_owned(), first_changed_line, patch: patch.to_owned() };
            assert_eq!(generate_edit_diff("f.txt", old, new), expected);
        }
    }
}

/// The xorshift32 pair of `scripts/jsdiff-oracle.mjs`: 3,000 lines with 40% of them changed.
fn large_pair() -> (String, String) {
    const VOCAB: [&str; 8] = ["}", "", "  return x;", "a", "b", "\tif (x) {", "x", "é中😀"];
    let mut state: u32 = 0x2545_f491;
    let mut next = move |n: u32| {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        (state % n) as usize
    };
    let old: Vec<&str> = (0..3000).map(|_| VOCAB[next(8)]).collect();
    let new: Vec<&str> = old.iter().map(|&line| if next(10) < 4 { VOCAB[next(8)] } else { line }).collect();
    (old.join("\n") + "\n", new.join("\n") + "\n")
}

#[test]
fn large_diff_matches_upstream() {
    let (old, new) = large_pair();
    let EditDiff { diff, first_changed_line, patch } = generate_edit_diff("f.txt", &old, &new);
    let first = first_changed_line.map_or("undefined".to_owned(), |line| line.to_string());
    let digest = ring::digest::digest(&ring::digest::SHA256, format!("{diff}\n{first}\n{patch}").as_bytes());
    let hex: String = digest.as_ref().iter().map(|byte| format!("{byte:02x}")).collect();
    // `largeSha256` of scripts/jsdiff-oracle.mjs.
    assert_eq!(hex, "716ed01ed3e1196c2cade31fca79b8b11ff07a1456b88ea7d07d59fe3f98e093");
}

#[test]
fn large_diff_is_a_valid_edit_script() {
    let old: Vec<u32> = (0..3000).map(|i| i % 7).collect();
    let new: Vec<u32> = (0..3000).map(|i| (i * 5 + 3) % 11).collect();
    let (mut rebuilt, mut old_pos) = (Vec::new(), 0);
    for change in diff_tokens(&old, &new, |a, b| a == b) {
        let new_run = &new[change.start..change.start + change.len];
        match change.kind {
            ChangeKind::Removed => old_pos += change.len,
            ChangeKind::Equal => {
                assert_eq!(&old[old_pos..old_pos + change.len], new_run);
                old_pos += change.len;
                rebuilt.extend_from_slice(new_run);
            }
            ChangeKind::Added => rebuilt.extend_from_slice(new_run),
        }
    }
    assert_eq!((rebuilt, old_pos), (new, old.len()));
}

/// Compares the port with the output of `scripts/jsdiff-oracle.mjs`; docs/parity/jsdiff.md has the steps.
#[test]
fn tokens_without_a_match_are_removed_then_added() {
    for (old, new) in (0..24).flat_map(|old| (0..24).map(move |new| (old, new))) {
        let changes = diff_tokens(&vec![0; old], &vec![1; new], |a, b| a == b);
        assert_eq!(changes, replace_all(old, new), "{old} old, {new} new");
    }
}

#[test]
#[ignore = "needs PI_JSDIFF_ORACLE, the output of scripts/jsdiff-oracle.mjs"]
fn matches_jsdiff_oracle() {
    let path = std::env::var("PI_JSDIFF_ORACLE").expect("PI_JSDIFF_ORACLE names the oracle output");
    let oracle: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let text = |value: &serde_json::Value| value.as_str().unwrap().to_owned();
    let mut failures = Vec::new();
    let line_cases = oracle["lines"].as_array().unwrap();
    for case in line_cases {
        let (old, new) = (text(&case["old"]), text(&case["new"]));
        let context = case["context"].as_u64().unwrap() as usize;
        let expected_first = case["first"].as_u64().map(|line| line as usize);
        if generate_diff_string(&old, &new, context) != (text(&case["diff"]), expected_first)
            || generate_unified_patch("f.txt", &old, &new, context) != text(&case["patch"])
        {
            failures.push(format!("lines {old:?} -> {new:?} (context {context})"));
        }
    }
    let word_cases = oracle["words"].as_array().unwrap();
    for case in word_cases {
        let (old, new) = (text(&case["old"]), text(&case["new"]));
        let expected: Vec<(String, String)> =
            case["changes"].as_array().unwrap().iter().map(|change| (text(&change[0]), text(&change[1]))).collect();
        let actual: Vec<(String, String)> =
            diff_words(&old, &new).into_iter().map(|(kind, value)| (format!("{kind:?}"), value)).collect();
        if actual != expected {
            failures.push(format!("words {old:?} -> {new:?}"));
        }
    }
    let cases = line_cases.len() + word_cases.len();
    assert!(
        failures.is_empty(),
        "{} of {cases} cases differ: {:#?}",
        failures.len(),
        &failures[..failures.len().min(20)]
    );
    assert!(line_cases.len() > 1000 && word_cases.len() > 1000, "the oracle output looks incomplete");
}
