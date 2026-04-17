#![cfg_attr(not(windows), allow(dead_code, unused_imports))]

// Windows-only application.
#[cfg(not(windows))]
compile_error!("fire-cursor is Windows-only (Win32 overlay window required)");

#[cfg(windows)]
mod particles;
#[cfg(windows)]
mod renderer;
#[cfg(windows)]
mod window;

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use std::time::Instant;
    use std::time::{Duration};

    use glam::Vec2;
    use winit::event::{Event, WindowEvent};
    use winit::event_loop::{ControlFlow, EventLoop};

    let event_loop = EventLoop::new()?;

    // Hide the *system* cursor for the duration of the app.
    // This is the only reliable way with a click-through overlay.
    let system_cursor_hidden = window::SystemCursorHider::new_hidden()?;

    // Secondary fallback (thread display counter).
    let cursor_hidden = window::CursorHider::new_hidden();

    let mut overlay = window::OverlayWindow::create(&event_loop)?;
    let mut renderer = renderer::Renderer::new(overlay.size().0, overlay.size().1);
    let mut particles = particles::ParticleSystem::new();

    let mut last_frame = Instant::now();

    overlay.window.request_redraw();

    let target_frame = Duration::from_micros(6_060); // ~165 FPS
    let mut next_frame_time = Instant::now();

    event_loop.run(move |event, elwt| {
        // Wait-until pacing reduces CPU usage and prevents jitter.
        elwt.set_control_flow(ControlFlow::WaitUntil(next_frame_time));

        match event {
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => {
                    // Restore system cursor immediately (Drop runs too, but this avoids any
                    // delay and helps if the process terminates quickly after exit).
                    system_cursor_hidden.restore_now();
                    elwt.exit();
                }
                WindowEvent::Resized(size) => {
                    overlay.update_size_from_window();
                    renderer.resize(size.width, size.height)
                }
                WindowEvent::ScaleFactorChanged { .. } => {
                    overlay.update_size_from_window();
                    let size = overlay.window.inner_size();
                    renderer.resize(size.width, size.height)
                }
                WindowEvent::RedrawRequested => {
                    let now = Instant::now();
                    let dt = (now - last_frame).as_secs_f32();
                    last_frame = now;

                    // Schedule the next frame.
                    next_frame_time = now + target_frame;

                    let cursor = match overlay.cursor_pos_in_window() {
                        Ok(p) => p,
                        Err(_) => Vec2::new(-1000.0, -1000.0),
                    };

                    particles.update_and_spawn(dt, cursor);

                    let instances: Vec<renderer::Instance> = particles
                        .particles()
                        .iter()
                        .map(|p| {
                            renderer::Instance::new(
                                [p.pos.x, p.pos.y],
                                p.size,
                                p.color_premul(),
                            )
                        })
                        .collect();

                    if let Err(e) = renderer.render(&instances, &mut overlay) {
                        eprintln!("render error: {e:#}");
                    }
                }
                _ => {}
            },

            Event::AboutToWait => {
                // Drive a steady redraw loop.
                // Re-assert cursor hidden (some apps/libraries toggle it).
                cursor_hidden.ensure_hidden();
                overlay.window.set_cursor_visible(false);
                if Instant::now() >= next_frame_time {
                    overlay.window.request_redraw();
                }
            }

            _ => {}
        }
    })
    .map_err(|e| anyhow::anyhow!(e))
}
