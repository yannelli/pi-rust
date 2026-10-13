//! Bounded output chunks and Kitty image placement parsing for the main-screen renderer.

use std::collections::BTreeSet;

use crate::terminal_image::{delete_kitty_image, is_image_line};
use crate::utils::visible_width;

const KITTY_SEQUENCE_PREFIX: &str = "\x1b_G";
pub(crate) const MAX_RENDER_WRITE_BYTES: usize = 1024 * 1024;

/// Streams terminal output in 1 MiB chunks, split on char boundaries.
pub(crate) struct BoundedWriter<'a> {
    buffer: String,
    written: usize,
    write: &'a mut dyn FnMut(&str),
}

impl<'a> BoundedWriter<'a> {
    pub fn new(write: &'a mut dyn FnMut(&str)) -> Self {
        Self { buffer: String::new(), written: 0, write }
    }

    pub fn append(&mut self, value: &str) {
        let mut rest = value;
        while !rest.is_empty() {
            let capacity = MAX_RENDER_WRITE_BYTES - self.buffer.len();
            if capacity == 0 {
                self.flush();
                continue;
            }
            let mut end = rest.len().min(capacity);
            while !rest.is_char_boundary(end) {
                end -= 1;
            }
            if end == 0 {
                self.flush();
                continue;
            }
            self.buffer.push_str(&rest[..end]);
            rest = &rest[end..];
            if self.buffer.len() == MAX_RENDER_WRITE_BYTES {
                self.flush();
            }
        }
    }

    pub fn flush(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        (self.write)(&self.buffer);
        self.written += self.buffer.len();
        self.buffer.clear();
    }

    /// Bytes written so far plus the pending chunk.
    pub fn len(&self) -> usize {
        self.written + self.buffer.len()
    }
}

struct KittyImageHeader {
    ids: Vec<u32>,
    rows: usize,
}

fn parse_kitty_image_header(line: &str) -> Option<KittyImageHeader> {
    let start = line.find(KITTY_SEQUENCE_PREFIX)? + KITTY_SEQUENCE_PREFIX.len();
    let end = start + line[start..].find(';')?;
    let mut ids = Vec::new();
    let mut rows = 1;
    for param in line[start..end].split(',') {
        let Some((key, value)) = param.split_once('=') else { continue };
        let Ok(number) = value.parse::<u64>() else { continue };
        if number == 0 || number > u64::from(u32::MAX) {
            continue;
        }
        match key {
            "i" => ids.push(number as u32),
            "r" => rows = number as usize,
            _ => {}
        }
    }
    Some(KittyImageHeader { ids, rows })
}

pub(crate) fn extract_kitty_image_ids(line: &str) -> Vec<u32> {
    parse_kitty_image_header(line).map(|h| h.ids).unwrap_or_default()
}

fn extract_kitty_image_rows(line: &str) -> usize {
    parse_kitty_image_header(line).map(|h| h.rows).unwrap_or(1)
}

pub(crate) fn collect_kitty_image_ids(lines: &[String]) -> BTreeSet<u32> {
    lines.iter().flat_map(|line| extract_kitty_image_ids(line)).collect()
}

pub(crate) fn delete_kitty_images<'a>(ids: impl IntoIterator<Item = &'a u32>) -> String {
    ids.into_iter().map(|id| delete_kitty_image(*id)).collect()
}

/// Rows an image placement reserves below its anchor line: blank rows up to the image height.
pub(crate) fn kitty_image_reserved_rows(lines: &[String], index: usize, max_index: Option<usize>) -> usize {
    let rows = extract_kitty_image_rows(lines.get(index).map(String::as_str).unwrap_or(""));
    if rows <= 1 {
        return 1;
    }
    let max_index = max_index.unwrap_or(lines.len().saturating_sub(1));
    let max_rows = rows.min((max_index + 1).saturating_sub(index)).min(lines.len() - index);
    let mut reserved = 1;
    while reserved < max_rows {
        let line = lines.get(index + reserved).map(String::as_str).unwrap_or("");
        if is_image_line(line) || visible_width(line) > 0 {
            break;
        }
        reserved += 1;
    }
    reserved
}

/// Widens a changed line range so it covers every image block it touches.
pub(crate) fn expand_changed_range_for_kitty_images(
    first_changed: usize,
    last_changed: usize,
    previous: &[String],
    next: &[String],
) -> (usize, usize) {
    let mut first = first_changed;
    let mut last = last_changed;
    for lines in [previous, next] {
        for i in 0..lines.len() {
            if extract_kitty_image_ids(&lines[i]).is_empty() {
                continue;
            }
            let block_end = i + kitty_image_reserved_rows(lines, i, None) - 1;
            if i >= first_changed || (i <= last_changed && block_end >= first_changed) {
                first = first.min(i);
                last = last.max(block_end);
            }
        }
    }
    (first, last)
}
