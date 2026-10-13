//! Ray casts the model's blocks into braille cells.

use super::math::{DOT_BITS, Rgb, Vec3, clamp01, half_vector, is_light, light_direction, rotation};

/// An axis-aligned block in object space.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockBox {
    pub min: Vec3,
    pub max: Vec3,
    pub color: Rgb,
}

/// Where the model is and how it is turned.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pose {
    pub center_x: f64,
    pub center_y: f64,
    /// Braille dots per model pixel at depth 0.
    pub scale: f64,
    pub yaw: f64,
    pub pitch: f64,
    pub roll: f64,
}

/// An inclusive cell rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellRect {
    pub min_x: usize,
    pub min_y: usize,
    pub max_x: usize,
    pub max_y: usize,
}

struct Face {
    axis: usize,
    /// Plane coordinate on `axis` in object space.
    plane: f64,
    u_min: f64,
    u_max: f64,
    v_min: f64,
    v_max: f64,
    /// Projected bounds in braille dots, inclusive.
    min_x: usize,
    min_y: usize,
    max_x: usize,
    max_y: usize,
    red: f64,
    green: f64,
    blue: f64,
}

/// Renders the model's blocks into braille cells. Only faces that point at the camera and are not covered by
/// a touching block are drawn; each face is rasterized by intersecting each dot's ray with the face's plane,
/// with a depth buffer resolving overlaps. Buffers are reused between frames.
pub struct BlockRaster {
    /// Braille dot bits per cell.
    pub bits: Vec<u8>,
    /// Lit dots per cell.
    pub counts: Vec<u8>,
    /// Summed RGB of the lit dots per cell.
    pub rgb: Vec<f32>,
    /// Halo tint strength per cell, from 0 to 1. Only set when a halo was requested.
    pub halo_amount: Vec<f32>,
    /// Halo color per cell.
    pub halo_rgb: Vec<f32>,
    pub(super) width: usize,
    pub(super) height: usize,
    /// Ray parameter of the nearest hit per dot; smaller is nearer.
    depth: Vec<f32>,
    /// Face index + 1 of the nearest hit per dot, 0 for none.
    face_ids: Vec<u32>,
    /// Cells written by the last frame, cleared before the next one.
    pub(super) dirty: Option<CellRect>,
    /// Cells with a halo from the last frame, cleared before the next one.
    pub(super) halo_dirty: Option<CellRect>,
    camera_distance: f64,
}

/// Zeroes one value per cell and three per cell in `rect`.
fn clear_rect<T: Copy + Default>(rect: CellRect, width: usize, values: &mut [T], triples: &mut [f32]) {
    for row in rect.min_y..=rect.max_y {
        let (start, end) = (row * width + rect.min_x, row * width + rect.max_x + 1);
        values[start..end].fill(T::default());
        triples[start * 3..end * 3].fill(0.0);
    }
}

fn grow(rect: Option<CellRect>, x: usize, y: usize) -> CellRect {
    match rect {
        None => CellRect { min_x: x, min_y: y, max_x: x, max_y: y },
        Some(rect) => CellRect {
            min_x: rect.min_x.min(x),
            min_y: rect.min_y.min(y),
            max_x: rect.max_x.max(x),
            max_y: rect.max_y.max(y),
        },
    }
}

impl BlockRaster {
    pub fn new(camera_distance: f64) -> Self {
        Self {
            bits: Vec::new(),
            counts: Vec::new(),
            rgb: Vec::new(),
            halo_amount: Vec::new(),
            halo_rgb: Vec::new(),
            width: 0,
            height: 0,
            depth: Vec::new(),
            face_ids: Vec::new(),
            dirty: None,
            halo_dirty: None,
            camera_distance,
        }
    }

    pub fn render(
        &mut self,
        width: usize,
        height: usize,
        pose: &Pose,
        boxes: &[BlockBox],
        background: Rgb,
        halo_strength: f64,
    ) {
        let dot_width = width * 2;
        let dot_height = height * 4;
        if width != self.width || height != self.height {
            self.width = width;
            self.height = height;
            self.bits = vec![0; width * height];
            self.counts = vec![0; width * height];
            self.rgb = vec![0.0; width * height * 3];
            self.halo_amount = vec![0.0; width * height];
            self.halo_rgb = vec![0.0; width * height * 3];
            self.halo_dirty = None;
            self.depth = vec![f32::INFINITY; dot_width * dot_height];
            self.face_ids = vec![0; dot_width * dot_height];
            self.dirty = None;
        } else if let Some(rect) = self.dirty.take() {
            clear_rect(rect, width, &mut self.bits, &mut self.rgb);
            for row in rect.min_y..=rect.max_y {
                self.counts[row * width + rect.min_x..=row * width + rect.max_x].fill(0);
            }
        }
        if let Some(rect) = self.halo_dirty.take() {
            clear_rect(rect, width, &mut self.halo_amount, &mut self.halo_rgb);
        }

        let m = rotation(pose.yaw, pose.pitch, pose.roll);
        let camera_distance = self.camera_distance;
        // The camera sits at (0, 0, camera_distance) in camera space; object space is the transpose rotation.
        let origin = [m[6] * camera_distance, m[7] * camera_distance, m[8] * camera_distance];
        let light = is_light(background);
        let faces = self.visible_faces(&m, &origin, pose, boxes, (dot_width, dot_height), light);
        if faces.is_empty() {
            return;
        }
        self.cast_rays(&faces, &m, &origin, pose, dot_width);

        let min_x = faces.iter().map(|face| face.min_x).min().unwrap_or(dot_width);
        let min_y = faces.iter().map(|face| face.min_y).min().unwrap_or(dot_height);
        let max_x = faces.iter().map(|face| face.max_x).max().unwrap_or(0);
        let max_y = faces.iter().map(|face| face.max_y).max().unwrap_or(0);
        // Shade the hit dots, pack them into braille cells, and reset the dot buffers for the next frame.
        let mut cells: Option<CellRect> = None;
        for dot_y in min_y..=max_y {
            let row_start = dot_y * dot_width;
            for dot_x in min_x..=max_x {
                let index = row_start + dot_x;
                let id = self.face_ids[index] as usize;
                if id != 0 {
                    let face = &faces[id - 1];
                    // Points farther from the camera fade slightly toward the background for depth.
                    let depth = f64::from(self.depth[index]);
                    let fog = clamp01(0.15 - camera_distance * (1.0 - depth) * 0.12) * if light { 0.5 } else { 1.0 };
                    self.face_ids[index] = 0;
                    self.depth[index] = f32::INFINITY;
                    let (cell_x, cell_y) = (dot_x >> 1, dot_y >> 2);
                    let cell = cell_y * width + cell_x;
                    self.bits[cell] |= DOT_BITS[(dot_y & 3) * 2 + (dot_x & 1)];
                    self.counts[cell] = self.counts[cell].wrapping_add(1);
                    let color = [face.red, face.green, face.blue];
                    for channel in 0..3 {
                        let shaded = color[channel] + (background[channel] - color[channel]) * fog;
                        let slot = &mut self.rgb[cell * 3 + channel];
                        *slot = (f64::from(*slot) + shaded) as f32;
                    }
                    cells = Some(grow(cells, cell_x, cell_y));
                }
            }
        }
        self.dirty = cells;
        if halo_strength > 0.0 {
            self.render_halo(halo_strength);
        }
    }

    /// Records the nearest face per dot in the depth and face buffers.
    fn cast_rays(&mut self, faces: &[Face], m: &[f64; 9], origin: &Vec3, pose: &Pose, dot_width: usize) {
        let (center_x, center_y, scale) = (pose.center_x, pose.center_y, pose.scale);
        let camera_distance = self.camera_distance;
        for (face_index, face) in faces.iter().enumerate() {
            let a = face.axis;
            let (u, v) = ((a + 1) % 3, (a + 2) % 3);
            let (origin_a, origin_u, origin_v) = (origin[a], origin[u], origin[v]);
            // The ray direction in object space is linear in the dot position, so it is stepped per dot.
            let (step_a, step_u, step_v) = (m[a] / scale, m[u] / scale, m[v] / scale);
            let sx = (face.min_x as f64 + 0.5 - center_x) / scale;
            for dot_y in face.min_y..=face.max_y {
                let sy = (dot_y as f64 + 0.5 - center_y) / scale;
                let mut direction_a = m[a] * sx + m[3 + a] * sy - m[6 + a] * camera_distance;
                let mut direction_u = m[u] * sx + m[3 + u] * sy - m[6 + u] * camera_distance;
                let mut direction_v = m[v] * sx + m[3 + v] * sy - m[6 + v] * camera_distance;
                for index in dot_y * dot_width + face.min_x..=dot_y * dot_width + face.max_x {
                    let t = (face.plane - origin_a) / direction_a;
                    if t > 0.0 && t < f64::from(self.depth[index]) {
                        let hit_u = origin_u + t * direction_u;
                        let hit_v = origin_v + t * direction_v;
                        if hit_u >= face.u_min && hit_u <= face.u_max && hit_v >= face.v_min && hit_v <= face.v_max {
                            self.depth[index] = t as f32;
                            self.face_ids[index] = face_index as u32 + 1;
                        }
                    }
                    direction_a += step_a;
                    direction_u += step_u;
                    direction_v += step_v;
                }
            }
        }
    }

    fn visible_faces(
        &self,
        m: &[f64; 9],
        origin: &Vec3,
        pose: &Pose,
        boxes: &[BlockBox],
        (dot_width, dot_height): (usize, usize),
        light_background: bool,
    ) -> Vec<Face> {
        let mut faces = Vec::new();
        let camera_distance = self.camera_distance;
        let (light, half) = (light_direction(), half_vector());
        for (box_index, block) in boxes.iter().enumerate() {
            for a in 0..3 {
                let (u, v) = ((a + 1) % 3, (a + 2) % 3);
                for side in [-1.0, 1.0] {
                    let plane = if side > 0.0 { block.max[a] } else { block.min[a] };
                    // Back faces point away from the camera.
                    if (origin[a] - plane) * side <= 0.0 {
                        continue;
                    }
                    // Faces pressed against a neighboring block are hidden. Positions are exact while blocks rest.
                    let covered = boxes.iter().enumerate().any(|(other_index, other)| {
                        other_index != box_index
                            && (if side > 0.0 { other.min[a] } else { other.max[a] }) == plane
                            && other.min[u] <= block.min[u]
                            && other.max[u] >= block.max[u]
                            && other.min[v] <= block.min[v]
                            && other.max[v] >= block.max[v]
                    });
                    if covered {
                        continue;
                    }
                    let (mut min_x, mut min_y) = (f64::INFINITY, f64::INFINITY);
                    let (mut max_x, mut max_y) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
                    for corner_u in [block.min[u], block.max[u]] {
                        for corner_v in [block.min[v], block.max[v]] {
                            let mut corner = [0.0; 3];
                            corner[a] = plane;
                            corner[u] = corner_u;
                            corner[v] = corner_v;
                            let x = m[0] * corner[0] + m[1] * corner[1] + m[2] * corner[2];
                            let y = m[3] * corner[0] + m[4] * corner[1] + m[5] * corner[2];
                            let z = m[6] * corner[0] + m[7] * corner[1] + m[8] * corner[2];
                            let perspective = (pose.scale * camera_distance) / (camera_distance - z);
                            let (screen_x, screen_y) =
                                (pose.center_x + x * perspective, pose.center_y + y * perspective);
                            min_x = min_x.min(screen_x);
                            min_y = min_y.min(screen_y);
                            max_x = max_x.max(screen_x);
                            max_y = max_y.max(screen_y);
                        }
                    }
                    let face_min_x = min_x.floor().max(0.0);
                    let face_min_y = min_y.floor().max(0.0);
                    let face_max_x = max_x.ceil().min(dot_width as f64 - 1.0);
                    let face_max_y = max_y.ceil().min(dot_height as f64 - 1.0);
                    if face_max_x < face_min_x || face_max_y < face_min_y {
                        continue;
                    }

                    // Faces are flat, so lighting is computed once per face. The normal in camera space is a
                    // column of the rotation matrix.
                    let (nx, ny, nz) = (m[a] * side, m[3 + a] * side, m[6 + a] * side);
                    let diffuse = (nx * light[0] + ny * light[1] + nz * light[2]).max(0.0);
                    let rim = (nx * 0.8 - nz * 0.3).max(0.0);
                    // On light backgrounds, highlights toward white would vanish, so faces only get darker there.
                    let specular = if light_background {
                        0.0
                    } else {
                        (nx * half[0] + ny * half[1] + nz * half[2]).max(0.0).powi(24) * 0.6 * 255.0
                    };
                    let shade = if light_background {
                        (0.55 + diffuse * 0.45 + rim * 0.1).min(1.0)
                    } else {
                        0.45 + diffuse * 0.78 + rim * 0.25
                    };
                    faces.push(Face {
                        axis: a,
                        plane,
                        u_min: block.min[u],
                        u_max: block.max[u],
                        v_min: block.min[v],
                        v_max: block.max[v],
                        min_x: face_min_x as usize,
                        min_y: face_min_y as usize,
                        max_x: face_max_x as usize,
                        max_y: face_max_y as usize,
                        red: (block.color[0] * shade + specular).min(255.0),
                        green: (block.color[1] * shade + specular).min(255.0),
                        blue: (block.color[2] * shade + specular).min(255.0),
                    });
                }
            }
        }
        faces
    }
}
