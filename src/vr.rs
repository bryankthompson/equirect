use std::ffi::CStr;
use openxr as xr;
use crate::vprintln;

use crate::desk_input::DeskInput;
use crate::input::{ControllerState, XrInput};
use crate::pointer_renderer::PointerRenderer;
use crate::renderer::Renderer;
use crate::ui::browser::{BrowserActions, BrowserState, draw as browser_draw};
use crate::ui::control_bar::{ControlBarActions, ControlBarState};
use crate::ui::panel::PanelRenderer;
use crate::ui::ui::N_CURSORS;
use crate::ui::settings::{SettingsActions, SettingsView, VideoSettings, draw as settings_draw};
use crate::video::texture::VideoTexture;
use crate::video_layer::{
    VideoSwapchain, EffectiveLayerMode, effective_mode,
    zoom_angles,
};
use crate::video_renderer::VideoRenderer;

// ── format helpers ────────────────────────────────────────────────────────────

fn vk_to_wgpu(vk: u32) -> Option<wgpu::TextureFormat> {
    match vk {
        37 => Some(wgpu::TextureFormat::Rgba8Unorm),
        43 => Some(wgpu::TextureFormat::Rgba8UnormSrgb),
        44 => Some(wgpu::TextureFormat::Bgra8Unorm),
        50 => Some(wgpu::TextureFormat::Bgra8UnormSrgb),
        _ => None,
    }
}

fn wgpu_to_vk(fmt: wgpu::TextureFormat) -> Option<u32> {
    match fmt {
        wgpu::TextureFormat::Rgba8Unorm => Some(37),
        wgpu::TextureFormat::Rgba8UnormSrgb => Some(43),
        wgpu::TextureFormat::Bgra8Unorm => Some(44),
        wgpu::TextureFormat::Bgra8UnormSrgb => Some(50),
        _ => None,
    }
}

// ── math helpers ──────────────────────────────────────────────────────────────

fn fov_to_projection(fov: xr::Fovf, near: f32, far: f32) -> glam::Mat4 {
    let tl = fov.angle_left.tan();
    let tr = fov.angle_right.tan();
    let td = fov.angle_down.tan();
    let tu = fov.angle_up.tan();
    let w = tr - tl;
    let h = tu - td;
    glam::Mat4::from_cols(
        glam::Vec4::new(2.0 / w, 0.0, 0.0, 0.0),
        glam::Vec4::new(0.0, 2.0 / h, 0.0, 0.0),
        glam::Vec4::new((tr + tl) / w, (tu + td) / h, -far / (far - near), -1.0),
        glam::Vec4::new(0.0, 0.0, -(far * near) / (far - near), 0.0),
    )
}

/// Extract only the yaw (rotation around stage Y axis) from an XR head pose as a quaternion.
/// Returns a rotation R such that R * NEG_Z = the head's horizontal forward direction.
fn yaw_quat_from_head(pose: &xr::Posef) -> glam::Quat {
    let q = pose.orientation;
    let forward = glam::Quat::from_xyzw(q.x, q.y, q.z, q.w) * glam::Vec3::NEG_Z;
    let yaw = f32::atan2(forward.x, -forward.z);
    // Negate: from_rotation_y(a) maps NEG_Z to (-sin(a), 0, -cos(a)),
    // so we need -yaw to get (sin(yaw), 0, -cos(yaw)) = the forward direction.
    glam::Quat::from_rotation_y(-yaw)
}

/// Did the SteamVR centre (the origin of the LOCAL space, seen from the stage) move since last frame?
/// Only a real re-centre moves it that far: a hand's width, or a few degrees of turn.
fn centre_moved(before: (glam::Vec3, glam::Quat), now: (glam::Vec3, glam::Quat)) -> bool {
    before.0.distance(now.0) > 0.02 || before.1.angle_between(now.1) > 2.0_f32.to_radians()
}

/// The scene's frame for a head: (orientation, origin). The origin is the spot on the floor under the head;
/// the orientation faces where the head looks, upright or (with `keep_pitch`) locked to the full head direction.
fn scene_frame(head: &xr::Posef, keep_pitch: bool) -> (glam::Quat, glam::Vec3) {
    let orient = if keep_pitch { full_quat_from_head(head) } else { yaw_quat_from_head(head) };
    (orient, glam::Vec3::new(head.position.x, 0.0, head.position.z))
}

/// Extract the full head orientation from an XR head pose as a glam quaternion.
fn full_quat_from_head(pose: &xr::Posef) -> glam::Quat {
    let q = pose.orientation;
    glam::Quat::from_xyzw(q.x, q.y, q.z, q.w)
}

/// Build an XR pose for a video layer given a base orientation.
/// Position is placed `dist` metres ahead along the horizontal forward of `orient`
/// at `height` metres above the stage floor, all of it moved by `origin`; the layer orientation is `orient` itself.
fn oriented_pose(dist: f32, height: f32, orient: glam::Quat, origin: glam::Vec3) -> xr::Posef {
    // Use only the horizontal component of the forward vector for positioning so
    // that looking up/down doesn't float the quad above or below eye level.
    let fwd = orient * glam::Vec3::NEG_Z;
    let horiz = glam::Vec2::new(fwd.x, fwd.z).normalize_or(glam::Vec2::new(0.0, -1.0));
    xr::Posef {
        position: xr::Vector3f { x: origin.x + horiz.x * dist, y: origin.y + height, z: origin.z + horiz.y * dist },
        orientation: xr::Quaternionf { x: orient.x, y: orient.y, z: orient.z, w: orient.w },
    }
}

fn pose_to_view(pose: xr::Posef) -> glam::Mat4 {
    let rot = glam::Quat::from_xyzw(
        pose.orientation.x,
        pose.orientation.y,
        pose.orientation.z,
        pose.orientation.w,
    );
    let pos = glam::Vec3::new(pose.position.x, pose.position.y, pose.position.z);
    glam::Mat4::from_rotation_translation(rot, pos).inverse()
}

// ── swapchain image wrapping ──────────────────────────────────────────────────

unsafe fn wrap_xr_image(
    device: &wgpu::Device,
    raw_img: u64,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
) -> wgpu::Texture {
    use ash::vk::Handle as _;

    let vk_image = ash::vk::Image::from_raw(raw_img);

    let hal_texture = {
        let hal_dev = unsafe {
            device
                .as_hal::<wgpu::hal::vulkan::Api>()
                .expect("wgpu not on Vulkan backend")
        };
        unsafe {
            hal_dev.texture_from_raw(
                vk_image,
                &wgpu::hal::TextureDescriptor {
                    label: Some("xr_swapchain"),
                    size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::wgt::TextureUses::COLOR_TARGET,
                    memory_flags: wgpu::hal::MemoryFlags::empty(),
                    view_formats: vec![],
                },
                None,
                wgpu::hal::vulkan::TextureMemory::External,
            )
        }
    };

    unsafe {
        device.create_texture_from_hal::<wgpu::hal::vulkan::Api>(
            hal_texture,
            &wgpu::TextureDescriptor {
                label: Some("xr_swapchain"),
                size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            },
        )
    }
}

// ── per-eye swapchain ─────────────────────────────────────────────────────────

pub struct EyeSwapchain {
    // textures must drop before swapchain: wgpu cleans up image views while
    // the VkImages still exist; the XR runtime frees VkImages on swapchain drop.
    pub textures: Vec<wgpu::Texture>,
    pub swapchain: xr::Swapchain<xr::Vulkan>,
    pub resolution: xr::Extent2Di,
    pub format: wgpu::TextureFormat,
}

// ── pre-init: XR instance + system, before wgpu device creation ──────────────

/// Holds the OpenXR instance and system ID before the wgpu device is created.
/// Used to query required Vulkan device extensions so the renderer can enable them.
pub struct VrPreInit {
    instance:           xr::Instance,
    system:             xr::SystemId,
    has_legacy_vulkan:  bool,
    pub has_cylinder:   bool,
    pub has_equirect2:  bool,
}

impl VrPreInit {
    pub fn new() -> Option<Self> {
        vprintln!("XR: loading OpenXR...");

        let xr_entry = xr::Entry::linked();

        let exts = xr_entry
            .enumerate_extensions()
            .map_err(|e| eprintln!("XR: enumerate_extensions failed: {e}"))
            .ok()?;

        if !exts.khr_vulkan_enable2 {
            eprintln!("XR: runtime does not support KHR_vulkan_enable2");
            return None;
        }

        // Enable both Vulkan bindings so we can query device extensions.
        // Also enable composition-layer extensions if the runtime supports them.
        let mut enabled = xr::ExtensionSet::default();
        enabled.khr_vulkan_enable2 = true;
        enabled.khr_vulkan_enable = exts.khr_vulkan_enable;
        enabled.khr_composition_layer_cylinder  = exts.khr_composition_layer_cylinder;
        enabled.khr_composition_layer_equirect2 = exts.khr_composition_layer_equirect2;

        let instance = xr_entry
            .create_instance(
                &xr::ApplicationInfo {
                    application_name: "equirect",
                    application_version: 1,
                    engine_name: "equirect",
                    engine_version: 1,
                    api_version: xr::Version::new(1, 0, 0),
                },
                &enabled,
                &[],
            )
            .map_err(|e| eprintln!("XR: create_instance failed: {e}"))
            .ok()?;

        let system = instance
            .system(xr::FormFactor::HEAD_MOUNTED_DISPLAY)
            .map_err(|e| eprintln!("XR: no HMD system found: {e}"))
            .ok()?;

        vprintln!("XR: HMD found");

        Some(Self {
            instance,
            system,
            has_legacy_vulkan: exts.khr_vulkan_enable,
            has_cylinder:  exts.khr_composition_layer_cylinder,
            // 180/360 video is drawn by our own shader sphere by default. Set EQUIRECT_USE_LAYER=1
            // to let SteamVR draw it as an equirect2 layer (hurt the eyes on the Vive Pro 2 in testing).
            has_equirect2: exts.khr_composition_layer_equirect2
                && std::env::var_os("EQUIRECT_USE_LAYER").is_some(),
        })
    }

    /// Returns the Vulkan device extensions required by the OpenXR runtime.
    /// The renderer must enable these when creating the wgpu Vulkan device.
    pub fn required_device_extensions(&self) -> Vec<&'static CStr> {
        if !self.has_legacy_vulkan {
            return Vec::new();
        }

        let ext_string = match self.instance.vulkan_legacy_device_extensions(self.system) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("XR: vulkan_legacy_device_extensions failed: {e}");
                return Vec::new();
            }
        };

        vprintln!("XR: runtime requires device extensions: {ext_string}");

        ext_string
            .split_ascii_whitespace()
            .map(|name| {
                let cstring = std::ffi::CString::new(name).unwrap();
                // Leak is acceptable for this one-time startup allocation.
                let leaked: &'static CStr = Box::leak(cstring.into_boxed_c_str());
                leaked
            })
            .collect()
    }
}

// ── VR context ────────────────────────────────────────────────────────────────

pub struct VrContext {
    // All XR/wgpu fields are ManuallyDrop so Drop can run them in order with tracing.
    frame_stream: std::mem::ManuallyDrop<xr::FrameStream<xr::Vulkan>>,
    frame_waiter: std::mem::ManuallyDrop<xr::FrameWaiter>,
    stage: std::mem::ManuallyDrop<xr::Space>,
    /// SteamVR's own centre ("Reset seated position" moves it). We draw in the stage, which that does not move,
    /// so we watch where this space sits in the stage and follow it. None if the runtime has no LOCAL space.
    local: Option<xr::Space>,
    /// Where `local` sat in the stage last frame (position, orientation).
    local_centre: Option<(glam::Vec3, glam::Quat)>,
    pub eyes: [std::mem::ManuallyDrop<EyeSwapchain>; 2],
    session: std::mem::ManuallyDrop<xr::Session<xr::Vulkan>>,
    instance: std::mem::ManuallyDrop<xr::Instance>,
    pub swapchain_format: wgpu::TextureFormat,
    /// Raw Vulkan format integer for the swapchain (needed when creating video swapchain).
    pub vk_format:        u32,
    pub has_cylinder:     bool,
    pub has_equirect2:    bool,
    view_type: xr::ViewConfigurationType,
    blend_mode: xr::EnvironmentBlendMode,
    running: bool,
    pub should_quit: bool,
    frame_count: u64,
    prev_menu_pressed: bool,
    prev_click: [bool; N_CURSORS],
    /// Desktop mouse and keyboard, on top of the controllers.
    desk: DeskInput,
    prev_both_grips: bool,
    prev_grip_pressed: bool,
    last_seek_time: Option<std::time::Instant>,
    /// Base orientation applied to all video layers.
    /// At startup this is a yaw-only rotation so the video faces the user
    /// regardless of where they happen to be looking vertically.
    /// After a grip press it is the full head orientation so the video is
    /// locked exactly to where the user was pointing.
    base_orientation: glam::Quat,
    /// Where the scene's middle is on the stage floor: the spot under the head when the scene was last centred.
    /// Panels, screen and pointer rays all live in the frame turned by `base_orientation` and moved here.
    base_origin: glam::Vec3,
    orientation_initialized: bool,
    /// The settings screen's Center button was pressed: centre on the next frame.
    center_asked: bool,
    // Input — NOT ManuallyDrop; dropped explicitly in Drop before the session.
    xr_input: Option<XrInput>,
}

impl Drop for VrContext {
    fn drop(&mut self) {
        // Drop action spaces before the session they belong to.
        self.xr_input = None;

        // SAFETY: every ManuallyDrop field is handled exactly once here.
        unsafe {
            // Drop the wgpu texture wrappers first while the Vulkan device is alive.
            for eye in &mut self.eyes {
                eye.textures.clear();
            }

            // The Oculus runtime frees swapchain/space GPU resources when
            // xrEndSession is called, so calling xrDestroySwapchain / xrDestroySpace
            // afterwards causes a double-free crash. Instead we forget those handles
            // and let xrDestroySession implicitly release all child objects, which
            // the OpenXR spec explicitly permits.
            let EyeSwapchain { swapchain: sc0, textures: t0, .. } =
                std::mem::ManuallyDrop::take(&mut self.eyes[0]);
            let EyeSwapchain { swapchain: sc1, textures: t1, .. } =
                std::mem::ManuallyDrop::take(&mut self.eyes[1]);
            drop(t0); // already empty
            drop(t1);
            std::mem::forget(sc0); // do NOT call xrDestroySwapchain
            std::mem::forget(sc1);

            let stage = std::mem::ManuallyDrop::take(&mut self.stage);
            std::mem::forget(stage); // do NOT call xrDestroySpace
            if let Some(local) = self.local.take() {
                std::mem::forget(local);
            }

            // xrDestroySession releases everything; xrDestroyInstance last.
            std::mem::ManuallyDrop::drop(&mut self.session);
            std::mem::ManuallyDrop::drop(&mut self.instance);

            // FrameStream / FrameWaiter hold no owned XR handles — just drop.
            std::mem::ManuallyDrop::drop(&mut self.frame_stream);
            std::mem::ManuallyDrop::drop(&mut self.frame_waiter);
        }
    }
}

impl VrContext {
    /// Complete XR initialisation using a pre-initialised instance/system and
    /// a fully constructed renderer whose Vulkan device has the required extensions.
    pub fn new(renderer: &Renderer, pre: VrPreInit) -> Option<Self> {
        let VrPreInit {
            instance: xr_instance,
            system,
            has_cylinder,
            has_equirect2,
            ..
        } = pre;

        // ── extract wgpu's Vulkan handles ─────────────────────────────────────
        let (vk_instance, vk_physical, vk_device, queue_family) = unsafe {
            use ash::vk::Handle as _;

            let hal_inst = renderer.instance
                .as_hal::<wgpu::hal::vulkan::Api>()
                .expect("wgpu not on Vulkan backend");
            let vi = hal_inst.shared_instance().raw_instance().handle().as_raw()
                as usize as *const std::ffi::c_void;

            let hal_adapt = renderer.adapter
                .as_hal::<wgpu::hal::vulkan::Api>()
                .expect("adapter not Vulkan");
            let vp = hal_adapt.raw_physical_device().as_raw() as usize
                as *const std::ffi::c_void;
            drop(hal_adapt);

            let hal_dev = renderer.device
                .as_hal::<wgpu::hal::vulkan::Api>()
                .expect("device not Vulkan");
            let vd = hal_dev.raw_device().handle().as_raw() as usize
                as *const std::ffi::c_void;
            let qf = hal_dev.queue_family_index();
            drop(hal_dev);

            (vi, vp, vd, qf)
        };

        // Required by the OpenXR spec before xrCreateSession.
        xr_instance
            .graphics_requirements::<xr::Vulkan>(system)
            .map_err(|e| eprintln!("XR: graphics_requirements failed: {e}"))
            .ok()?;

        // SteamVR refuses xrCreateSession unless xrGetVulkanGraphicsDevice was called first
        // ("failed to call xrGetVulkanGraphicsDevice before xrCreateSession").
        let xr_physical = unsafe { xr_instance.vulkan_graphics_device(system, vk_instance) }
            .map_err(|e| eprintln!("XR: vulkan_graphics_device failed: {e}"))
            .ok()?;
        if xr_physical != vk_physical {
            eprintln!("XR: warning: the runtime wants a different GPU than the renderer picked");
        }

        // ── create XR session ────────────────────────────────────────────────
        let (session, frame_waiter, frame_stream) = unsafe {
            xr_instance
                .create_session::<xr::Vulkan>(
                    system,
                    &xr::vulkan::SessionCreateInfo {
                        instance: vk_instance,
                        physical_device: vk_physical,
                        device: vk_device,
                        queue_family_index: queue_family,
                        queue_index: 0,
                    },
                )
                .map_err(|e| eprintln!("XR: create_session failed: {e}"))
                .ok()?
        };

        vprintln!("XR: session created");

        let stage = session
            .create_reference_space(xr::ReferenceSpaceType::STAGE, xr::Posef::IDENTITY)
            .map_err(|e| eprintln!("XR: create_reference_space failed: {e}"))
            .ok()?;

        let local = session
            .create_reference_space(xr::ReferenceSpaceType::LOCAL, xr::Posef::IDENTITY)
            .map_err(|e| eprintln!("XR: no LOCAL space, SteamVR's centre will not be followed: {e}"))
            .ok();

        let view_type = xr::ViewConfigurationType::PRIMARY_STEREO;
        let view_cfgs = xr_instance
            .enumerate_view_configuration_views(system, view_type)
            .map_err(|e| eprintln!("XR: enumerate_view_configuration_views failed: {e}"))
            .ok()?;

        let blend_mode = xr_instance
            .enumerate_environment_blend_modes(system, view_type)
            .map_err(|e| eprintln!("XR: enumerate_environment_blend_modes failed: {e}"))
            .ok()?
            .into_iter()
            .find(|&m| m == xr::EnvironmentBlendMode::OPAQUE)
            .unwrap_or(xr::EnvironmentBlendMode::OPAQUE);

        // ── pick swapchain format ────────────────────────────────────────────
        let supported = session
            .enumerate_swapchain_formats()
            .map_err(|e| eprintln!("XR: enumerate_swapchain_formats failed: {e}"))
            .ok()?;

        let desktop_fmt = renderer.surface_format();

        let (xr_vk_fmt, wgpu_fmt) = match wgpu_to_vk(desktop_fmt)
            .filter(|v| supported.contains(v))
            .map(|v| (v, desktop_fmt))
            .or_else(|| supported.iter().find_map(|&f| vk_to_wgpu(f).map(|wf| (f, wf))))
        {
            Some(v) => v,
            None => {
                eprintln!("XR: no recognised format in runtime list {supported:?}");
                return None;
            }
        };

        vprintln!("XR: swapchain format {wgpu_fmt:?}");

        // ── create per-eye swapchains ────────────────────────────────────────
        let mut eyes: [Option<EyeSwapchain>; 2] = [None, None];

        for (i, cfg) in view_cfgs.iter().enumerate().take(2) {
            let w = cfg.recommended_image_rect_width;
            let h = cfg.recommended_image_rect_height;

            let swapchain = session
                .create_swapchain(&xr::SwapchainCreateInfo {
                    create_flags: xr::SwapchainCreateFlags::EMPTY,
                    usage_flags: xr::SwapchainUsageFlags::COLOR_ATTACHMENT
                        | xr::SwapchainUsageFlags::SAMPLED,
                    format: xr_vk_fmt,
                    sample_count: 1,
                    width: w,
                    height: h,
                    face_count: 1,
                    array_size: 1,
                    mip_count: 1,
                })
                .map_err(|e| eprintln!("XR: create_swapchain eye {i} failed: {e}"))
                .ok()?;

            let raw_images = swapchain
                .enumerate_images()
                .map_err(|e| eprintln!("XR: enumerate_images eye {i} failed: {e}"))
                .ok()?;

            let textures = raw_images
                .iter()
                .map(|&img| unsafe { wrap_xr_image(&renderer.device, img, w, h, wgpu_fmt) })
                .collect();

            let eye = EyeSwapchain {
                textures,
                swapchain,
                resolution: xr::Extent2Di { width: w as i32, height: h as i32 },
                format: wgpu_fmt,
            };
            vprintln!("XR: eye {i}: {}x{} × {} images ({:?})", w, h, raw_images.len(), eye.format);
            eyes[i] = Some(eye);
        }

        let [Some(eye0), Some(eye1)] = eyes else {
            eprintln!("XR: failed to create both eye swapchains");
            return None;
        };

        vprintln!("XR: ready — waiting for headset (cylinder={has_cylinder}, equirect2={has_equirect2})");

        // Input actions — set up now so bindings are registered before session starts.
        let xr_input = XrInput::new(&xr_instance, &session);

        Some(VrContext {
            instance: std::mem::ManuallyDrop::new(xr_instance),
            session: std::mem::ManuallyDrop::new(session),
            frame_waiter: std::mem::ManuallyDrop::new(frame_waiter),
            frame_stream: std::mem::ManuallyDrop::new(frame_stream),
            stage: std::mem::ManuallyDrop::new(stage),
            local,
            local_centre: None,
            eyes: [
                std::mem::ManuallyDrop::new(eye0),
                std::mem::ManuallyDrop::new(eye1),
            ],
            swapchain_format: wgpu_fmt,
            vk_format: xr_vk_fmt,
            has_cylinder,
            has_equirect2,
            view_type,
            blend_mode,
            running: false,
            should_quit: false,
            frame_count: 0,
            prev_menu_pressed: false,
            prev_click: [false; N_CURSORS],
            desk: DeskInput::new(true),
            prev_both_grips: false,
            prev_grip_pressed: false,
            last_seek_time: None,
            base_orientation: glam::Quat::IDENTITY,
            base_origin: glam::Vec3::ZERO,
            orientation_initialized: false,
            center_asked: false,
            xr_input,
        })
    }

    /// Create an XR swapchain to hold video frames for composition layers.
    /// Call once per video file; drop before `VrContext` drops.
    pub fn create_video_swapchain(
        &self,
        device: &wgpu::Device,
        width:  u32,
        height: u32,
    ) -> Option<VideoSwapchain> {
        VideoSwapchain::new(
            &self.session,
            device,
            width,
            height,
            self.swapchain_format,
            self.vk_format,
        )
    }

    /// Ask the runtime to end the session cleanly (FOCUSED → STOPPING → EXITING).
    pub fn request_exit(&self) {
        if let Err(e) = self.session.request_exit() {
            eprintln!("XR: request_exit failed (may be OK if session not running): {e}");
        }
    }

    /// Put the scene's middle under `head` and turn it to face where the head looks: upright (yaw only) or, with
    /// `keep_pitch`, locked to the full head direction. Heights stay as they were (the Screen page sets those).
    fn center_scene(&mut self, head: &xr::Posef, keep_pitch: bool) {
        (self.base_orientation, self.base_origin) = scene_frame(head, keep_pitch);
    }

    /// The scene's frame (where the panels and the screen live) as a matrix into the stage.
    fn scene_to_stage(&self) -> glam::Mat4 {
        glam::Mat4::from_translation(self.base_origin) * glam::Mat4::from_quat(self.base_orientation)
    }

    pub fn poll_events(&mut self) -> bool {
        let mut buf = xr::EventDataBuffer::new();
        while let Some(event) = self.instance.poll_event(&mut buf).ok().flatten() {
            use xr::Event::*;
            match event {
                SessionStateChanged(e) => {
                    vprintln!("XR: session → {:?}", e.state());
                    match e.state() {
                        xr::SessionState::READY => {
                            self.session
                                .begin(self.view_type)
                                .expect("XR session.begin() failed");
                            self.running = true;
                            vprintln!("XR: rendering started");
                        }
                        xr::SessionState::STOPPING => {
                            self.session.end().expect("XR session.end() failed");
                            self.running = false;
                        }
                        xr::SessionState::EXITING | xr::SessionState::LOSS_PENDING => {
                            self.should_quit = true;
                        }
                        _ => {}
                    }
                }
                InstanceLossPending(_) => {
                    self.should_quit = true;
                }
                // SteamVR's "reset seated position" lands here. Only logged: the frame loop follows the
                // centre by watching the LOCAL space, and this line shows which space the runtime moved.
                ReferenceSpaceChangePending(e) => {
                    crate::diag::log(&format!(
                        "xr: reference space change pending: {:?}, pose valid {}",
                        e.reference_space_type(), e.pose_valid(),
                    ));
                }
                _ => {}
            }
        }
        self.running
    }

    pub fn render_frame(
        &mut self,
        renderer:         &Renderer,
        ui_assets:        &mut crate::ui::assets::UiAssets,
        // XR composition-layer path (quad / cylinder / equirect2).
        mut video_layer:  Option<(&mut VideoSwapchain, &VideoSettings)>,
        // Shader fallback for fisheye (or when no video swapchain exists yet).
        video_shader:     Option<(&VideoRenderer, &VideoTexture, &VideoSettings)>,
        mut panel:        Option<(&mut PanelRenderer, &ControlBarState)>,
        mut browser:      Option<(&mut PanelRenderer, &mut BrowserState)>,
        mut settings:     Option<(&mut PanelRenderer, SettingsView<'_>)>,
        pointer_renderer: Option<&PointerRenderer>,
    ) -> (ControlBarActions, BrowserActions, SettingsActions) {
        if !self.poll_events() {
            return (ControlBarActions::default(), BrowserActions::default(), SettingsActions::default());
        }

        let frame_state = match self.frame_waiter.wait() {
            Ok(fs) => fs,
            Err(e) => {
                eprintln!("XR: frame_waiter.wait failed: {e}");
                return (ControlBarActions::default(), BrowserActions::default(), SettingsActions::default());
            }
        };

        if let Err(e) = self.frame_stream.begin() {
            eprintln!("XR: frame_stream.begin failed: {e}");
            return (ControlBarActions::default(), BrowserActions::default(), SettingsActions::default());
        }

        if !frame_state.should_render {
            self.frame_stream
                .end(frame_state.predicted_display_time, self.blend_mode, &[])
                .unwrap_or_else(|e| eprintln!("XR: frame_stream.end (no render) failed: {e}"));
            self.frame_count += 1;
            return (ControlBarActions::default(), BrowserActions::default(), SettingsActions::default());
        }

        // Poll controller input using the frame's predicted display time.
        let controllers: [Option<ControllerState>; 2] =
            if let Some(ref input) = self.xr_input {
                input.poll(&self.session, &self.stage, frame_state.predicted_display_time)
            } else {
                [None, None]
            };

        if self.frame_count == 0 {
            vprintln!("XR: first rendered frame");
        }

        let (_flags, views) = match self
            .session
            .locate_views(self.view_type, frame_state.predicted_display_time, &self.stage)
        {
            Ok(v) => v,
            Err(e) => {
                eprintln!("XR: locate_views failed: {e}");
                self.frame_stream
                    .end(frame_state.predicted_display_time, self.blend_mode, &[])
                    .ok();
                return (ControlBarActions::default(), BrowserActions::default(), SettingsActions::default());
            }
        };

        // On the first rendered frame: centre on the head, yaw only, so the video faces the user
        // regardless of any pitch the headset had while being put on.
        let head = views[0].pose;
        if !self.orientation_initialized {
            self.center_scene(&head, false);
            self.orientation_initialized = true;
            vprintln!("XR: base orientation initialised (yaw-only)");
        }

        // SteamVR's centre moved (the user reset the seated position): follow it, so "centre" in SteamVR
        // works here too. The head now sits at SteamVR's new origin, so centring on it is the same thing.
        if let Some(local) = &self.local {
            match local.locate(&self.stage, frame_state.predicted_display_time) {
                Ok(loc) if loc.location_flags.contains(
                    xr::SpaceLocationFlags::POSITION_VALID | xr::SpaceLocationFlags::ORIENTATION_VALID,
                ) => {
                    let (p, o) = (loc.pose.position, loc.pose.orientation);
                    let now = (glam::Vec3::new(p.x, p.y, p.z), glam::Quat::from_xyzw(o.x, o.y, o.z, o.w));
                    if let Some(before) = self.local_centre {
                        if centre_moved(before, now) {
                            crate::diag::log("xr: SteamVR centre moved: centring the scene on the head");
                            self.center_scene(&head, false);
                        }
                    }
                    self.local_centre = Some(now);
                }
                _ => {}
            }
        }
        // The settings screen's Center button.
        if std::mem::take(&mut self.center_asked) {
            self.center_scene(&head, false);
            crate::diag::log("xr: centred on the head (Center button)");
        }

        // Edge-detect grip press — either controller resets the base orientation
        // to the full head orientation (including pitch).
        let any_grip_now = controllers.iter().flatten().any(|c| c.grip_pressed);
        let grip_just_pressed = any_grip_now && !self.prev_grip_pressed;
        self.prev_grip_pressed = any_grip_now;
        if grip_just_pressed {
            self.center_scene(&head, true);
            vprintln!("XR: scene centred (full orientation)");
        }

        // Edge-detect B/Y menu button across both controllers.
        let any_menu_now = controllers.iter().flatten().any(|c| c.menu_pressed);
        let menu_just_pressed = any_menu_now && !self.prev_menu_pressed;
        self.prev_menu_pressed = any_menu_now;

        // Thumbstick X → seek ±10 s, throttled to at most once per 100 ms.
        // Use the largest-magnitude X value across both controllers.
        let thumb_x = controllers.iter().flatten()
            .map(|c| c.thumbstick_x)
            .max_by(|a, b| a.abs().partial_cmp(&b.abs()).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or(0.0);

        // Build controller rays in the panel's local frame (inverse of base_orientation).
        // Panels live at (0, y, -2) in local space; base_orientation rotates that frame
        // into stage space.  Hit-testing and UI input both need local-frame rays.
        let inv_orient = self.base_orientation.inverse();
        let origin = self.base_origin;
        // Desktop mouse: a third pointer aimed straight at whichever panel is showing
        // (already in the panel frame, so it skips the orientation above).
        let mouse: Option<ControllerState> = {
            let active: Option<&PanelRenderer> = panel.as_ref().map(|(p, _)| &**p)
                .or_else(|| browser.as_ref().map(|(p, _)| &**p))
                .or_else(|| settings.as_ref().map(|(p, _)| &**p));
            active.and_then(|p| self.desk.pointer(p))
        };
        let oriented_controllers: [Option<ControllerState>; N_CURSORS] = [
            controllers[0].map(|c| ControllerState {
                ray_origin: inv_orient * (c.ray_origin - origin),
                ray_dir:    inv_orient * c.ray_dir,
                ..c
            }),
            controllers[1].map(|c| ControllerState {
                ray_origin: inv_orient * (c.ray_origin - origin),
                ray_dir:    inv_orient * c.ray_dir,
                ..c
            }),
            mouse,
        ];

        // Update the control-bar panel UI texture once (shared by both eyes).
        let mut cb_actions = match &mut panel {
            Some((p, s)) => p.update(&renderer.device, &renderer.queue, ui_assets, s, &oriented_controllers),
            None => ControlBarActions::default(),
        };
        // Trigger pull (DeoVR style): pops the controls up when everything is hidden, and
        // hides them when the pull is aimed away from the control bar.
        let click_now: [bool; N_CURSORS] = [
            controllers[0].map_or(false, |c| c.clicking),
            controllers[1].map_or(false, |c| c.clicking),
            mouse.map_or(false, |m| m.clicking),
        ];
        // A mouse click while everything is hidden brings the control bar back
        // (no panel is showing then, so the mouse has no pointer to click with).
        let mouse_click = self.desk.take_click();
        let all_hidden = panel.is_none() && browser.is_none() && settings.is_none();
        let mut trigger_toggle = all_hidden && mouse_click;
        for i in 0..N_CURSORS {
            if click_now[i] && !self.prev_click[i] {
                if all_hidden {
                    trigger_toggle = true;
                } else if let (Some((p, _)), Some(oc)) = (&panel, &oriented_controllers[i]) {
                    if p.hit_test_3d(oc.ray_origin, oc.ray_dir).is_none() {
                        trigger_toggle = true;
                    }
                }
            }
        }
        self.prev_click = click_now;
        cb_actions.menu_toggle = menu_just_pressed || trigger_toggle;
        // Both grips squeezed together = reset the video settings to the detected ones.
        let both_grips = controllers[0].map_or(false, |c| c.grip_pressed)
            && controllers[1].map_or(false, |c| c.grip_pressed);
        cb_actions.reset_settings = both_grips && !self.prev_both_grips;
        self.prev_both_grips = both_grips;

        // Emit a seek delta when the thumbstick X is past the dead zone and
        // at least 100 ms have elapsed since the last seek emission.
        const SEEK_DEAD_ZONE: f32 = 0.2;
        const SEEK_INTERVAL:  std::time::Duration = std::time::Duration::from_millis(100);
        if thumb_x.abs() > SEEK_DEAD_ZONE {
            let now = std::time::Instant::now();
            let due = self.last_seek_time
                .map(|t| now.duration_since(t) >= SEEK_INTERVAL)
                .unwrap_or(true);
            if due {
                self.last_seek_time = Some(now);
                cb_actions.seek_delta_secs = Some(thumb_x as f64 * 10.0);
            }
        } else {
            // Reset the throttle when the stick returns to centre so the next
            // deflection fires immediately rather than waiting out the interval.
            self.last_seek_time = None;
        }
        // Keyboard shortcuts last, so a held arrow key is not overwritten by the thumbstick seek.
        self.desk.apply_keys(&mut cb_actions);

        // Update the browser panel UI texture once (shared by both eyes).
        let browser_actions = match &mut browser {
            Some((p, s)) => p.update_ui(
                &renderer.device, &renderer.queue, ui_assets, &oriented_controllers,
                |ui| browser_draw(ui, s),
            ),
            None => BrowserActions::default(),
        };

        // Update the settings panel UI texture once (shared by both eyes).
        let settings_actions = match &mut settings {
            Some((p, s)) => p.update_ui(
                &renderer.device, &renderer.queue, ui_assets, &oriented_controllers,
                |ui| settings_draw(ui, s),
            ),
            None => SettingsActions::default(),
        };

        // Compute where each controller ray hits any active panel (for beam truncation).
        // Hit-test in local (panel) space, then rotate the result back to stage space
        // so the pointer beam (rendered with stage-space view_mat) lines up correctly.
        let pointer_hits: [Option<glam::Vec3>; 2] = {
            let mut h = [None, None];
            for (i, oc) in oriented_controllers.iter().take(2).enumerate() {
                if let Some(oc) = oc {
                    let local_hit = panel.as_ref()
                        .and_then(|(p, _)| p.hit_test_3d(oc.ray_origin, oc.ray_dir))
                        .or_else(|| browser.as_ref()
                            .and_then(|(p, _)| p.hit_test_3d(oc.ray_origin, oc.ray_dir)))
                        .or_else(|| settings.as_ref()
                            .and_then(|(p, _)| p.hit_test_3d(oc.ray_origin, oc.ray_dir)));
                    // Rotate hit back to stage space for the pointer renderer.
                    h[i] = local_hit.map(|hp| self.base_orientation * hp + self.base_origin);
                }
            }
            h
        };

        // ── XR layer video: blit once per frame (not per eye) ────────────────
        // When using composition layers, the runtime handles per-eye projection.
        if let Some((sc, _vs)) = &mut video_layer {
            sc.update(&renderer.device, &renderer.queue);
        }

        // ── per-eye render (panels + pointer + fisheye shader fallback) ───────
        for eye in 0..2 {
            let img_idx = self.eyes[eye].swapchain.acquire_image().unwrap() as usize;
            self.eyes[eye].swapchain.wait_image(xr::Duration::INFINITE).unwrap();
            let tex_view = self.eyes[eye].textures[img_idx].create_view(&Default::default());

            let proj     = fov_to_projection(views[eye].fov, 0.1, 100.0);
            let view_mat = pose_to_view(views[eye].pose);

            // Fisheye shader path (only when no XR layer is in use).
            if let Some((vr_rend, _, vs)) = video_shader {
                // Pre-rotate the view by base_orientation so the video's forward axis
                // (-Z in video space) maps to the user's captured direction in stage space.
                let view_oriented = view_mat * self.scene_to_stage();
                vr_rend.render_eye(&tex_view, proj, view_oriented, vs, eye, &renderer.device, &renderer.queue);
            } else {
                // Clear to transparent so the video composition layer shows through.
                renderer.clear_xr_eye(&tex_view);
            }

            // Panels live in the "oriented" local frame.  Pre-multiply view by the base
            // orientation so that a panel at local (0, y, -2) appears in front of the
            // user's initial (or grip-reset) facing direction in stage space.
            let view_oriented = view_mat * self.scene_to_stage();
            if let Some((p, _)) = &panel {
                p.render_eye(&tex_view, proj, view_oriented, &renderer.device, &renderer.queue);
            }
            if let Some((p, _)) = &browser {
                p.render_eye(&tex_view, proj, view_oriented, &renderer.device, &renderer.queue);
            }
            if let Some((p, _)) = &settings {
                p.render_eye(&tex_view, proj, view_oriented, &renderer.device, &renderer.queue);
            }
            if let Some(ptr) = pointer_renderer {
                ptr.render_eye(&tex_view, proj, view_mat, &controllers, &pointer_hits, &renderer.device, &renderer.queue);
            }

            self.eyes[eye].swapchain.release_image().unwrap();
        }

        // ── build composition layer list ──────────────────────────────────────

        let eyes  = &self.eyes;
        let stage = &self.stage;

        let proj_views = [
            xr::CompositionLayerProjectionView::new()
                .pose(views[0].pose)
                .fov(views[0].fov)
                .sub_image(
                    xr::SwapchainSubImage::new()
                        .swapchain(&eyes[0].swapchain)
                        .image_array_index(0)
                        .image_rect(xr::Rect2Di {
                            offset: xr::Offset2Di { x: 0, y: 0 },
                            extent: eyes[0].resolution,
                        }),
                ),
            xr::CompositionLayerProjectionView::new()
                .pose(views[1].pose)
                .fov(views[1].fov)
                .sub_image(
                    xr::SwapchainSubImage::new()
                        .swapchain(&eyes[1].swapchain)
                        .image_array_index(0)
                        .image_rect(xr::Rect2Di {
                            offset: xr::Offset2Di { x: 0, y: 0 },
                            extent: eyes[1].resolution,
                        }),
                ),
        ];

        // Projection layer blends over the video layer (panels + pointer are
        // transparent everywhere except where they're actually drawn).
        let proj_layer = xr::CompositionLayerProjection::new()
            .layer_flags(xr::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA)
            .space(stage)
            .views(&proj_views);

        // Pre-allocate all possible video layer structs on the stack.
        // We fill in the applicable ones and push references into `layer_refs`.
        let mut quad_layers: [xr::CompositionLayerQuad<xr::Vulkan>; 2] =
            [xr::CompositionLayerQuad::new(), xr::CompositionLayerQuad::new()];
        let mut cyl_layers: [xr::CompositionLayerCylinderKHR<xr::Vulkan>; 2] =
            [xr::CompositionLayerCylinderKHR::new(), xr::CompositionLayerCylinderKHR::new()];
        let mut eq_layers: [xr::CompositionLayerEquirect2KHR<xr::Vulkan>; 2] =
            [xr::CompositionLayerEquirect2KHR::new(), xr::CompositionLayerEquirect2KHR::new()];

        let mut layer_refs: Vec<&xr::CompositionLayerBase<xr::Vulkan>> = Vec::new();

        if let Some((sc, vs)) = &video_layer {
            let n = VideoSwapchain::layer_count(vs);
            let mode = effective_mode(vs, self.has_cylinder, self.has_equirect2);
            if self.frame_count == 0 {
                vprintln!("XR: video layer mode={:?} n={n} mode_setting={:?}", mode, vs.mode);
            }

            // Physical screen dimensions (meters).
            let screen_w    = 3.2_f32;
            let disp_aspect = sc.display_aspect(vs);
            let screen_h    = screen_w / disp_aspect;
            let zoom        = vs.zoom.max(0.1);

            // ── fill layer structs ────────────────────────────────────────────
            for i in 0..n {
                let eye_vis = VideoSwapchain::eye_visibility(vs, i);
                let sub     = sc.sub_image(vs, i);
                match mode {
                    EffectiveLayerMode::Quad => {
                        // Cap portrait videos so they are never taller in world
                        // space than a 16:9 landscape video at the same width.
                        // A 16:9 source gives screen_h = 3.2 / (16/9) = 1.8 m.
                        // A 9:16 source would give screen_h = 3.2 / (9/16) = 5.7 m.
                        // Instead we scale both axes down to keep screen_h = 1.8 m,
                        // giving a 1.0 m × 1.8 m panel — the correct aspect ratio,
                        // just smaller.
                        const REF_ASPECT: f32 = 16.0 / 9.0;
                        let max_h = screen_w / REF_ASPECT;
                        let (w, h) = if screen_h > max_h {
                            let scale = max_h / screen_h;
                            (screen_w * scale, max_h)
                        } else {
                            (screen_w, screen_h)
                        };
                        quad_layers[i] = xr::CompositionLayerQuad::new()
                            .space(stage)
                            .eye_visibility(eye_vis)
                            .sub_image(sub)
                            .pose(oriented_pose(2.5, 1.0, self.base_orientation, self.base_origin))
                            .size(xr::Extent2Df {
                                width:  w * zoom,
                                height: h * zoom,
                            });
                    }
                    EffectiveLayerMode::Cylinder => {
                        let central_angle = std::f32::consts::PI * 2.0 / 3.0 * zoom; // 120°*zoom
                        let radius        = 2.5_f32;
                        cyl_layers[i] = xr::CompositionLayerCylinderKHR::new()
                            .space(stage)
                            .eye_visibility(eye_vis)
                            .sub_image(sub)
                            .pose(oriented_pose(0.0, 1.0, self.base_orientation, self.base_origin))
                            .radius(radius)
                            .central_angle(central_angle)
                            .aspect_ratio(disp_aspect);
                    }
                    EffectiveLayerMode::Equirect180 => {
                        let (ch, vu, vd) = zoom_angles(
                            std::f32::consts::PI, std::f32::consts::FRAC_PI_2,
                            std::f32::consts::FRAC_PI_2, zoom,
                        );
                        eq_layers[i] = xr::CompositionLayerEquirect2KHR::new()
                            .space(stage)
                            .eye_visibility(eye_vis)
                            .sub_image(sub)
                            .pose(oriented_pose(0.0, 0.0, self.base_orientation, self.base_origin))
                            .radius(0.0)
                            .central_horizontal_angle(ch)
                            .upper_vertical_angle(vu)
                            .lower_vertical_angle(-vd);
                    }
                    EffectiveLayerMode::Equirect360 => {
                        let (ch, vu, vd) = zoom_angles(
                            std::f32::consts::TAU, std::f32::consts::FRAC_PI_2,
                            std::f32::consts::FRAC_PI_2, zoom,
                        );
                        eq_layers[i] = xr::CompositionLayerEquirect2KHR::new()
                            .space(stage)
                            .eye_visibility(eye_vis)
                            .sub_image(sub)
                            .pose(oriented_pose(0.0, 0.0, self.base_orientation, self.base_origin))
                            .radius(0.0)
                            .central_horizontal_angle(ch)
                            .upper_vertical_angle(vu)
                            .lower_vertical_angle(-vd);
                    }
                }
            }

            // ── push refs (separate loop so fill borrows don't overlap) ───────
            for i in 0..n {
                match mode {
                    EffectiveLayerMode::Quad      => layer_refs.push(&quad_layers[i]),
                    EffectiveLayerMode::Cylinder  => layer_refs.push(&cyl_layers[i]),
                    EffectiveLayerMode::Equirect180
                    | EffectiveLayerMode::Equirect360 => layer_refs.push(&eq_layers[i]),
                }
            }
        }

        // Video layers go first (behind panels).
        layer_refs.push(&proj_layer);

        self.frame_stream
            .end(frame_state.predicted_display_time, self.blend_mode, &layer_refs)
            .unwrap_or_else(|e| eprintln!("XR: frame_stream.end failed: {e}"));

        self.frame_count += 1;
        (cb_actions, browser_actions, settings_actions)
    }
}

impl crate::mode::Mode for VrContext {
    fn target_format(&self) -> wgpu::TextureFormat { self.swapchain_format }
    fn supports_video_layer(&self) -> bool { true }
    fn supports_equirect2(&self) -> bool { self.has_equirect2 }
    fn should_quit(&self) -> bool { self.should_quit }
    fn request_exit(&self) { VrContext::request_exit(self); }

    fn handle_window_event(&mut self, event: &winit::event::WindowEvent) {
        self.desk.handle_window_event(event);
    }
    fn handle_device_event(&mut self, event: &winit::event::DeviceEvent) {
        self.desk.handle_device_event(event);
    }
    fn set_mouse_active(&mut self, on: bool) { self.desk.set_active(on); }
    fn center(&mut self) { self.center_asked = true; }

    fn create_video_swapchain(
        &self,
        device: &wgpu::Device,
        width:  u32,
        height: u32,
    ) -> Option<VideoSwapchain> {
        VrContext::create_video_swapchain(self, device, width, height)
    }

    fn render_frame(
        &mut self,
        renderer:         &Renderer,
        ui_assets:        &mut crate::ui::assets::UiAssets,
        video_layer:      Option<(&mut VideoSwapchain, &VideoSettings)>,
        video_shader:     Option<(&VideoRenderer, &VideoTexture, &VideoSettings)>,
        panel:            Option<(&mut PanelRenderer, &ControlBarState)>,
        browser:          Option<(&mut PanelRenderer, &mut BrowserState)>,
        settings:         Option<(&mut PanelRenderer, SettingsView<'_>)>,
        pointer_renderer: Option<&PointerRenderer>,
    ) -> (ControlBarActions, BrowserActions, SettingsActions) {
        VrContext::render_frame(
            self, renderer, ui_assets, video_layer, video_shader, panel, browser, settings, pointer_renderer,
        )
    }
}

#[cfg(test)]
mod centre_tests {
    use super::*;

    fn head(x: f32, y: f32, z: f32, yaw_deg: f32, pitch_deg: f32) -> xr::Posef {
        let q = glam::Quat::from_rotation_y(yaw_deg.to_radians()) * glam::Quat::from_rotation_x(pitch_deg.to_radians());
        xr::Posef {
            position: xr::Vector3f { x, y, z },
            orientation: xr::Quaternionf { x: q.x, y: q.y, z: q.z, w: q.w },
        }
    }

    #[test]
    fn centring_puts_the_panels_in_front_of_the_head_wherever_it_is() {
        // Head 1.5 m right of and 0.7 m behind the stage middle, turned 90 degrees to the left (looking along -X).
        let (orient, origin) = scene_frame(&head(1.5, 1.2, 0.7, 90.0, 0.0), false);
        assert_eq!(origin, glam::Vec3::new(1.5, 0.0, 0.7), "the origin is the floor spot under the head");
        // A panel 2 m ahead at 1.4 m: ahead is -X now, the height stays the stage height.
        let panel = origin + orient * glam::Vec3::new(0.0, 1.4, -2.0);
        assert!((panel - glam::Vec3::new(-0.5, 1.4, 0.7)).length() < 1e-5, "{panel:?}");
    }

    #[test]
    fn yaw_only_ignores_the_pitch_and_a_grip_keeps_it() {
        let h = head(0.0, 1.0, 0.0, 0.0, 40.0);
        let (upright, _) = scene_frame(&h, false);
        let (locked, _) = scene_frame(&h, true);
        let ahead = |q: glam::Quat| q * glam::Vec3::NEG_Z;
        assert!(ahead(upright).y.abs() < 1e-5, "upright stays level");
        assert!(ahead(locked).y > 0.5, "the grip locks to where the head points: {:?}", ahead(locked));
    }

    #[test]
    fn only_a_real_move_of_steamvrs_centre_counts() {
        let at = (glam::Vec3::new(0.0, 0.0, 0.0), glam::Quat::IDENTITY);
        let jitter = (glam::Vec3::new(0.004, 0.0, -0.003), glam::Quat::from_rotation_y(0.2_f32.to_radians()));
        let stepped = (glam::Vec3::new(0.3, 0.0, 0.0), glam::Quat::IDENTITY);
        let turned = (glam::Vec3::ZERO, glam::Quat::from_rotation_y(25.0_f32.to_radians()));
        assert!(!centre_moved(at, jitter));
        assert!(centre_moved(at, stepped));
        assert!(centre_moved(at, turned));
    }
}
