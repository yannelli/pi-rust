//! Bounded suffix/prefix overlap search, measured in UTF-16 code units like JavaScript strings.

fn index_of<T: Eq>(haystack: &[T], needle: &[T], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return (from <= haystack.len()).then_some(from);
    }
    let last = haystack.len().checked_sub(needle.len())?;
    let first = &needle[0];
    (from..=last).find(|&start| haystack[start] == *first && haystack[start..start + needle.len()] == *needle)
}

fn overlap_units<T: Eq>(a: &[T], b: &[T], scan: usize, probe: usize, max_candidates: usize) -> usize {
    if a.is_empty() || b.is_empty() || scan == 0 {
        return 0;
    }
    let tail = if a.len() > scan { &a[a.len() - scan..] } else { a };
    // A long head finds large overlaps with few candidates; one unit then finds any overlap.
    for head_len in [probe.min(b.len()), 1] {
        let head = &b[..head_len];
        let mut tried = 0;
        let mut found = index_of(tail, head, 0);
        while let Some(k) = found {
            tried += 1;
            if tried > max_candidates {
                break;
            }
            let n = tail.len() - k;
            if n <= b.len() && tail[k..] == b[..n] {
                return n;
            }
            found = index_of(tail, head, k + 1);
        }
        if head_len == 1 {
            break;
        }
    }
    0
}

/// Longest suffix of `a` that is a prefix of `b`, in UTF-16 units, scanning the last `scan` units.
pub fn overlap(a: &str, b: &str, scan: usize) -> usize {
    overlap_with(a, b, scan, 64, 8)
}

/// [`overlap`] with an explicit probe length and candidate bound.
pub fn overlap_with(a: &str, b: &str, scan: usize, probe: usize, max_candidates: usize) -> usize {
    if a.is_ascii() && b.is_ascii() {
        return overlap_units(a.as_bytes(), b.as_bytes(), scan, probe, max_candidates);
    }
    let a: Vec<u16> = a.encode_utf16().collect();
    let b: Vec<u16> = b.encode_utf16().collect();
    overlap_units(&a, &b, scan, probe, max_candidates)
}
