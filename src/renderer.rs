//! GPU renderer (wgpu) for the cursor fire particles.
//!
//! Rendering approach:
//! - Each particle is drawn as an instanced quad (two triangles).
//! - Instance data contains position (px), size (px), and premultiplied RGBA.
//! - Vertex shader converts pixel coords to NDC using the current surface size.
//! - We clear the frame to transparent black and alpha-blend particles.

use anyhow::Context;
use bytemuck::{Pod, Zeroable};
use glam::Vec4;

use wgpu::util::DeviceExt;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    // Screen size in pixels
    screen_size: [f32; 2],
    _pad: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Instance {
    pub pos: [f32; 2],
    pub size: f32,
    pub _pad0: f32,
    pub color: [f32; 4], // premultiplied RGBA
}

pub struct Renderer {
    mode: PresentMode,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: Option<wgpu::SurfaceConfiguration>,

    pipeline: wgpu::RenderPipeline,
    uniform_buf: wgpu::Buffer,
    uniform_bind_group: wgpu::BindGroup,

    quad_vbuf: wgpu::Buffer,
    quad_vertex_count: u32,

    instance_buf: wgpu::Buffer,
    instance_capacity: usize,

    // Layered-present path
    offscreen: Option<Offscreen>,

    debug_logged_alpha: bool,
}

enum PresentMode {
    Surface {
        surface: wgpu::Surface<'static>,
    },
    Layered,
}

struct Offscreen {
    width: u32,
    height: u32,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    readback: wgpu::Buffer,
    padded_bytes_per_row: u32,
}

impl Renderer {
    pub async fn new(window: &winit::window::Window) -> anyhow::Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });

        // Create a surface first, but be prepared to fall back to layered presentation
        // if the surface alpha mode is opaque-only.
        let surface_tmp = instance
            .create_surface(window)
            .context("failed to create wgpu surface")?;

        // SAFETY: We extend the surface lifetime to 'static; the window lives for the full run.
        let surface_tmp: wgpu::Surface<'static> = unsafe { std::mem::transmute(surface_tmp) };

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface_tmp),
                force_fallback_adapter: false,
            })
            .await
            .context("no suitable GPU adapters found")?;

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::default(),
                },
                None,
            )
            .await
            .context("request_device failed")?;

        let caps = surface_tmp.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);

        let size = window.inner_size();

        // Prefer premultiplied alpha to match DWM composition for transparent windows.
        // If the surface only supports Opaque, the window will appear with a black background.
        eprintln!("wgpu surface alpha modes: {:?}", caps.alpha_modes);
        let alpha_mode = caps
            .alpha_modes
            .iter()
            .copied()
            .find(|m| matches!(m, wgpu::CompositeAlphaMode::PreMultiplied))
            .or_else(|| {
                caps.alpha_modes
                    .iter()
                    .copied()
                    .find(|m| matches!(m, wgpu::CompositeAlphaMode::PostMultiplied))
            })
            .or_else(|| {
                caps.alpha_modes
                    .iter()
                    .copied()
                    .find(|m| matches!(m, wgpu::CompositeAlphaMode::Inherit))
            });

        let (mode, config, offscreen) = if let Some(alpha_mode) = alpha_mode {
            eprintln!("wgpu chosen alpha mode: {alpha_mode:?}");
            let config = wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format,
                width: size.width.max(1),
                height: size.height.max(1),
                present_mode: wgpu::PresentMode::Fifo,
                alpha_mode,
                view_formats: vec![],
                desired_maximum_frame_latency: 2,
            };
            surface_tmp.configure(&device, &config);
            (PresentMode::Surface { surface: surface_tmp }, Some(config), None)
        } else {
            // Opaque-only: render offscreen and present via UpdateLayeredWindow.
            eprintln!("wgpu surface reports Opaque-only alpha; using layered window present");
            let size = window.inner_size();
            let off = create_offscreen(&device, size.width.max(1), size.height.max(1));
            (PresentMode::Layered, None, Some(off))
        };

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("particle.wgsl"),
            source: wgpu::ShaderSource::Wgsl(PARTICLE_SHADER.into()),
        });

        let uniform_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("uniform_layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let uniform_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("uniform_buf"),
            contents: bytemuck::bytes_of(&Uniforms {
                screen_size: [
                    offscreen
                        .as_ref()
                        .map(|o| o.width)
                        .or_else(|| config.as_ref().map(|c| c.width))
                        .unwrap_or(1) as f32,
                    offscreen
                        .as_ref()
                        .map(|o| o.height)
                        .or_else(|| config.as_ref().map(|c| c.height))
                        .unwrap_or(1) as f32,
                ],
                _pad: [0.0; 2],
            }),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let uniform_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("uniform_bind_group"),
            layout: &uniform_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });

        // Quad covering [-0.5, 0.5] in local space; scaled per instance.
        // Two triangles.
        let quad_vertices: &[[f32; 2]] = &[
            [-0.5, -0.5],
            [0.5, -0.5],
            [0.5, 0.5],
            [-0.5, -0.5],
            [0.5, 0.5],
            [-0.5, 0.5],
        ];
        let quad_vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("quad_vbuf"),
            contents: bytemuck::cast_slice(quad_vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let instance_capacity = 8192;
        let instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("instance_buf"),
            size: (instance_capacity * std::mem::size_of::<Instance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pipeline_layout"),
            bind_group_layouts: &[&uniform_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                compilation_options: Default::default(),
                buffers: &[
                    // Quad vertices
                    wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<[f32; 2]>() as u64,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &[wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x2,
                            offset: 0,
                            shader_location: 0,
                        }],
                    },
                    // Instance data
                    wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<Instance>() as u64,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &[
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x2,
                                offset: 0,
                                shader_location: 1,
                            },
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32,
                                offset: 8,
                                shader_location: 2,
                            },
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x4,
                                offset: 16,
                                shader_location: 3,
                            },
                        ],
                    },
                ],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.as_ref().map(|c| c.format).unwrap_or(wgpu::TextureFormat::Bgra8Unorm),
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
        });

        Ok(Self {
            mode,
            device,
            queue,
            config,
            pipeline,
            uniform_buf,
            uniform_bind_group,
            quad_vbuf,
            quad_vertex_count: quad_vertices.len() as u32,
            instance_buf,
            instance_capacity,
            offscreen,
            debug_logged_alpha: false,
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        let width = width.max(1);
        let height = height.max(1);
        match &mut self.mode {
            PresentMode::Surface { surface } => {
                let Some(cfg) = &mut self.config else { return; };
                if width == cfg.width && height == cfg.height {
                    return;
                }
                cfg.width = width;
                cfg.height = height;
                surface.configure(&self.device, cfg);
            }
            PresentMode::Layered => {
                let recreate = match &self.offscreen {
                    Some(o) => o.width != width || o.height != height,
                    None => true,
                };
                if recreate {
                    self.offscreen = Some(create_offscreen(&self.device, width, height));
                }
            }
        }

        let uniforms = Uniforms {
            screen_size: [width as f32, height as f32],
            _pad: [0.0; 2],
        };
        self.queue
            .write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&uniforms));
    }

    pub fn render(
        &mut self,
        instances: &[Instance],
        overlay: &mut crate::window::OverlayWindow,
    ) -> anyhow::Result<()> {
        if instances.is_empty() {
            // Still present a cleared frame to avoid stale pixels.
        }

        if instances.len() > self.instance_capacity {
            self.instance_capacity = instances.len().next_power_of_two();
            self.instance_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("instance_buf(resized)"),
                size: (self.instance_capacity * std::mem::size_of::<Instance>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }

        self.queue
            .write_buffer(&self.instance_buf, 0, bytemuck::cast_slice(instances));

        match &mut self.mode {
            PresentMode::Surface { surface } => {
                let Some(cfg) = &self.config else { return Ok(()); };
                let frame = match surface.get_current_texture() {
                    Ok(f) => f,
                    Err(wgpu::SurfaceError::Outdated | wgpu::SurfaceError::Lost) => {
                        surface.configure(&self.device, cfg);
                        return Ok(());
                    }
                    Err(wgpu::SurfaceError::Timeout) => return Ok(()),
                    Err(wgpu::SurfaceError::OutOfMemory) => {
                        anyhow::bail!("wgpu surface out of memory")
                    }
                };

                let view = frame
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default());

                let mut encoder = self
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("encoder"),
                    });

                render_pass(
                    &mut encoder,
                    &self.pipeline,
                    &self.uniform_bind_group,
                    &self.quad_vbuf,
                    &self.instance_buf,
                    self.quad_vertex_count,
                    instances.len() as u32,
                    &view,
                );

                self.queue.submit(Some(encoder.finish()));
                frame.present();
                Ok(())
            }
            PresentMode::Layered => {
                let off = self
                    .offscreen
                    .as_mut()
                    .context("offscreen not initialized")?;

                let mut encoder = self
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("encoder"),
                    });

                render_pass(
                    &mut encoder,
                    &self.pipeline,
                    &self.uniform_bind_group,
                    &self.quad_vbuf,
                    &self.instance_buf,
                    self.quad_vertex_count,
                    instances.len() as u32,
                    &off.view,
                );

                // Copy texture -> readback buffer.
                encoder.copy_texture_to_buffer(
                    wgpu::ImageCopyTexture {
                        texture: &off.texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::ImageCopyBuffer {
                        buffer: &off.readback,
                        layout: wgpu::ImageDataLayout {
                            offset: 0,
                            bytes_per_row: Some(off.padded_bytes_per_row),
                            rows_per_image: Some(off.height),
                        },
                    },
                    wgpu::Extent3d {
                        width: off.width,
                        height: off.height,
                        depth_or_array_layers: 1,
                    },
                );

                self.queue.submit(Some(encoder.finish()));

                // Map + wait.
                let slice = off.readback.slice(..);
                let (tx, rx) = std::sync::mpsc::channel();
                slice.map_async(wgpu::MapMode::Read, move |r| {
                    let _ = tx.send(r);
                });
                self.device.poll(wgpu::Maintain::Wait);
                rx.recv().ok().context("map_async dropped")??;

                let data = slice.get_mapped_range();
                let row_bytes = (off.width as usize) * 4;
                let padded = off.padded_bytes_per_row as usize;
                let mut compact = vec![0u8; row_bytes * off.height as usize];
                for y in 0..off.height as usize {
                    let src = &data[y * padded..y * padded + row_bytes];
                    let dst = &mut compact[y * row_bytes..y * row_bytes + row_bytes];
                    dst.copy_from_slice(src);
                }
                drop(data);
                off.readback.unmap();

                if !self.debug_logged_alpha {
                    let mut min_a = 255u8;
                    let mut max_a = 0u8;
                    let mut nonzero = 0usize;
                    for px in compact.chunks_exact(4) {
                        let a = px[3];
                        min_a = min_a.min(a);
                        max_a = max_a.max(a);
                        if a != 0 {
                            nonzero += 1;
                        }
                    }
                    eprintln!(
                        "layered frame alpha stats: min={min_a} max={max_a} nonzero_pixels={nonzero} of {}",
                        compact.len() / 4
                    );
                    self.debug_logged_alpha = true;
                }

                overlay.present_layered_bgra8_premul(off.width, off.height, &compact)
            }
        }
    }
}

fn render_pass(
    encoder: &mut wgpu::CommandEncoder,
    pipeline: &wgpu::RenderPipeline,
    uniform_bind_group: &wgpu::BindGroup,
    quad_vbuf: &wgpu::Buffer,
    instance_buf: &wgpu::Buffer,
    quad_vertex_count: u32,
    instance_count: u32,
    view: &wgpu::TextureView,
) {
    let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("render_pass"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 0.0,
                }),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
    });

    rpass.set_pipeline(pipeline);
    rpass.set_bind_group(0, uniform_bind_group, &[]);
    rpass.set_vertex_buffer(0, quad_vbuf.slice(..));
    rpass.set_vertex_buffer(1, instance_buf.slice(..));
    rpass.draw(0..quad_vertex_count, 0..instance_count);
}

fn create_offscreen(device: &wgpu::Device, width: u32, height: u32) -> Offscreen {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("offscreen_tex"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Bgra8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as u32;
    let unpadded = width * 4;
    let padded_bytes_per_row = ((unpadded + align - 1) / align) * align;
    let readback_size = padded_bytes_per_row as u64 * height as u64;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: readback_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    Offscreen {
        width,
        height,
        texture,
        view,
        readback,
        padded_bytes_per_row,
    }
}

impl Instance {
    pub fn new(pos: [f32; 2], size: f32, color_premul: Vec4) -> Self {
        Self {
            pos,
            size,
            _pad0: 0.0,
            color: [color_premul.x, color_premul.y, color_premul.z, color_premul.w],
        }
    }
}

const PARTICLE_SHADER: &str = r#"
struct Uniforms {
    screen_size: vec2<f32>,
    _pad: vec2<f32>,
};

@group(0) @binding(0)
var<uniform> u: Uniforms;

struct VsIn {
    @location(0) local_pos: vec2<f32>,
    @location(1) inst_pos: vec2<f32>,
    @location(2) inst_size: f32,
    @location(3) inst_color: vec4<f32>,
};

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) local: vec2<f32>,
};

fn px_to_ndc(p: vec2<f32>) -> vec2<f32> {
    // p is in pixels from top-left.
    let x = (p.x / u.screen_size.x) * 2.0 - 1.0;
    let y = 1.0 - (p.y / u.screen_size.y) * 2.0;
    return vec2<f32>(x, y);
}

@vertex
fn vs_main(input: VsIn) -> VsOut {
    var out: VsOut;

    let world_px = input.inst_pos + input.local_pos * input.inst_size;
    let ndc = px_to_ndc(world_px);

    out.pos = vec4<f32>(ndc, 0.0, 1.0);
    out.color = input.inst_color;
    out.local = input.local_pos;
    return out;
}

@fragment
fn fs_main(input: VsOut) -> @location(0) vec4<f32> {
    // Soft circular falloff to make particles look round-ish.
    let r = length(input.local);
    let soft = smoothstep(0.55, 0.10, r);
    return vec4<f32>(input.color.rgb * soft, input.color.a * soft);
}
"#;
