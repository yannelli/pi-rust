//! One frame of [`EasterEgg3dAnimation`]: the dissolving screen, the model, the stars, and the hint.

use std::collections::HashMap;

use pi_tui::colors::{background_ansi, foreground_ansi, rgb_color};

use super::animation::{EasterEgg3dAnimation, Star};
use super::math::{
    BACKGROUND_FADE, DEPTH, DISSOLVE_END, DOT_BITS, DUST_DURATION, FLY_START, LIGHT_HALO_STRENGTH, LOGO_COLOR_STEP,
    Rgb, STAR_ALPHA_LEVELS, STAR_FLICKER_LEVELS, Vec3, WAVE_JITTER, WAVE_SPREAD, braille, clamp01, ease_out, hash,
    is_light, js_round, mix, smooth, to_int32,
};
use super::raster::{BlockBox, BlockRaster, Pose};
use super::screen::{ScreenCell, glyph_ink, parse_screen};
use crate::theme::theme;

/// Packs a color into one integer key, like upstream's bit shifts on rounded channels.
fn pack(color: Rgb) -> i64 {
    let [r, g, b] = color.map(|channel| to_int32(js_round(channel)));
    i64::from((r.wrapping_shl(16)) | (g.wrapping_shl(8)) | b)
}

fn ansi(cache: &mut HashMap<i64, String>, key: i64, is_background: bool) -> String {
    let cache_key = key * 2 + i64::from(is_background);
    cache
        .entry(cache_key)
        .or_insert_with(|| {
            let channel = |shift: i64| ((key >> shift) & 255) as f64;
            let Ok(color) = rgb_color(channel(16), channel(8), channel(0)) else {
                return String::new();
            };
            let mode = theme().get_color_mode();
            if is_background { background_ansi(&color, mode) } else { foreground_ansi(&color, mode) }
        })
        .clone()
}

/// Builds one output line, emitting color changes only when they differ from the last ones.
struct LineWriter<'a> {
    line: String,
    current_fg: i64,
    current_bg: i64,
    cache: &'a mut HashMap<i64, String>,
}

impl LineWriter<'_> {
    fn emit(&mut self, text: &str, fg: Option<Rgb>, bg: Option<Rgb>) {
        let fg_key = fg.map_or(-1, pack);
        let bg_key = bg.map_or(-1, pack);
        if fg_key != self.current_fg && text != " " {
            let code = if fg.is_some() { ansi(self.cache, fg_key, false) } else { "\x1b[39m".to_string() };
            self.line.push_str(&code);
            self.current_fg = fg_key;
        }
        if bg_key != self.current_bg {
            let code = if bg.is_some() { ansi(self.cache, bg_key, true) } else { "\x1b[49m".to_string() };
            self.line.push_str(&code);
            self.current_bg = bg_key;
        }
        self.line.push_str(text);
    }
}

fn halo(raster: &BlockRaster, index: usize, base: Option<Rgb>, background: Rgb) -> Option<Rgb> {
    let amount = f64::from(raster.halo_amount[index]);
    if amount <= 0.0 {
        return base;
    }
    let color = [0, 1, 2].map(|channel| f64::from(raster.halo_rgb[index * 3 + channel]));
    Some(mix(base.unwrap_or(background), color, amount))
}

impl EasterEgg3dAnimation {
    pub(super) fn render_frame(&mut self, width: usize) -> Vec<String> {
        let height = self.terminal_rows();
        let (foreground, background) = (self.foreground, self.background);
        self.mark_rendered();
        if self.screen_size != Some((width, height)) {
            self.screen_size = Some((width, height));
            self.cells = self.prepare_cells(width, height);
            self.stars = Self::prepare_stars(width, height);
        }

        // Dissolve time runs forward on entry and backward on exit, so the screen reassembles in reverse order.
        let (dissolve_time, pose, hint_alpha, star_alpha, offsets) = match &self.exit {
            Some(exit) => {
                let progress = self.exit_progress();
                let landing = clamp01(progress / 0.8);
                let settle = 1.0 - smooth(landing);
                let mut pose = self.pose(width, height, Self::fly_progress(exit.time) * settle, exit.time);
                pose.yaw = exit.yaw + (exit.target_yaw - exit.yaw) * ease_out(landing);
                pose.pitch *= settle;
                pose.roll *= settle;
                // Blocks are home well before the logo lands.
                let gather = 1.0 - smooth(progress / 0.5);
                let offsets: Vec<Vec3> = exit.offsets.iter().map(|offset| offset.map(|v| v * gather)).collect();
                (
                    exit.time.min(DISSOLVE_END) * (1.0 - progress),
                    pose,
                    Self::hint_alpha(exit.time) * (1.0 - smooth(progress / 0.2)),
                    Self::star_alpha(exit.time) * (1.0 - smooth(progress / 0.3)),
                    offsets,
                )
            }
            None => {
                let time = self.elapsed();
                let pose = self.pose(width, height, Self::fly_progress(time), time);
                (time, pose, Self::hint_alpha(time), Self::star_alpha(time), self.block_offsets(time))
            }
        };
        self.draw(width, height, &pose, &offsets, (dissolve_time, hint_alpha, star_alpha), (foreground, background))
    }

    fn draw(
        &mut self,
        width: usize,
        height: usize,
        pose: &Pose,
        offsets: &[Vec3],
        (dissolve_time, hint_alpha, star_alpha): (f64, f64, f64),
        (foreground, background): (Rgb, Rgb),
    ) -> Vec<String> {
        let (columns, rows) = (self.model.columns as f64, self.model.rows as f64);
        let boxes: Vec<BlockBox> = self
            .model
            .blocks
            .iter()
            .zip(offsets)
            .map(|(block, &[dx, dy, dz])| {
                let x = block.home[0] as f64 - columns / 2.0 + dx;
                let y = block.home[1] as f64 - rows / 2.0 + dy;
                BlockBox {
                    min: [x, y, dz - DEPTH / 2.0],
                    max: [x + 1.0, y + 1.0, dz + DEPTH / 2.0],
                    color: block.color,
                }
            })
            .collect();
        let halo_strength = if is_light(background) { LIGHT_HALO_STRENGTH } else { 0.0 };
        self.raster.render(width, height, pose, &boxes, background, halo_strength);

        let background_fade = smooth(dissolve_time / BACKGROUND_FADE);
        let text_fade = smooth(dissolve_time / 0.6) * 0.3;
        // Once the screen has fully dissolved, the text layer is empty and can be skipped.
        let text_active = dissolve_time < DISSOLVE_END || background_fade < 1.0;
        let hint = Self::hint(width, height, hint_alpha, background);
        let frame = (dissolve_time * 14.0).floor() as i64;
        let now = self.elapsed();
        // Star brightness is quantized, so a row only changes when one of its stars steps to another level
        // instead of on every frame.
        let star_level = js_round(star_alpha * STAR_ALPHA_LEVELS) / STAR_ALPHA_LEVELS;
        let starfield = |star: Option<&Star>| -> (String, Option<Rgb>) {
            let Some(star) = star.filter(|_| star_level > 0.0) else {
                return (" ".to_string(), None);
            };
            let wave = 0.5 + 0.5 * (now * star.speed + star.phase).sin();
            let flicker = js_round(wave * STAR_FLICKER_LEVELS) / STAR_FLICKER_LEVELS;
            let amount = star.strength * (0.7 + 0.3 * flicker) * star_level;
            (star.glyph.to_string(), Some(mix(background, mix(foreground, star.tint, 0.5), amount)))
        };

        let raster = &self.raster;
        let mut lines = Vec::with_capacity(height);
        for row in 0..height {
            let cells: &[ScreenCell] = self.cells.get(row).map_or(&[], Vec::as_slice);
            let mut writer =
                LineWriter { line: String::new(), current_fg: -1, current_bg: -1, cache: &mut self.ansi_cache };
            let mut column = 0;
            while column < width {
                let index = row * width + column;
                let cell = if text_active { cells.get(column) } else { None };
                let base = cell.and_then(|cell| cell.bg).filter(|_| background_fade < 1.0);
                let cell_bg = halo(raster, index, base.map(|bg| mix(bg, background, background_fade)), background);
                let star = || starfield(self.stars.get(index).and_then(Option::as_ref));
                if let Some(hint) = hint.as_ref().filter(|hint| row as i64 == hint.row)
                    && column >= hint.start
                    && column < hint.start + hint.text.len()
                {
                    let offset = column - hint.start;
                    let color = if offset < hint.key_length { hint.key_color } else { hint.color };
                    writer.emit(&hint.text[offset].to_string(), Some(color), cell_bg);
                    column += 1;
                    continue;
                }
                let dots = raster.bits[index];
                if dots != 0 {
                    // Quantized so neighboring cells on one face usually share a color and its escape sequence.
                    let step = LOGO_COLOR_STEP * f64::from(raster.counts[index]);
                    let color = [0, 1, 2]
                        .map(|channel| js_round(f64::from(raster.rgb[index * 3 + channel]) / step) * LOGO_COLOR_STEP);
                    writer.emit(&braille(dots).to_string(), Some(color), cell_bg);
                    column += 1;
                    continue;
                }
                let Some(cell) = cell else {
                    let (text, fg) = star();
                    writer.emit(&text, fg, cell_bg);
                    column += 1;
                    continue;
                };
                let dust = (dissolve_time - cell.delay) / DUST_DURATION;
                let next_dots = raster.bits.get(index + 1).copied().unwrap_or(0);
                if dust < 0.0 && (cell.width == 1 || (cell.width == 2 && next_dots == 0)) {
                    writer.emit(&cell.text, Some(mix(cell.fg, background, text_fade)), cell_bg);
                    column += cell.width;
                    continue;
                }
                let dot_count =
                    if dust < 0.0 { cell.ink } else { js_round(cell.ink as f64 * (1.0 - clamp01(dust))) as usize };
                if dot_count == 0 {
                    let (text, fg) = star();
                    writer.emit(&text, fg, cell_bg);
                    column += 1;
                    continue;
                }
                // Pick `dot_count` distinct dots; the choice changes a few times per second so the dust shimmers.
                let mut bits = 0u8;
                let mut placed = 0;
                let mut attempt = 0;
                while placed < dot_count && attempt < 32 {
                    let random = hash(cell.seed * 7919 + frame * 131 + attempt);
                    attempt += 1;
                    let bit = DOT_BITS[(random * 8.0).floor() as usize];
                    if bits & bit != 0 {
                        continue;
                    }
                    bits |= bit;
                    placed += 1;
                }
                let fade = text_fade + (1.0 - text_fade) * clamp01(dust).powf(0.8);
                writer.emit(&braille(bits).to_string(), Some(mix(cell.fg, background, fade)), cell_bg);
                column += 1;
            }
            lines.push(writer.line + "\x1b[0m");
        }
        lines
    }

    fn prepare_cells(&self, width: usize, height: usize) -> Vec<Vec<ScreenCell>> {
        let mut cells = parse_screen(&self.screen, width, self.foreground, self.background);
        let (columns, rows, origin) = (self.model.columns as f64, self.model.rows, self.model.origin);
        let (width_f, height_f) = (width as f64, height as f64);
        // The dust spreads out from where the model starts.
        let center_x = origin.map_or(width_f / 2.0, |origin| origin.column as f64 + columns / 2.0);
        let center_y = origin.map_or(height_f / 2.0, |origin| origin.row as f64 + rows as f64 / 4.0);
        let farthest = center_x.max(width_f - center_x).hypot(center_y.max(height_f - center_y) * 2.0);
        for (row, line) in cells.iter_mut().enumerate() {
            for column in 0..line.len() {
                let position = row as i64 * 65_537 + column as i64;
                let seed = hash(position);
                // Wide graphemes share their first column's timing so both halves change together.
                let owner = if line[column].width == 0 && column > 0 { column - 1 } else { column };
                if line[column].width != 0 {
                    let distance = (column as f64 - center_x).hypot((row as f64 - center_y) * 2.0) / farthest;
                    line[column].delay = FLY_START + distance * WAVE_SPREAD + seed * WAVE_JITTER;
                } else {
                    line[column].delay = line[owner].delay;
                }
                line[column].seed = position;
                line[column].ink = glyph_ink(&line[owner].text, seed);
            }
        }
        // The 3D model replaces its half-block rendering on screen.
        if let Some(origin) = origin {
            for line in cells.iter_mut().skip(origin.row).take(rows.div_ceil(2)) {
                for cell in line.iter_mut().skip(origin.column).take(self.model.columns) {
                    cell.text = " ".to_string();
                    cell.width = 1;
                    cell.ink = 0;
                    cell.bg = None;
                }
            }
        }
        cells
    }
}
