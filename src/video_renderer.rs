use bytemuck::{Pod, Zeroable};
use glam::Mat4;
use wgpu::util::DeviceExt;

use crate::screen::Screen;
use crate::ui::settings::{Projection, StereoLayout, VideoMode, VideoSettings};
use crate::video::texture::VideoTexture;

// ── GPU types ──────────────────────────────────────────────────────────────

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct CameraUniform {
    projection: [[f32; 4]; 4],
    view: [[f32; 4]; 4],
}

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct VideoParams {
    uv_offset: [f32; 2],
    uv_scale:  [f32; 2],
    mode:      u32,
    inv_zoom:  f32,
    /// 0 = BGRA texture, 1 = NV12 (Y + UV planes).
    pixel_fmt: u32,
    _pad:      u32,
}

impl VideoParams {
    fn from_settings(settings: &VideoSettings, eye: usize, pixel_fmt: u32) -> Self {
        let zoom = settings.zoom.max(0.1);
        let inv_zoom = 1.0 / zoom;
        let mode = match (settings.proj, settings.mode) {
            (Projection::Fisheye,  VideoMode::View180) => 1,
            (Projection::Fisheye,  VideoMode::View360) => 2,
            (Projection::Equirect, VideoMode::View180) => 3,
            (Projection::Equirect, VideoMode::View360) => 4,
            _ => 0,
        };
        // Stereo UV crop: select which half of the video this eye should see.
        let (uv_offset, uv_scale) = match settings.stereo {
            StereoLayout::OneView => ([0.0_f32, 0.0], [1.0_f32, 1.0]),
            StereoLayout::LR => {
                if eye == 0 { ([0.0, 0.0], [0.5, 1.0]) }
                else        { ([0.5, 0.0], [0.5, 1.0]) }
            },
            StereoLayout::RL => {
                if eye == 0 { ([0.5, 0.0], [0.5, 1.0]) }
                else        { ([0.0, 0.0], [0.5, 1.0]) }
            },
            StereoLayout::TB => {
                if eye == 0 { ([0.0, 0.0], [1.0, 0.5]) }
                else        { ([0.0, 0.5], [1.0, 0.5]) }
            },
            StereoLayout::BT => {
                if eye == 0 { ([0.0, 0.5], [1.0, 0.5]) }
                else        { ([0.0, 0.0], [1.0, 0.5]) }
            },
        };
        Self {
            uv_offset,
            uv_scale,
            mode,
            inv_zoom,
            pixel_fmt,
            _pad: 0,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct Vertex {
    position: [f32; 3],
    uv: [f32; 2],
}

/// Strips the curved screen is cut into (a flat screen is one).
const SEGMENTS: usize = 64;

/// The screen: 3.2 m wide (times zoom) and 2.5 m in front of the viewer by default, where the SteamVR quad layer used to
/// hang. Portrait video is capped at the height of a 16:9 screen. Drawn in the scene (not as an overlay) so the control
/// panels always stay in front of it.
///
/// `curved` (Curved mode) bends it onto a cylinder of radius distance / curve, its middle staying put; 100% puts the axis at
/// the viewer. The tilt then turns the whole screen about the viewer's eyes (at `height`), so a screen tilted up still
/// faces the viewer. Triangles come in the order the old flat quad used: top-left, bottom-left, top-right, then the rest.
fn build_screen(aspect: f32, zoom: f32, curved: bool, screen: &Screen) -> Vec<Vertex> {
    let mut w = 3.2_f32;
    let mut h = w / aspect.max(0.01);
    if h > 1.8 {
        let s = 1.8 / h;
        w *= s;
        h = 1.8;
    }
    let (w, h) = (w * zoom, h * zoom);
    let d = screen.distance;
    let c = if curved { screen.curve as f32 / 100.0 } else { 0.0 };
    let bent = c > 0.001;
    let (sin_t, cos_t) = screen.tilt.to_radians().sin_cos();
    // A point `u` metres across and `y` up from the middle of the screen, in the scene.
    let place = |u: f32, y: f32| -> [f32; 3] {
        let (x, z) = if bent {
            let r = d / c;
            let a = u / r;
            (r * a.sin(), -d + r * (1.0 - a.cos()))
        } else {
            (u, -d)
        };
        [x, screen.height + y * cos_t - z * sin_t, y * sin_t + z * cos_t]
    };
    let n = if bent { SEGMENTS } else { 1 };
    let mut v = Vec::with_capacity(n * 6);
    for i in 0..n {
        let (t0, t1) = (i as f32 / n as f32, (i + 1) as f32 / n as f32);
        let (u0, u1) = ((t0 - 0.5) * w, (t1 - 0.5) * w);
        let tl = Vertex { position: place(u0,  h * 0.5), uv: [t0, 0.0] };
        let bl = Vertex { position: place(u0, -h * 0.5), uv: [t0, 1.0] };
        let tr = Vertex { position: place(u1,  h * 0.5), uv: [t1, 0.0] };
        let br = Vertex { position: place(u1, -h * 0.5), uv: [t1, 1.0] };
        v.extend([tl, bl, tr, tr, bl, br]);
    }
    v
}

// ── VideoRenderer ──────────────────────────────────────────────────────────

pub struct VideoRenderer {
    pipeline:           wgpu::RenderPipeline, // flat quad / fisheye (vertex buffer)
    equirect_pipeline:  wgpu::RenderPipeline, // fullscreen triangle, no vertex buffer
    camera_buffer:      wgpu::Buffer,
    camera_bind_group:  wgpu::BindGroup,
    texture_bgl:        wgpu::BindGroupLayout,
    sampler:            wgpu::Sampler,
    vertex_buffer:      wgpu::Buffer,
    /// Width / height of the whole video frame (both eyes for side-by-side).
    aspect:             f32,
    texture_bind_group: Option<wgpu::BindGroup>,
    params_buffer:      wgpu::Buffer,
    params_bind_group:  wgpu::BindGroup,
    /// 0 = BGRA, 1 = NV12.  Set by `set_texture`.
    pixel_fmt:          u32,
}

impl VideoRenderer {
    pub fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        video_width: u32,
        video_height: u32,
    ) -> Self {
        let shader =
            device.create_shader_module(wgpu::include_wgsl!("shaders/video-quad.wgsl"));

        // ── camera bind group layout (group 0) ────────────────────────────
        let camera_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vr_camera_bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let init_cam = CameraUniform {
            projection: Mat4::IDENTITY.to_cols_array_2d(),
            view: Mat4::IDENTITY.to_cols_array_2d(),
        };
        let camera_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("vr_camera_buf"),
            contents: bytemuck::bytes_of(&init_cam),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let camera_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vr_camera_bg"),
            layout: &camera_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: camera_buffer.as_entire_binding(),
            }],
        });

        // ── texture bind group layout (group 1) ───────────────────────────
        // binding 0: Y or BGRA texture
        // binding 1: sampler
        // binding 2: UV chroma texture (NV12 only; dummy 1×1 for BGRA)
        let texture_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vr_texture_bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        multisampled: false,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        multisampled: false,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    },
                    count: None,
                },
            ],
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("vr_video_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        // ── video params bind group layout (group 2) ──────────────────────
        let params_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vr_params_bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let default_params = VideoParams {
            uv_offset: [0.0, 0.0], uv_scale: [1.0, 1.0], mode: 0, inv_zoom: 1.0, pixel_fmt: 0, _pad: 0,
        };
        let params_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("vr_params_buf"),
            contents: bytemuck::bytes_of(&default_params),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let params_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vr_params_bg"),
            layout: &params_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: params_buffer.as_entire_binding(),
            }],
        });

        // ── pipelines ─────────────────────────────────────────────────────
        let pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: &[Some(&camera_bgl), Some(&texture_bgl), Some(&params_bgl)],
                immediate_size: 0,
            });

        // Flat/fisheye pipeline: uses a vertex buffer with position + uv.
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("video_quad_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x2],
                }],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        // Equirect pipeline: fullscreen triangle, no vertex buffer.
        // The vertex shader (vs_equirect) generates positions from vertex_index
        // and passes clip-space xy as uv for ray-direction reconstruction.
        let equirect_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("video_equirect_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_equirect"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        // ── quad vertex buffer ────────────────────────────────────────────
        let aspect = video_width as f32 / video_height.max(1) as f32;
        // Room for the most triangles a screen has (a curved one); `render_eye` fills it before every draw.
        let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("video_quad_vbuf"),
            size: (SEGMENTS * 6 * std::mem::size_of::<Vertex>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            equirect_pipeline,
            camera_buffer,
            camera_bind_group,
            texture_bgl,
            sampler,
            vertex_buffer,
            aspect,
            texture_bind_group: None,
            params_buffer,
            params_bind_group,
            pixel_fmt: 0,
        }
    }

    /// Call once (and whenever the video texture is recreated) to update the
    /// sampler binding.
    pub fn set_texture(&mut self, device: &wgpu::Device, texture: &VideoTexture) {
        self.pixel_fmt = if texture.is_nv12 { 1 } else { 0 };
        self.texture_bind_group = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vr_texture_bg"),
            layout: &self.texture_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&texture.view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&texture.uv_view),
                },
            ],
        }));
    }

    /// Render one eye to `target`.  Does nothing if no texture has been set yet.
    pub fn render_eye(
        &self,
        target: &wgpu::TextureView,
        projection: Mat4,
        view: Mat4,
        settings: &VideoSettings,
        eye: usize,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) {
        let Some(tex_bg) = &self.texture_bind_group else { return };

        // All direction-based modes (fisheye + equirect) use the fullscreen-triangle pipeline.
        let use_direction_shader = matches!(
            settings.mode,
            VideoMode::View180 | VideoMode::View360
        );

        let cam = CameraUniform {
            projection: projection.to_cols_array_2d(),
            view: view.to_cols_array_2d(),
        };
        queue.write_buffer(&self.camera_buffer, 0, bytemuck::bytes_of(&cam));

        let params = VideoParams::from_settings(settings, eye, self.pixel_fmt);
        queue.write_buffer(&self.params_buffer, 0, bytemuck::bytes_of(&params));

        // The screen: size follows the zoom and the per-eye aspect (half of a side-by-side frame); only Curved mode bends.
        let mut screen_vertices = 0;
        if !use_direction_shader {
            let eye_aspect = match settings.stereo {
                StereoLayout::LR | StereoLayout::RL => self.aspect * 0.5,
                StereoLayout::TB | StereoLayout::BT => self.aspect * 2.0,
                StereoLayout::OneView => self.aspect,
            };
            let verts = build_screen(eye_aspect, settings.zoom.max(0.1), settings.mode == VideoMode::Curved2D, &settings.screen);
            queue.write_buffer(&self.vertex_buffer, 0, bytemuck::cast_slice(&verts));
            screen_vertices = verts.len() as u32;
        }

        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("video_quad_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });

            pass.set_bind_group(0, &self.camera_bind_group, &[]);
            pass.set_bind_group(1, tex_bg, &[]);
            pass.set_bind_group(2, &self.params_bind_group, &[]);

            if use_direction_shader {
                // Fullscreen triangle — no vertex buffer needed.
                // The fragment shader reconstructs ray directions from clip-space uv
                // using the camera projection and view-rotation matrices.
                pass.set_pipeline(&self.equirect_pipeline);
                pass.draw(0..3, 0..1);
            } else {
                pass.set_pipeline(&self.pipeline);
                pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
                pass.draw(0..screen_vertices, 0..1);
            }
        }

        queue.submit(std::iter::once(encoder.finish()));
        device
            .poll(wgpu::PollType::Wait { submission_index: None, timeout: None })
            .ok();
    }
}

#[cfg(test)]
mod screen_geometry_tests {
    use super::*;

    const WIDE: f32 = 16.0 / 9.0;

    fn corners(v: &[Vertex]) -> Vec<[f32; 3]> { v.iter().map(|x| x.position).collect() }

    fn close(a: f32, b: f32) -> bool { (a - b).abs() < 1e-3 }

    #[test]
    fn the_default_flat_screen_hangs_where_it_always_did() {
        let v = build_screen(WIDE, 1.0, false, &Screen::default());
        assert_eq!(v.len(), 6);
        let p = corners(&v);
        let same = |a: [f32; 3], b: [f32; 3]| a.iter().zip(b).all(|(x, y)| close(*x, y));
        assert!(same(p[0], [-1.6, 1.9, -2.5]), "top left {:?}", p[0]);
        assert!(same(p[1], [-1.6, 0.1, -2.5]), "bottom left {:?}", p[1]);
        assert!(same(p[2], [ 1.6, 1.9, -2.5]), "top right {:?}", p[2]);
        assert_eq!(v[0].uv, [0.0, 0.0]);
        assert_eq!(v[5].uv, [1.0, 1.0]);
    }

    #[test]
    fn only_curved_mode_bends_and_zoom_scales() {
        let flat = build_screen(WIDE, 1.0, false, &Screen { curve: 100, ..Screen::default() });
        assert_eq!(flat.len(), 6, "the curve value means nothing for a flat screen");
        let curved = build_screen(WIDE, 1.0, true, &Screen { curve: 0, ..Screen::default() });
        assert_eq!(curved.len(), 6, "0% curve is flat");
        let big = build_screen(WIDE, 2.0, false, &Screen::default());
        assert!(close(big[2].position[0], 3.2));
    }

    #[test]
    fn a_full_curve_wraps_the_screen_around_the_viewer() {
        let s = Screen { curve: 100, ..Screen::default() };
        let v = build_screen(WIDE, 1.0, true, &s);
        assert_eq!(v.len(), SEGMENTS * 6);
        for p in corners(&v) {
            assert!(close((p[0] * p[0] + p[2] * p[2]).sqrt(), 2.5), "{p:?} is not 2.5 m from the viewer's axis");
        }
        // the middle stays where the flat screen's middle is, the edges come toward the viewer
        let edge = v[0].position;
        assert!(edge[2] > -2.5 + 0.1 && edge[0] < -1.0, "{edge:?}");
        // the width along the curve is still 3.2 m (the strips are short chords, so a hair less)
        let across: f32 = (0..SEGMENTS).map(|i| {
            let (a, b) = (v[i * 6].position, v[i * 6 + 2].position);
            ((a[0] - b[0]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
        }).sum();
        assert!((across - 3.2).abs() < 0.01, "{across}");
    }

    #[test]
    fn a_half_curve_has_twice_the_radius() {
        let s = Screen { curve: 50, ..Screen::default() };
        for p in corners(&build_screen(WIDE, 1.0, true, &s)) {
            // radius 5 m, its axis 2.5 m behind the viewer
            assert!(close((p[0] * p[0] + (p[2] - 2.5) * (p[2] - 2.5)).sqrt(), 5.0), "{p:?}");
        }
    }

    #[test]
    fn height_lifts_and_tilt_turns_the_screen_about_the_viewer() {
        let up = Screen { height: 1.5, ..Screen::default() };
        assert!(close(build_screen(WIDE, 1.0, false, &up)[0].position[1], 2.4));
        // tilted straight up (lying on the back): the screen's middle is d above the viewer's eyes
        let overhead = Screen { tilt: 90.0, ..Screen::default() };
        let p = corners(&build_screen(WIDE, 1.0, false, &overhead));
        let mid = [p[0], p[1], p[2], p[5]].iter().fold([0.0; 3], |m, q| [m[0] + q[0] / 4.0, m[1] + q[1] / 4.0, m[2] + q[2] / 4.0]);
        assert!(close(mid[0], 0.0) && close(mid[1], 1.0 + 2.5) && close(mid[2], 0.0), "{mid:?}");
        // and a small tilt keeps the distance from the eyes
        let tilted = Screen { tilt: -20.0, ..Screen::default() };
        let v = build_screen(WIDE, 1.0, false, &tilted);
        let (a, b) = (v[0].position, v[1].position);
        let mid_y = (a[1] + b[1]) / 2.0 - 1.0;
        let mid_z = (a[2] + b[2]) / 2.0;
        assert!(close((mid_y * mid_y + mid_z * mid_z).sqrt(), 2.5), "{a:?} {b:?}");
        assert!(mid_y < 0.0, "a negative tilt turns the screen down");
    }
}

