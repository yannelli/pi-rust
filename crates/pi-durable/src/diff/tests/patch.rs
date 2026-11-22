use super::{Xorshift, generate_diff_string, generate_unified_patch};

type Row<'a> = (char, usize, &'a str);

struct Applied<'a> {
    text: String,
    rows: Vec<Row<'a>>,
    first_change: Option<usize>,
}

fn hunk_range(range: &str) -> (usize, usize) {
    let (start, count) = range.split_once(',').unwrap();
    let start: usize = start.parse().unwrap();
    let count: usize = count.parse().unwrap();
    (if count == 0 { start } else { start - 1 }, count)
}

fn apply_patch<'a>(old: &str, patch: &'a str) -> Applied<'a> {
    let source: Vec<_> = old.split_inclusive('\n').collect();
    let mut lines = patch.split_inclusive('\n').peekable();
    assert_eq!(lines.next(), Some("--- f.txt\n"));
    assert_eq!(lines.next(), Some("+++ f.txt\n"));
    let mut applied = Applied { text: String::new(), rows: Vec::new(), first_change: None };
    let (mut old_pos, mut new_pos) = (0, 0);
    while let Some(header) = lines.next() {
        let ranges = header.strip_prefix("@@ -").unwrap().strip_suffix(" @@\n").unwrap();
        let (from, to) = ranges.split_once(" +").unwrap();
        let (old_start, old_count) = hunk_range(from);
        let (new_start, new_count) = hunk_range(to);
        for line in &source[old_pos..old_start] {
            applied.text.push_str(line);
        }
        new_pos += old_start - old_pos;
        assert_eq!(new_pos, new_start);
        old_pos = old_start;

        while let Some(line) = lines.next_if(|line| !line.starts_with("@@ ")) {
            let (sign, content) = line.split_at(1);
            let sign = sign.chars().next().unwrap();
            assert!(matches!(sign, '+' | '-' | ' '));
            let display = content.strip_suffix('\n').unwrap();
            let missing_lf = lines.next_if_eq(&"\\ No newline at end of file\n").is_some();
            let content = if missing_lf { display } else { content };
            let number = if sign == '+' { new_pos + 1 } else { old_pos + 1 };
            applied.rows.push((sign, number, display));
            if sign != ' ' {
                applied.first_change.get_or_insert(new_pos + 1);
            }
            if sign != '+' {
                assert_eq!(source[old_pos], content);
                old_pos += 1;
            }
            if sign != '-' {
                applied.text.push_str(content);
                new_pos += 1;
            }
        }
        assert_eq!((old_pos, new_pos), (old_start + old_count, new_start + new_count));
    }
    for line in &source[old_pos..] {
        applied.text.push_str(line);
    }
    applied
}

fn display_rows(display: &str) -> Vec<Row<'_>> {
    display
        .split_terminator('\n')
        .filter(|line| line.trim_start_matches(' ') != "...")
        .map(|line| {
            let (sign, rest) = line.split_at(1);
            let (number, text) = rest.trim_start_matches(' ').split_once(' ').unwrap();
            (sign.chars().next().unwrap(), number.parse().unwrap(), text)
        })
        .collect()
}

fn check_pair(old: &str, new: &str) {
    for context in 0..=10 {
        let patch = generate_unified_patch("f.txt", old, new, context);
        let applied = apply_patch(old, &patch);
        assert_eq!(applied.text, new, "old={old:?} context={context}");
        let (display, first_change) = generate_diff_string(old, new, context);
        assert_eq!((display_rows(&display), first_change), (applied.rows, applied.first_change), "{patch:?}");
    }
}

#[test]
fn patches_apply_and_display_lines_match_the_hunks() {
    const LINES: &[&str] = &["\n", "\r\n", "é中😀\n", "a\rb\n", "...\r\n", "\0\n", " +1\n", "a\n", "@@\n"];
    for (old, new) in [("", ""), ("", "\r"), ("é\r\n", ""), ("a\r\nb\né", "é\nb\r\né"), ("a\n", "a")] {
        check_pair(old, new);
    }
    let mut rng = Xorshift(0x6a09_e667);
    for _ in 0..128 {
        let old: Vec<_> = (0..rng.pick(40)).map(|_| LINES[rng.pick(LINES.len())]).collect();
        let mut new = old.clone();
        for _ in 0..rng.pick(9) {
            let at = rng.pick(new.len() + 1);
            if at < new.len() && rng.pick(2) == 0 {
                new.remove(at);
            } else {
                new.insert(at, LINES[rng.pick(LINES.len())]);
            }
        }
        let (mut old, mut new) = (old.concat(), new.concat());
        for text in [&mut old, &mut new] {
            if rng.pick(2) == 0 && text.ends_with('\n') {
                text.pop();
            }
        }
        check_pair(&old, &new);
    }
}
