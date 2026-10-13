//! Kitty image upload cache: images already uploaded are placed again without sending their data.

use std::collections::HashSet;

use super::{AltScreenState, CachedKittyImage};
use crate::terminal_image::{delete_kitty_image, get_kitty_image_placement};

const MAX_CACHED_OFFSCREEN_KITTY_IMAGES: usize = 16;
const MAX_CACHED_OFFSCREEN_KITTY_TRANSMISSION_BYTES: usize = 32 * 1024 * 1024;
const MAX_CACHED_OFFSCREEN_KITTY_DECODED_BYTES: usize = 64 * 1024 * 1024;

/// Replaces lines of already uploaded images with placement-only commands and evicts offscreen
/// images over the cache limits. Returns the lines and the deletion commands for evicted images.
pub(super) fn prepare_kitty_screen(alt: &mut AltScreenState, screen: &[String]) -> (Vec<String>, String) {
    let mut visible: HashSet<u32> = HashSet::new();
    let lines = screen
        .iter()
        .map(|line| {
            let Some(placement) = get_kitty_image_placement(line) else { return line.clone() };
            visible.insert(placement.image_id);
            let next = CachedKittyImage {
                transmission_generation: placement.transmission_generation,
                transmission_bytes: placement.transmission_bytes,
                estimated_decoded_bytes: placement.estimated_decoded_bytes as usize,
            };
            let index = alt.uploaded_kitty_images.iter().position(|(id, _)| *id == placement.image_id);
            let cached = index.map(|index| alt.uploaded_kitty_images.remove(index).1);
            alt.uploaded_kitty_images.push((placement.image_id, next));
            match cached {
                Some(cached) if cached.transmission_generation == placement.transmission_generation => {
                    placement.replacement_line
                }
                _ => line.clone(),
            }
        })
        .collect();

    let offscreen = |images: &[(u32, CachedKittyImage)]| {
        images.iter().filter(|(id, _)| !visible.contains(id)).fold((0, 0, 0), |(count, sent, decoded), (_, image)| {
            (count + 1, sent + image.transmission_bytes, decoded + image.estimated_decoded_bytes)
        })
    };
    let (mut count, mut sent, mut decoded) = offscreen(&alt.uploaded_kitty_images);
    let mut evicted = String::new();
    let mut index = 0;
    while index < alt.uploaded_kitty_images.len() {
        if count <= MAX_CACHED_OFFSCREEN_KITTY_IMAGES
            && sent <= MAX_CACHED_OFFSCREEN_KITTY_TRANSMISSION_BYTES
            && decoded <= MAX_CACHED_OFFSCREEN_KITTY_DECODED_BYTES
        {
            break;
        }
        let (id, image) = alt.uploaded_kitty_images[index];
        if visible.contains(&id) {
            index += 1;
            continue;
        }
        evicted.push_str(&delete_kitty_image(id));
        alt.uploaded_kitty_images.remove(index);
        count -= 1;
        sent -= image.transmission_bytes;
        decoded -= image.estimated_decoded_bytes;
    }
    (lines, evicted)
}
