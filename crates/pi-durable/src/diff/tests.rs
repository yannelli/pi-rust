use super::*;

/// `(old, new, display diff, first changed line, patch)`, as upstream pi and jsdiff 8.0.4 give them.
const CASES: &[(&str, &str, &str, Option<usize>, &str)] = &[
    ("a\n", "b\na\na\n", "+1 b\n 1 a\n+3 a", Some(1), "--- f.txt\n+++ f.txt\n@@ -1,1 +1,3 @@\n+b\n a\n+a\n"),
    ("x\n", "y\n", "-1 x\n+1 y", Some(1), "--- f.txt\n+++ f.txt\n@@ -1,1 +1,1 @@\n-x\n+y\n"),
    (
        "a\nb",
        "a\nc\n",
        " 1 a\n-2 b\n+2 c",
        Some(2),
        "--- f.txt\n+++ f.txt\n@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+c\n",
    ),
    (
        "a\nb\n",
        "a\nb",
        " 1 a\n-2 b\n+2 b",
        Some(2),
        "--- f.txt\n+++ f.txt\n@@ -1,2 +1,2 @@\n a\n-b\n+b\n\\ No newline at end of file\n",
    ),
    ("", "a\n", "+1 a", Some(1), "--- f.txt\n+++ f.txt\n@@ -0,0 +1,1 @@\n+a\n"),
    ("a\nb\n", "", "-1 a\n-2 b", Some(1), "--- f.txt\n+++ f.txt\n@@ -1,2 +0,0 @@\n-a\n-b\n"),
    ("a\n", "a\n", "", None, "--- f.txt\n+++ f.txt\n"),
    (
        "a\r\nb\r\n",
        "a\r\nc\r\n",
        " 1 a\r\n-2 b\r\n+2 c\r",
        Some(2),
        "--- f.txt\n+++ f.txt\n@@ -1,2 +1,2 @@\n a\r\n-b\r\n+c\r\n",
    ),
    ("a\rb\n", "a\n", "-1 a\rb\n+1 a", Some(1), "--- f.txt\n+++ f.txt\n@@ -1,1 +1,1 @@\n-a\rb\n+a\n"),
    (
        "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30\n",
        "l1\nl2\nX\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nl11\nX\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30\n",
        "  1 l1\n  2 l2\n- 3 l3\n+ 3 X\n  4 l4\n  5 l5\n  6 l6\n  7 l7\n  8 l8\n  9 l9\n 10 l10\n 11 l11\n-12 l12\n+12 X\n 13 l13\n 14 l14\n 15 l15\n 16 l16\n    ...",
        Some(3),
        "--- f.txt\n+++ f.txt\n@@ -1,16 +1,16 @@\n l1\n l2\n-l3\n+X\n l4\n l5\n l6\n l7\n l8\n l9\n l10\n l11\n-l12\n+X\n l13\n l14\n l15\n l16\n",
    ),
    (
        "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30\n",
        "l1\nl2\nX\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nl11\nl12\nX\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30\n",
        "  1 l1\n  2 l2\n- 3 l3\n+ 3 X\n  4 l4\n  5 l5\n  6 l6\n  7 l7\n    ...\n  9 l9\n 10 l10\n 11 l11\n 12 l12\n-13 l13\n+13 X\n 14 l14\n 15 l15\n 16 l16\n 17 l17\n    ...",
        Some(3),
        "--- f.txt\n+++ f.txt\n@@ -1,7 +1,7 @@\n l1\n l2\n-l3\n+X\n l4\n l5\n l6\n l7\n@@ -9,9 +9,9 @@\n l9\n l10\n l11\n l12\n-l13\n+X\n l14\n l15\n l16\n l17\n",
    ),
];

#[test]
fn line_diffs_match_upstream() {
    for &(old, new, diff, first_changed_line, patch) in CASES {
        let expected = EditDiff { diff: diff.to_owned(), first_changed_line, patch: patch.to_owned() };
        assert_eq!(generate_edit_diff("f.txt", old, new), expected, "{old:?} -> {new:?}");
        assert_eq!(generate_diff_string(old, new), (expected.diff, first_changed_line));
        assert_eq!(generate_unified_patch("f.txt", old, new), expected.patch);
    }
}

#[test]
fn large_diff_is_a_valid_edit_script() {
    let old: Vec<u32> = (0..3000).map(|i| i % 7).collect();
    let new: Vec<u32> = (0..3000).map(|i| (i * 5 + 3) % 11).collect();
    let changes = diff_tokens(&old, &new, |a, b| a == b);
    let (mut rebuilt, mut old_pos) = (Vec::new(), 0);
    for change in &changes {
        match change.kind {
            ChangeKind::Removed => old_pos += change.len,
            ChangeKind::Equal => {
                assert_eq!(old[old_pos..old_pos + change.len], new[change.start..change.start + change.len]);
                old_pos += change.len;
                rebuilt.extend_from_slice(&new[change.start..change.start + change.len]);
            }
            ChangeKind::Added => rebuilt.extend_from_slice(&new[change.start..change.start + change.len]),
        }
    }
    assert_eq!((rebuilt, old_pos), (new, old.len()));
}
