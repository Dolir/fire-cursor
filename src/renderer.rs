use glam::Vec4;

#[derive(Clone, Copy)]
pub struct Instance {
    pub pos: [f32; 2],
    pub size: f32,
    pub color: [f32; 4], // premultiplied RGBA
}

impl Instance {
    pub fn new(pos: [f32; 2], size: f32, color_premul: Vec4) -> Self {
        Self {
            pos,
            size,
            color: [color_premul.x, color_premul.y, color_premul.z, color_premul.w],
        }
    }
}

pub struct Renderer {
    width: u32,
    height: u32,
    compact_buffer: Vec<u8>,
    was_empty: bool,
    dirty_rect: Option<(u32, u32, u32, u32)>, // min_x, min_y, max_x, max_y
}

impl Renderer {
    pub fn new(width: u32, height: u32) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        Self {
            width,
            height,
            compact_buffer: vec![0u8; (width as usize) * 4 * (height as usize)],
            was_empty: false,
            dirty_rect: None,
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        let width = width.max(1);
        let height = height.max(1);
        if self.width == width && self.height == height {
            return;
        }
        self.width = width;
        self.height = height;
        self.compact_buffer = vec![0u8; (width as usize) * 4 * (height as usize)];
        self.was_empty = false;
        self.dirty_rect = None;
    }

    pub fn render(
        &mut self,
        instances: &[Instance],
        overlay: &mut crate::window::OverlayWindow,
    ) -> anyhow::Result<()> {
        if instances.is_empty() {
            if self.was_empty {
                return Ok(());
            }
            self.was_empty = true;
        } else {
            self.was_empty = false;
        }

        // Clear only the dirty rectangle from the previous frame.
        if let Some((min_x, min_y, max_x, max_y)) = self.dirty_rect {
            let row_bytes = (self.width as usize) * 4;
            let clear_width_bytes = ((max_x - min_x) as usize) * 4;
            for y in min_y..max_y {
                let offset = (y as usize) * row_bytes + (min_x as usize) * 4;
                self.compact_buffer[offset..offset + clear_width_bytes].fill(0);
            }
        } else if !self.was_empty {
            self.compact_buffer.fill(0);
        }

        if instances.is_empty() {
            self.dirty_rect = None;
            return overlay.present_layered_bgra8_premul(self.width, self.height, &self.compact_buffer);
        }

        let mut new_min_x = self.width;
        let mut new_min_y = self.height;
        let mut new_max_x = 0;
        let mut new_max_y = 0;

        let width_f = self.width as f32;
        let height_f = self.height as f32;

        // Soft-Rasterize instances based on a fragment-like equation
        for inst in instances {
            let r = inst.size / 2.0;
            
            let min_x_f = (inst.pos[0] - r).max(0.0);
            let min_y_f = (inst.pos[1] - r).max(0.0);
            let max_x_f = (inst.pos[0] + r).min(width_f);
            let max_y_f = (inst.pos[1] + r).min(height_f);

            let min_xi = min_x_f as u32;
            let min_yi = min_y_f as u32;
            let max_xi = (max_x_f.ceil() as u32).min(self.width);
            let max_yi = (max_y_f.ceil() as u32).min(self.height);
            
            if min_xi >= max_xi || min_yi >= max_yi {
                continue;
            }

            new_min_x = new_min_x.min(min_xi);
            new_min_y = new_min_y.min(min_yi);
            new_max_x = new_max_x.max(max_xi);
            new_max_y = new_max_y.max(max_yi);

            let cx = inst.pos[0];
            let cy = inst.pos[1];
            
            let color_r_f = inst.color[0];
            let color_g_f = inst.color[1];
            let color_b_f = inst.color[2];
            let color_a_f = inst.color[3];

            let row_bytes = (self.width as usize) * 4;

            for y in min_yi..max_yi {
                let dy = y as f32 + 0.5 - cy;
                let dy2 = dy * dy;
                
                let row_start = (y as usize) * row_bytes;
                
                for x in min_xi..max_xi {
                    let dx = x as f32 + 0.5 - cx;
                    let dist = (dx * dx + dy2).sqrt();
                    
                    let r_norm = dist / inst.size;
                    
                    let edge0 = 0.55;
                    let edge1 = 0.10;
                    
                    let soft = if r_norm >= edge0 {
                        0.0
                    } else if r_norm <= edge1 {
                        1.0
                    } else {
                        let t = (r_norm - edge0) / (edge1 - edge0);
                        t * t * (3.0 - 2.0 * t)
                    };
                    
                    if soft <= 0.001 {
                        continue;
                    }
                    
                    let a = color_a_f * soft;
                    let r = color_r_f * soft;
                    let g = color_g_f * soft;
                    let b = color_b_f * soft;
                    
                    let pixel_idx = row_start + (x as usize) * 4;
                    
                    let dst_b = self.compact_buffer[pixel_idx] as f32 / 255.0;
                    let dst_g = self.compact_buffer[pixel_idx + 1] as f32 / 255.0;
                    let dst_r = self.compact_buffer[pixel_idx + 2] as f32 / 255.0;
                    let dst_a = self.compact_buffer[pixel_idx + 3] as f32 / 255.0;
                    
                    let out_b = b + dst_b * (1.0 - a);
                    let out_g = g + dst_g * (1.0 - a);
                    let out_r = r + dst_r * (1.0 - a);
                    let out_a = a + dst_a * (1.0 - a);
                    
                    self.compact_buffer[pixel_idx] = (out_b.clamp(0.0, 1.0) * 255.0) as u8;
                    self.compact_buffer[pixel_idx + 1] = (out_g.clamp(0.0, 1.0) * 255.0) as u8;
                    self.compact_buffer[pixel_idx + 2] = (out_r.clamp(0.0, 1.0) * 255.0) as u8;
                    self.compact_buffer[pixel_idx + 3] = (out_a.clamp(0.0, 1.0) * 255.0) as u8;
                }
            }
        }

        if new_min_x < new_max_x && new_min_y < new_max_y {
            self.dirty_rect = Some((new_min_x, new_min_y, new_max_x, new_max_y));
        } else {
            self.dirty_rect = None;
        }

        overlay.present_layered_bgra8_premul(self.width, self.height, &self.compact_buffer)
    }
}
