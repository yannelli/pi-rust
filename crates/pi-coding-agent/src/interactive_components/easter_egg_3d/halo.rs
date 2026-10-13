//! The light-background halo: the logo's coverage and color blurred into neighboring cells.

use super::math::{HALO_LEVELS, HALO_RADIUS_X, HALO_RADIUS_Y, LOGO_COLOR_STEP, js_round};
use super::raster::{BlockRaster, CellRect};

/// Normalized tent filter weights over `-radius..=radius`.
fn tent(radius: i64) -> Vec<f64> {
    let weights: Vec<f64> = (0..=radius * 2).map(|i| (radius + 1 - (i - radius).abs()) as f64).collect();
    let total: f64 = weights.iter().sum();
    weights.into_iter().map(|weight| weight / total).collect()
}

impl BlockRaster {
    /// Blurs the logo's coverage and color over neighboring cells with a separable tent filter, so the tint
    /// falls off smoothly past the logo's edges.
    pub(super) fn render_halo(&mut self, strength: f64) {
        let Some(cells) = self.dirty else {
            return;
        };
        let (width, height) = (self.width as i64, self.height as i64);
        let (cells_min_x, cells_max_x) = (cells.min_x as i64, cells.max_x as i64);
        let (cells_min_y, cells_max_y) = (cells.min_y as i64, cells.max_y as i64);
        let min_x = (cells_min_x - HALO_RADIUS_X).max(0);
        let max_x = (cells_max_x + HALO_RADIUS_X).min(width - 1);
        let min_y = (cells_min_y - HALO_RADIUS_Y).max(0);
        let max_y = (cells_max_y + HALO_RADIUS_Y).min(height - 1);
        let region_width = (max_x - min_x + 1) as usize;
        let weights_x = tent(HALO_RADIUS_X);
        let weights_y = tent(HALO_RADIUS_Y);

        // Horizontal pass over the rows that contain the logo: coverage and premultiplied color per cell.
        let rows = (cells_max_y - cells_min_y + 1) as usize;
        let mut horizontal = vec![0f32; rows * region_width * 4];
        for y in cells_min_y..=cells_max_y {
            for x in min_x..=max_x {
                let mut sums = [0.0f64; 4];
                for dx in -HALO_RADIUS_X..=HALO_RADIUS_X {
                    let source_x = x + dx;
                    if source_x < cells_min_x || source_x > cells_max_x {
                        continue;
                    }
                    let source = (y * width + source_x) as usize;
                    let count = self.counts[source];
                    if count == 0 {
                        continue;
                    }
                    let weight = weights_x[(dx + HALO_RADIUS_X) as usize] / 8.0;
                    sums[0] += f64::from(count) * weight;
                    for channel in 0..3 {
                        sums[channel + 1] += f64::from(self.rgb[source * 3 + channel]) * weight;
                    }
                }
                let target = (((y - cells_min_y) * region_width as i64 + (x - min_x)) * 4) as usize;
                for (offset, sum) in sums.into_iter().enumerate() {
                    horizontal[target + offset] = sum as f32;
                }
            }
        }

        // Vertical pass into the halo buffers.
        for y in min_y..=max_y {
            for x in min_x..=max_x {
                let mut sums = [0.0f64; 4];
                for dy in -HALO_RADIUS_Y..=HALO_RADIUS_Y {
                    let source_y = y + dy;
                    if source_y < cells_min_y || source_y > cells_max_y {
                        continue;
                    }
                    let weight = weights_y[(dy + HALO_RADIUS_Y) as usize];
                    let source = (((source_y - cells_min_y) * region_width as i64 + (x - min_x)) * 4) as usize;
                    for (offset, sum) in sums.iter_mut().enumerate() {
                        *sum += f64::from(horizontal[source + offset]) * weight;
                    }
                }
                let target = (y * width + x) as usize;
                let [mut coverage, mut red, mut green, mut blue] = sums;
                // Cells under the logo keep at least their own coverage, so faces stay solidly tinted.
                let count = f64::from(self.counts[target]);
                if count > 0.0 && count / 8.0 > coverage {
                    coverage = count / 8.0;
                    red = (f64::from(self.rgb[target * 3]) / count) * coverage;
                    green = (f64::from(self.rgb[target * 3 + 1]) / count) * coverage;
                    blue = (f64::from(self.rgb[target * 3 + 2]) / count) * coverage;
                }
                if coverage <= 0.0 {
                    continue;
                }
                // Quantized so neighboring cells usually share a background and its escape sequence.
                let amount = js_round(strength * coverage.min(1.0).powf(0.7) * HALO_LEVELS) / HALO_LEVELS;
                if amount <= 0.0 {
                    continue;
                }
                self.halo_amount[target] = amount as f32;
                for (channel, value) in [red, green, blue].into_iter().enumerate() {
                    let quantized = js_round(value / coverage / LOGO_COLOR_STEP) * LOGO_COLOR_STEP;
                    self.halo_rgb[target * 3 + channel] = quantized as f32;
                }
            }
        }
        self.halo_dirty = Some(CellRect {
            min_x: min_x as usize,
            min_y: min_y as usize,
            max_x: max_x as usize,
            max_y: max_y as usize,
        });
    }
}
