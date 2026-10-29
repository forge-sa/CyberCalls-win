//! Click-through fullscreen overlay that draws the call bubbles with wgpu + glyphon.
//! The surface only exists while a call is live or bubbles are still fading out.
//! The scene and rendering live here; `window` owns the Win32 window and the event loop.

use anyhow::{Context, Result};
use glyphon::{
    cosmic_text::Align, Attrs, Buffer, Cache, Color, Family, FontSystem, Metrics, Resolution,
    Shaping, SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Weight,
};
use std::time::Instant;

use crate::config::OverlayConfig;
use crate::Speaker;

mod window;

pub use window::{channel, run, UiSender};

static FONT_REGULAR: &[u8] = include_bytes!("../assets/fonts/Tektur-500.ttf");
static FONT_BOLD: &[u8] = include_bytes!("../assets/fonts/Tektur-800.ttf");
const FAMILY: &str = "Tektur";

#[derive(Debug)]
pub enum UiEvent {
    CallStarted { peer: String },
    PeerName(String),
    CallEnded,
    Speaking { speaker: Speaker, on: bool },
    Line { id: u64, speaker: Speaker, text: String, done: bool },
    Drop { id: u64 },
}

struct Style {
    fill: [u8; 3],
    fill_alpha: f32,
    ring: [u8; 3],
    accent: [u8; 3],
    text: [u8; 3],
    outline: [u8; 3],
}

// colors picked from the reference frame
const RED: Style = Style {
    fill: [222, 52, 68],
    fill_alpha: 0.74,
    ring: [255, 24, 30],
    accent: [190, 255, 245],
    text: [98, 242, 228],
    outline: [6, 32, 46],
};
const GREEN: Style = Style {
    fill: [64, 226, 84],
    fill_alpha: 0.64,
    ring: [96, 255, 112],
    accent: [255, 120, 220],
    text: [246, 170, 72],
    outline: [52, 26, 6],
};

// you are the green box on the right, whoever you're talking to is red on the left
fn style(s: Speaker) -> &'static Style {
    match s {
        Speaker::Me => &GREEN,
        Speaker::Peer => &RED,
    }
}

fn on_right(s: Speaker) -> bool {
    s == Speaker::Me
}

const POP_SECS: f32 = 0.45;
const DIE_SECS: f32 = 0.35;
const END_LINGER_SECS: f32 = 4.0;

fn hash(n: f32) -> f32 {
    (n.sin() * 43758.547).fract().abs()
}

fn ease_out_back(x: f32) -> f32 {
    let c1 = 1.70158;
    let c3 = c1 + 1.0;
    1.0 + c3 * (x - 1.0).powi(3) + c1 * (x - 1.0).powi(2)
}

fn srgb_to_linear(c: u8) -> f32 {
    let c = c as f32 / 255.0;
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

fn color3(c: [u8; 3], linear: bool) -> [f32; 3] {
    if linear {
        c.map(srgb_to_linear)
    } else {
        c.map(|v| v as f32 / 255.0)
    }
}

fn attrs(weight: u16) -> Attrs<'static> {
    Attrs::new().family(Family::Name(FAMILY)).weight(Weight(weight))
}

/// A font system holding only the bundled Tektur faces.
fn font_system() -> FontSystem {
    let mut db = glyphon::fontdb::Database::new();
    db.load_font_data(FONT_REGULAR.to_vec());
    db.load_font_data(FONT_BOLD.to_vec());
    FontSystem::new_with_locale_and_db("en-US".into(), db)
}

// ---------------------------------------------------------------------------------------------
// scene: bubbles and their animation state, independent of Win32/wgpu

struct Bubble {
    id: u64,
    speaker: Speaker,
    text: String,
    done: bool,
    born: f32,
    revealed: f32,
    shown: usize,
    done_at: Option<f32>,
    dying: Option<f32>,
    y: f32,
    label: Buffer,
    body: Buffer,
    body_h: f32,
    seed: f32,
    phase: f32,
}

struct Sizes {
    b: f32,
    label: Metrics,
    body: Metrics,
    call: Metrics,
    gap: f32,
    spacing: f32,
}

impl Sizes {
    fn new(b: f32) -> Self {
        Self {
            b,
            label: Metrics::new(b * 0.30, b * 0.36),
            body: Metrics::new(b * 0.26, b * 0.31),
            call: Metrics::new(b * 0.31, b * 0.34),
            gap: b * 0.14,
            spacing: b * 0.16,
        }
    }
}

struct Scene {
    cfg: OverlayConfig,
    m: Sizes,
    me: String,
    peer: String,
    bubbles: Vec<Bubble>,
    speaking: [bool; 2],
    active: bool,
    ended_at: Option<f32>,
    start: Instant,
    t: f32,
    call: Buffer,
    call_w: f32,
}

struct BoxInst {
    x: f32,
    y: f32,
    alpha: f32,
    glitch: f32,
    scale: f32,
    seed: f32,
    phase: f32,
    speaker: Speaker,
}

impl Scene {
    fn new(cfg: OverlayConfig, me: String, fonts: &mut FontSystem) -> Self {
        let m = Sizes::new(cfg.box_size);
        let mut call = Buffer::new(fonts, m.call);
        call.set_size(None, None);
        call.set_text("CALL", &attrs(800), Shaping::Advanced, None);
        call.shape_until_scroll(fonts, false);
        let call_w = call.layout_runs().map(|r| r.line_w).fold(0.0, f32::max);
        Self {
            cfg,
            m,
            me,
            peer: String::new(),
            bubbles: Vec::new(),
            speaking: [false; 2],
            active: false,
            ended_at: None,
            start: Instant::now(),
            t: 0.0,
            call,
            call_w,
        }
    }

    fn wrap_width(&self) -> f32 {
        self.cfg.column_width - self.m.b - self.m.gap
    }

    fn name(&self, s: Speaker) -> &str {
        match s {
            Speaker::Me => &self.me,
            Speaker::Peer => &self.peer,
        }
    }

    fn apply(&mut self, ev: UiEvent, fonts: &mut FontSystem) {
        let now = self.start.elapsed().as_secs_f32();
        match ev {
            UiEvent::CallStarted { peer } => {
                self.peer = peer;
                self.active = true;
                self.ended_at = None;
            }
            UiEvent::PeerName(p) => self.peer = p,
            UiEvent::CallEnded => {
                self.active = false;
                self.ended_at = Some(now);
                self.speaking = [false; 2];
            }
            UiEvent::Speaking { speaker, on } => self.speaking[speaker as usize] = on,
            UiEvent::Line { id, speaker, text, done } => {
                if let Some(b) = self.bubbles.iter_mut().find(|b| b.id == id) {
                    b.text = text;
                    b.done = done;
                    b.revealed = b.revealed.min(b.text.chars().count() as f32);
                    b.shown = usize::MAX; // force re-layout
                } else if !text.is_empty() {
                    let mut label = Buffer::new(fonts, self.m.label);
                    label.set_size(None, None);
                    label.set_text(self.name(speaker), &attrs(500), Shaping::Advanced, None);
                    label.shape_until_scroll(fonts, false);
                    let mut body = Buffer::new(fonts, self.m.body);
                    body.set_size(Some(self.wrap_width()), None);
                    self.bubbles.push(Bubble {
                        id,
                        speaker,
                        text,
                        done,
                        born: now,
                        revealed: 0.0,
                        shown: usize::MAX,
                        done_at: None,
                        dying: None,
                        y: f32::NAN,
                        label,
                        body,
                        body_h: 0.0,
                        seed: hash(id as f32 * 7.31) * 100.0,
                        phase: hash(id as f32 * 3.7) * 6.28,
                    });
                }
            }
            UiEvent::Drop { id } => {
                if let Some(b) = self.bubbles.iter_mut().find(|b| b.id == id) {
                    b.dying.get_or_insert(now);
                }
            }
        }
    }

    /// Advance animations. Returns whether anything is still moving.
    fn update(&mut self, fonts: &mut FontSystem, screen_h: f32) -> bool {
        let now = self.start.elapsed().as_secs_f32();
        let dt = (now - self.t).clamp(0.0, 0.1);
        self.t = now;
        let cps = self.cfg.chars_per_sec;
        let linger = self.cfg.linger_secs;
        let ended = self.ended_at.map(|e| now - e > END_LINGER_SECS).unwrap_or(false);
        let wrap = self.wrap_width();
        let body_line = self.m.body.line_height;

        // newest bubble per speaker spins fast while that person is talking
        let newest: [Option<u64>; 2] = [Speaker::Me, Speaker::Peer]
            .map(|s| self.bubbles.iter().rev().find(|b| b.speaker == s).map(|b| b.id));

        for b in &mut self.bubbles {
            let len = b.text.chars().count();
            if now - b.born > POP_SECS * 0.6 {
                b.revealed = (b.revealed + dt * cps).min(len as f32);
            }
            let shown = b.revealed as usize;
            if shown != b.shown {
                b.shown = shown;
                let split = b.text.char_indices().nth(shown).map_or(b.text.len(), |(i, _)| i);
                let hidden = attrs(500).color(Color::rgba(0, 0, 0, 0));
                let align = if on_right(b.speaker) { Align::Right } else { Align::Left };
                b.body.set_size(Some(wrap), None);
                b.body.set_rich_text(
                    [(&b.text[..split], attrs(500)), (&b.text[split..], hidden)],
                    &attrs(500),
                    Shaping::Advanced,
                    Some(align),
                );
                b.body.shape_until_scroll(fonts, false);
                b.body_h = b.body.layout_runs().count() as f32 * body_line;
            }
            if b.done && shown >= len && b.done_at.is_none() {
                b.done_at = Some(now);
            }
            if b.dying.is_none()
                && (b.done_at.is_some_and(|d| now - d > linger) || ended)
            {
                b.dying = Some(now);
            }
            let fast = self.speaking[b.speaker as usize] && newest[b.speaker as usize] == Some(b.id);
            b.phase += dt * if fast { 4.5 } else { 0.9 };
        }
        self.bubbles.retain(|b| b.dying.is_none_or(|d| now - d < DIE_SECS));

        // stack top-down; when the column overflows, retire the oldest lines
        let top = self.cfg.top * screen_h;
        let limit = (self.cfg.bottom - self.cfg.top) * screen_h;
        let heights: Vec<f32> = self.bubbles.iter().map(|b| self.bubble_h(b)).collect();
        let mut total: f32 = heights.iter().sum();
        for (b, h) in self.bubbles.iter_mut().zip(&heights) {
            if total <= limit {
                break;
            }
            if b.dying.is_none() {
                b.dying = Some(now);
            }
            total -= h;
        }
        let mut y = top;
        let k = 1.0 - (-dt * 14.0).exp();
        for (b, h) in self.bubbles.iter_mut().zip(&heights) {
            if b.y.is_nan() {
                b.y = y;
            }
            b.y += (y - b.y) * k;
            y += h;
        }

        !self.bubbles.is_empty()
    }

    fn bubble_h(&self, b: &Bubble) -> f32 {
        self.m.label.line_height + self.m.b.max(b.body_h + self.m.b * 0.04) + self.m.spacing
    }

    /// (opacity, glitch, scale) of a bubble right now.
    fn fx(&self, b: &Bubble) -> (f32, f32, f32) {
        let age = self.t - b.born;
        let p = (age / POP_SECS).clamp(0.0, 1.0);
        let mut alpha = 1.0;
        let mut glitch = (1.0 - p).powf(1.5);
        let scale = 0.65 + 0.35 * ease_out_back(p);
        if p < 1.0 && hash((self.t * 30.0).floor() + b.seed) > 0.35 + p {
            alpha = 0.0; // flicker while materializing
        }
        // rare idle hiccup
        if hash((self.t * 5.0).floor() * 1.37 + b.seed * 7.0) > 0.985 {
            glitch = glitch.max(0.35);
        }
        if let Some(d) = b.dying {
            let q = ((self.t - d) / DIE_SECS).clamp(0.0, 1.0);
            alpha *= 1.0 - q;
            glitch = glitch.max(q);
        }
        (alpha, glitch, scale)
    }

    fn column_x(&self, screen_w: f32) -> f32 {
        self.cfg.center_x * screen_w - self.cfg.column_width * 0.5
    }

    fn box_pos(&self, b: &Bubble, screen_w: f32) -> (f32, f32) {
        let x0 = self.column_x(screen_w);
        let x = if on_right(b.speaker) { x0 + self.cfg.column_width - self.m.b } else { x0 };
        (x, b.y + self.m.label.line_height)
    }

    fn boxes(&self, screen_w: f32) -> Vec<BoxInst> {
        self.bubbles
            .iter()
            .map(|b| {
                let (x, y) = self.box_pos(b, screen_w);
                let (alpha, glitch, scale) = self.fx(b);
                BoxInst { x, y, alpha, glitch, scale, seed: b.seed, phase: b.phase, speaker: b.speaker }
            })
            .collect()
    }

    fn text_areas<'a>(&'a self, screen_w: f32, bounds: TextBounds, out: &mut Vec<TextArea<'a>>) {
        let o = (self.m.b * 0.018).max(1.5);
        let ring: [(f32, f32); 8] = std::array::from_fn(|i| {
            let a = i as f32 * std::f32::consts::FRAC_PI_4;
            (a.cos() * o, a.sin() * o)
        });
        for b in &self.bubbles {
            let (alpha, glitch, _) = self.fx(b);
            if alpha <= 0.0 {
                continue;
            }
            let st = style(b.speaker);
            let a = (alpha * 255.0) as u8;
            let fill = Color::rgba(st.text[0], st.text[1], st.text[2], a);
            let dark = Color::rgba(st.outline[0], st.outline[1], st.outline[2], a);
            let (bx, by) = self.box_pos(b, screen_w);
            // jitter text sideways during glitches
            let jx = if glitch > 0.2 { (hash(self.t * 40.0 + b.seed) - 0.5) * 10.0 * glitch } else { 0.0 };

            let x0 = self.column_x(screen_w);
            let body_x = if on_right(b.speaker) { x0 } else { bx + self.m.b + self.m.gap };
            let texts = [(&b.label, bx + jx, b.y), (&b.body, body_x + jx, by + self.m.b * 0.04)];
            for (buf, left, top) in texts {
                for (dx, dy) in ring {
                    out.push(TextArea {
                        buffer: buf,
                        left: left + dx,
                        top: top + dy,
                        scale: 1.0,
                        bounds,
                        default_color: dark,
                        custom_glyphs: &[],
                    });
                }
                out.push(TextArea { buffer: buf, left, top, scale: 1.0, bounds, default_color: fill, custom_glyphs: &[] });
            }
            out.push(TextArea {
                buffer: &self.call,
                left: bx + (self.m.b - self.call_w) * 0.5 + jx,
                top: by + (self.m.b - self.m.call.line_height) * 0.5,
                scale: 1.0,
                bounds,
                default_color: Color::rgba(255, 255, 255, a),
                custom_glyphs: &[],
            });
        }
    }

    fn wants_surface(&self) -> bool {
        self.active || !self.bubbles.is_empty()
    }
}

// ---------------------------------------------------------------------------------------------
// GPU

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Globals {
    screen: [f32; 2],
    time: f32,
    _pad: f32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct GpuInst {
    rect: [f32; 4],
    fill: [f32; 4],
    ring: [f32; 4],
    accent: [f32; 4],
    misc: [f32; 4],
}

struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    format: wgpu::TextureFormat,
    alpha_mode: wgpu::CompositeAlphaMode,
    present_mode: wgpu::PresentMode,
    pipeline: wgpu::RenderPipeline,
    globals: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    instances: wgpu::Buffer,
    capacity: usize,
    swash: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text: TextRenderer,
}

impl Gpu {
    fn new(instance: &wgpu::Instance, surface: &wgpu::Surface<'static>) -> Result<Self> {
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(surface),
            ..Default::default()
        }))
        .context("no GPU adapter")?;
        log::info!("rendering on {}", adapter.get_info().name);
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .context("requesting GPU device")?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps.formats.iter().copied().find(|f| f.is_srgb()).unwrap_or(caps.formats[0]);
        let alpha_mode = [wgpu::CompositeAlphaMode::PreMultiplied, wgpu::CompositeAlphaMode::Inherit]
            .into_iter()
            .find(|m| caps.alpha_modes.contains(m))
            .unwrap_or(caps.alpha_modes[0]);
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        };
        log::debug!("surface format {format:?}, alpha {alpha_mode:?}, present {present_mode:?}");

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("callbox"),
            source: wgpu::ShaderSource::Wgsl(include_str!("callbox.wgsl").into()),
        });
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
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
        let globals = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &bgl,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let attributes = wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Float32x4, 4 => Float32x4];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("callbox"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<GpuInst>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &attributes,
                })],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let capacity = 16;
        let instances = Self::instance_buffer(&device, capacity);

        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, format);
        let text = TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);

        Ok(Self {
            device,
            queue,
            format,
            alpha_mode,
            present_mode,
            pipeline,
            globals,
            bind_group,
            instances,
            capacity,
            swash: SwashCache::new(),
            viewport,
            atlas,
            text,
        })
    }

    fn instance_buffer(device: &wgpu::Device, n: usize) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("callbox instances"),
            size: (n * std::mem::size_of::<GpuInst>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    fn surface_config(&self, w: u32, h: u32) -> wgpu::SurfaceConfiguration {
        wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: self.format,
            width: w,
            height: h,
            present_mode: self.present_mode,
            alpha_mode: self.alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
            color_space: wgpu::SurfaceColorSpace::Auto,
        }
    }
}


impl Gpu {
    /// Draws the scene into the surface's next texture and submits it. The caller presents the
    /// returned frame; `None` means there was no texture this time (the surface is reconfigured
    /// when that helps) and the caller should try again on the next frame.
    fn render(
        &mut self,
        scene: &Scene,
        fonts: &mut FontSystem,
        surface: &wgpu::Surface<'static>,
        config: &wgpu::SurfaceConfiguration,
    ) -> Option<wgpu::SurfaceTexture> {
        let (w, h) = (config.width as f32, config.height as f32);
        let linear = self.format.is_srgb();
        let insts: Vec<GpuInst> = scene
            .boxes(w)
            .into_iter()
            .map(|b| {
                let st = style(b.speaker);
                let [fr, fg, fb] = color3(st.fill, linear);
                let [rr, rg, rb] = color3(st.ring, linear);
                let [ar, ag, ab] = color3(st.accent, linear);
                GpuInst {
                    rect: [b.x, b.y, scene.m.b, b.alpha],
                    fill: [fr, fg, fb, st.fill_alpha],
                    ring: [rr, rg, rb, b.phase],
                    accent: [ar, ag, ab, b.glitch],
                    misc: [b.seed, b.scale, 0.0, 0.0],
                }
            })
            .collect();
        if insts.len() > self.capacity {
            self.capacity = insts.len().next_power_of_two();
            self.instances = Self::instance_buffer(&self.device, self.capacity);
        }
        self.queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(&insts));
        self.queue.write_buffer(
            &self.globals,
            0,
            bytemuck::bytes_of(&Globals { screen: [w, h], time: scene.t, _pad: 0.0 }),
        );

        self.viewport.update(&self.queue, Resolution { width: config.width, height: config.height });
        let bounds = TextBounds { left: 0, top: 0, right: config.width as i32, bottom: config.height as i32 };
        let mut areas = Vec::new();
        scene.text_areas(w, bounds, &mut areas);
        if let Err(e) = self.text.prepare(
            &self.device,
            &self.queue,
            fonts,
            &mut self.atlas,
            &self.viewport,
            areas,
            &mut self.swash,
        ) {
            log::error!("text prepare: {e}");
        }

        let frame = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                surface.configure(&self.device, config);
                return None;
            }
            other => {
                log::warn!("no surface texture: {other:?}");
                return None;
            }
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if !insts.is_empty() {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, self.instances.slice(..));
                pass.draw(0..4, 0..insts.len() as u32);
            }
            if let Err(e) = self.text.render(&self.atlas, &self.viewport, &mut pass) {
                log::error!("text render: {e}");
            }
        }
        self.queue.submit(Some(encoder.finish()));
        self.atlas.trim();
        Some(frame)
    }
}
