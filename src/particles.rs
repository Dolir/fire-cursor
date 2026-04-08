//! Particle simulation for the fire trail.
//!
//! Math overview:
//! - Particles are simulated in screen pixel coordinates.
//! - Y increases downward (screen coordinates), so an "upward" velocity has negative Y.
//! - Each particle has an `age` and `lifetime`; normalized lifetime is `t = age/lifetime`.
//! - Color is a simple piecewise linear gradient:
//!   yellow → orange → red → transparent.

use glam::{Vec2, Vec4};
use rand::Rng;

#[derive(Clone, Copy, Debug)]
pub struct Particle {
    pub pos: Vec2,
    pub vel: Vec2,
    pub age: f32,
    pub lifetime: f32,
    pub size: f32,
}

impl Particle {
    pub fn is_dead(&self) -> bool {
        self.age >= self.lifetime
    }

    pub fn normalized_age(&self) -> f32 {
        if self.lifetime <= 0.0 {
            1.0
        } else {
            (self.age / self.lifetime).clamp(0.0, 1.0)
        }
    }

    /// Premultiplied RGBA color for correct blending on a transparent surface.
    pub fn color_premul(&self) -> Vec4 {
        let t = self.normalized_age();

        // Base colors (non-premultiplied)
        let yellow = Vec4::new(1.0, 0.95, 0.2, 1.0);
        let orange = Vec4::new(1.0, 0.45, 0.05, 1.0);
        let red = Vec4::new(0.9, 0.1, 0.02, 1.0);

        // Piecewise linear gradient + fade out.
        let (rgb, alpha) = if t < 0.30 {
            let k = t / 0.30;
            (yellow.lerp(orange, k), 1.0)
        } else if t < 0.70 {
            let k = (t - 0.30) / 0.40;
            (orange.lerp(red, k), 1.0)
        } else {
            let k = (t - 0.70) / 0.30;
            // Fade alpha in the last segment.
            (red, (1.0 - k).clamp(0.0, 1.0))
        };

        // Premultiply alpha.
        Vec4::new(rgb.x * alpha, rgb.y * alpha, rgb.z * alpha, alpha)
    }
}

pub struct ParticleSystem {
    particles: Vec<Particle>,
    spawn_accumulator: f32,
    pub max_particles: usize,
    pub spawn_rate: f32, // particles / second
}

impl ParticleSystem {
    pub fn new() -> Self {
        Self {
            particles: Vec::with_capacity(4096),
            spawn_accumulator: 0.0,
            max_particles: 8000,
            spawn_rate: 900.0,
        }
    }

    pub fn particles(&self) -> &[Particle] {
        &self.particles
    }

    pub fn update_and_spawn(&mut self, dt: f32, emitter_pos: Vec2) {
        let dt = dt.clamp(0.0, 0.05); // clamp to avoid huge jumps after pauses

        // Spawn based on a rate (stable across frame rates)
        self.spawn_accumulator += self.spawn_rate * dt;
        let to_spawn = self.spawn_accumulator.floor() as usize;
        self.spawn_accumulator -= to_spawn as f32;

        if self.particles.len() < self.max_particles {
            self.spawn(emitter_pos, to_spawn);
        }

        // Simple fire-ish motion: upward acceleration + drag.
        let upward_accel = Vec2::new(0.0, -220.0);
        let drag = 1.8;

        for p in &mut self.particles {
            p.age += dt;
            p.vel += upward_accel * dt;
            p.vel *= (1.0 - drag * dt).clamp(0.0, 1.0);
            p.pos += p.vel * dt;
        }

        self.particles.retain(|p| !p.is_dead());
    }

    fn spawn(&mut self, pos: Vec2, count: usize) {
        let mut rng = rand::thread_rng();

        for _ in 0..count {
            if self.particles.len() >= self.max_particles {
                break;
            }

            // Random jitter around the cursor so it looks like a plume.
            let jitter = Vec2::new(rng.gen_range(-3.0..=3.0), rng.gen_range(-3.0..=3.0));

            // Upward bias + randomness.
            let vel = Vec2::new(
                rng.gen_range(-55.0..=55.0),
                rng.gen_range(-260.0..=-110.0),
            );

            let lifetime = rng.gen_range(0.35..=0.85);
            let size = rng.gen_range(6.0..=14.0);

            self.particles.push(Particle {
                pos: pos + jitter,
                vel,
                age: 0.0,
                lifetime,
                size,
            });
        }
    }
}
