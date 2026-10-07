//! C ABI for the SwiftUI host: one `T1View` per NSView. Every call happens on the main thread.
//! Coordinates are points, top-left origin; `scale` is the backing scale factor.
use eframe::egui::{self, Event, Key, Modifiers, PointerButton, Pos2, RawInput, Rect, Vec2};
use std::ffi::{c_char, c_void, CStr};
use std::ptr::NonNull;
use std::time::{Duration, Instant};
use t1_ui::App;

pub struct T1View {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    renderer: egui_wgpu::Renderer,
    ctx: egui::Context,
    app: App,
    input: RawInput,
    size_pts: Vec2,
    scale: f32,
    pointer: Pos2,
    /// render no earlier than this unless input arrived (egui's repaint request)
    next_frame: Instant,
    dirty: bool,
    start: Instant,
    cursor: egui::CursorIcon,
    copied: Option<String>,
    /// T1_SHOT=<png> [T1_SHOT_AFTER=<s>]: read one rendered frame back, save it, exit
    shot: Option<(String, f64)>,
    /// T1_DUMP_STATE=<json path>: write one state snapshot after 15 s (contract sample)
    dump: Option<String>,
}

fn mods(m: u32) -> Modifiers {
    Modifiers { shift: m & 1 != 0, ctrl: m & 2 != 0, alt: m & 4 != 0, mac_cmd: m & 8 != 0, command: m & 8 != 0 }
}

impl T1View {
    fn push(&mut self, e: Event) { self.input.events.push(e); self.dirty = true; }

    fn resize(&mut self, w: f32, h: f32, scale: f32) {
        self.size_pts = Vec2::new(w.max(1.0), h.max(1.0));
        self.scale = scale.max(1.0);
        self.config.width = (self.size_pts.x * self.scale).round() as u32;
        self.config.height = (self.size_pts.y * self.scale).round() as u32;
        self.surface.configure(&self.device, &self.config);
        self.dirty = true;
    }

    fn render(&mut self) -> bool {
        let now = Instant::now();
        if self.dump.is_some() && self.start.elapsed().as_secs() > 15 {
            let p = self.dump.take().unwrap();
            let _ = std::fs::write(&p, self.app.state_json());
        }
        if !self.dirty && now < self.next_frame { return false; }
        self.dirty = false;
        let mut input = std::mem::take(&mut self.input);
        input.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, self.size_pts));
        input.time = Some(self.start.elapsed().as_secs_f64());
        input.focused = true;
        if let Some(vp) = input.viewports.get_mut(&egui::ViewportId::ROOT) { vp.native_pixels_per_point = Some(self.scale); }
        let app = &mut self.app;
        let out = self.ctx.run_ui(input, |ui| app.frame(ui));
        self.cursor = out.platform_output.cursor_icon;
        for c in &out.platform_output.commands { if let egui::OutputCommand::CopyText(s) = c { self.copied = Some(s.clone()); } }
        let delay = out.viewport_output.get(&egui::ViewportId::ROOT).map(|v| v.repaint_delay).unwrap_or(Duration::from_secs(1));
        self.next_frame = now + delay.min(Duration::from_secs(1));

        // upload texture changes before anything can bail out: egui sends each texture once, so a
        // skipped upload leaves later partial updates pointing at a texture that never existed
        for (id, deltas) in &out.textures_delta.set { for d in deltas { self.renderer.update_texture(&self.device, &self.queue, *id, d); } }
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            _ => {
                // nothing drawn this frame, so textures egui dropped can go now
                for id in &out.textures_delta.free { self.renderer.free_texture(id); }
                self.surface.configure(&self.device, &self.config);
                self.dirty = true;
                return false;
            }
        };
        let jobs = self.ctx.tessellate(out.shapes, out.pixels_per_point);
        let sd = egui_wgpu::ScreenDescriptor { size_in_pixels: [self.config.width, self.config.height], pixels_per_point: out.pixels_per_point };
        let mut enc = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("t1") });
        let cmds = self.renderer.update_buffers(&self.device, &self.queue, &mut enc, &jobs, &sd);
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        {
            // transparent: the native window background shows through the chart and book
            let pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("egui"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view, resolve_target: None, depth_slice: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
                })],
                ..Default::default()
            });
            self.renderer.render(&mut pass.forget_lifetime(), &jobs, &sd);
        }
        let shot = self.shot.as_ref().filter(|(_, after)| self.start.elapsed().as_secs_f64() > *after).map(|(p, _)| p.clone());
        let readback = shot.as_ref().map(|_| {
            // rows padded to 256 bytes for the copy
            let (w, h) = (self.config.width, self.config.height);
            let row = (w * 4).div_ceil(256) * 256;
            let buf = self.device.create_buffer(&wgpu::BufferDescriptor { label: Some("shot"), size: (row * h) as u64, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
            enc.copy_texture_to_buffer(frame.texture.as_image_copy(), wgpu::TexelCopyBufferInfo { buffer: &buf, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(h) } }, wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 });
            (buf, row, w, h)
        });
        self.queue.submit(cmds.into_iter().chain([enc.finish()]));
        if let (Some(path), Some((buf, row, w, h))) = (shot, readback) { save_png(&self.device, &buf, row, w, h, self.config.format, &path); std::process::exit(0); }
        self.queue.present(frame);
        for id in &out.textures_delta.free { self.renderer.free_texture(id); }
        true
    }
}

fn save_png(device: &wgpu::Device, buf: &wgpu::Buffer, row: u32, w: u32, h: u32, format: wgpu::TextureFormat, path: &str) {
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    let _ = device.poll(wgpu::PollType::wait_indefinitely());
    let Ok(data) = buf.slice(..).get_mapped_range() else { return };
    let bgra = matches!(format, wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb);
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        let r = &data[(y * row) as usize..(y * row + w * 4) as usize];
        for c in r.chunks(4) { if bgra { px.extend_from_slice(&[c[2], c[1], c[0], 255]); } else { px.extend_from_slice(&[c[0], c[1], c[2], 255]); } }
    }
    let Ok(f) = std::fs::File::create(path) else { return };
    let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    if let Ok(mut wr) = enc.write_header() { let _ = wr.write_image_data(&px); }
}

/// Create a view rendering into `ns_view` (an NSView*; wgpu installs its CAMetalLayer).
/// Returns null when no Metal device / surface could be created.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn t1_view_new(ns_view: *mut c_void, w: f32, h: f32, scale: f32) -> *mut T1View {
    install_panic_log();
    let Some(nv) = NonNull::new(ns_view) else { return std::ptr::null_mut() };
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor { backends: wgpu::Backends::METAL, ..wgpu::InstanceDescriptor::new_without_display_handle() });
    let target = wgpu::SurfaceTargetUnsafe::RawHandle {
        raw_display_handle: Some(raw_window_handle::RawDisplayHandle::AppKit(raw_window_handle::AppKitDisplayHandle::new())),
        raw_window_handle: raw_window_handle::RawWindowHandle::AppKit(raw_window_handle::AppKitWindowHandle::new(nv)),
    };
    // SAFETY: the host keeps the NSView alive until t1_view_free
    let Ok(surface) = (unsafe { instance.create_surface_unsafe(target) }) else { return std::ptr::null_mut() };
    let Ok(adapter) = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions { compatible_surface: Some(&surface), power_preference: wgpu::PowerPreference::HighPerformance, ..Default::default() })) else { return std::ptr::null_mut() };
    let Ok((device, queue)) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())) else { return std::ptr::null_mut() };
    let caps = surface.get_capabilities(&adapter);
    // egui expects a non-sRGB-converting target for its own gamma handling
    let format = caps.formats.iter().copied().find(|f| !f.is_srgb()).unwrap_or(caps.formats[0]);
    let Some(base) = surface.get_default_config(&adapter, 1, 1) else { return std::ptr::null_mut() };
    let config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC, format, present_mode: wgpu::PresentMode::AutoVsync, desired_maximum_frame_latency: 2,
        alpha_mode: caps.alpha_modes.iter().copied().find(|m| *m != wgpu::CompositeAlphaMode::Opaque).unwrap_or(caps.alpha_modes[0]), view_formats: vec![format], ..base
    };
    let renderer = egui_wgpu::Renderer::new(&device, format, egui_wgpu::RendererOptions::default());
    let ctx = egui::Context::default();
    let app = App::with_ctx(&ctx, true);
    let mut v = Box::new(T1View {
        surface, device, queue, config, renderer, ctx, app, input: RawInput::default(), size_pts: Vec2::new(w, h), scale,
        pointer: Pos2::ZERO, next_frame: Instant::now(), dirty: true, start: Instant::now(), cursor: egui::CursorIcon::Default, copied: None,
        dump: std::env::var("T1_DUMP_STATE").ok(),
        shot: std::env::var("T1_SHOT").ok().map(|p| (p, std::env::var("T1_SHOT_AFTER").ok().and_then(|s| s.parse().ok()).unwrap_or(20.0))),
    });
    v.resize(w, h, scale);
    Box::into_raw(v)
}

macro_rules! view { ($p:expr) => { match unsafe { $p.as_mut() } { Some(v) => v, None => return Default::default() } } }

#[unsafe(no_mangle)]
pub unsafe extern "C" fn t1_view_free(v: *mut T1View) { if !v.is_null() { drop(unsafe { Box::from_raw(v) }); } }

#[unsafe(no_mangle)]
pub unsafe extern "C" fn t1_view_resize(v: *mut T1View, w: f32, h: f32, scale: f32) { view!(v).resize(w, h, scale) }

/// Draws a frame when input arrived or egui asked for one; returns 1 when it drew.
/// Call from the display link every vsync: idle frames cost one comparison.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn t1_view_render(v: *mut T1View) -> i32 {
    let v = view!(v);
    // a panic must not cross the C boundary (that aborts the app): drop the frame, keep running
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| v.render())) {
        Ok(r) => r as i32,
        Err(_) => { v.dirty = true; 0 }
    }
}

/// Panics go to ~/Library/Logs/TerminalOne/panic.log (with location and backtrace): the app is
/// launched from Finder, so stderr is gone.
fn install_panic_log() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if let Some(home) = std::env::var_os("HOME") {
                let dir = std::path::Path::new(&home).join("Library/Logs/TerminalOne");
                let _ = std::fs::create_dir_all(&dir);
                let bt = std::backtrace::Backtrace::force_capture();
                let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                let line = format!("{ts} {info}\n{bt}\n");
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("panic.log")) { let _ = f.write_all(line.as_bytes()); }
            }
            prev(info);
        }));
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn t1_view_pointer_move(v: *mut T1View, x: f32, y: f32) {
    let v = view!(v);
    v.pointer = Pos2::new(x, y);
    v.push(Event::PointerMoved(v.pointer));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn t1_view_pointer_leave(v: *mut T1View) { view!(v).push(Event::PointerGone) }

/// button: 0 left, 1 right, 2 middle; mods: 1 shift, 2 ctrl, 4 alt, 8 cmd
#[unsafe(no_mangle)]
pub unsafe extern "C" fn t1_view_pointer_button(v: *mut T1View, x: f32, y: f32, button: i32, pressed: i32, m: u32) {
    let v = view!(v);
    let button = match button { 1 => PointerButton::Secondary, 2 => PointerButton::Middle, _ => PointerButton::Primary };
    v.pointer = Pos2::new(x, y);
    v.push(Event::PointerButton { pos: v.pointer, button, pressed: pressed != 0, modifiers: mods(m) });
}

/// Scroll in points (precise trackpad deltas; the host converts line deltas).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn t1_view_scroll(v: *mut T1View, dx: f32, dy: f32, m: u32) {
    let v = view!(v);
    v.push(Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: Vec2::new(dx, dy), phase: egui::TouchPhase::Move, modifiers: mods(m) });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn t1_view_zoom(v: *mut T1View, factor: f32) { view!(v).push(Event::Zoom(factor)) }

/// `name` is an egui key name ("ArrowLeft", "Enter", "A", ...); unknown names are ignored.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn t1_view_key(v: *mut T1View, name: *const c_char, pressed: i32, m: u32) {
    let v = view!(v);
    let Some(key) = (unsafe { name.as_ref() }).and_then(|_| unsafe { CStr::from_ptr(name) }.to_str().ok()).and_then(Key::from_name) else { return };
    // the host's Cmd+C / Cmd+X / Cmd+V arrive as keys; egui wants the semantic events
    if pressed != 0 && m & 8 != 0 {
        match key { Key::C => return v.push(Event::Copy), Key::X => return v.push(Event::Cut), _ => {} }
    }
    v.push(Event::Key { key, physical_key: None, pressed: pressed != 0, repeat: false, modifiers: mods(m) });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn t1_view_text(v: *mut T1View, text: *const c_char) {
    let v = view!(v);
    if text.is_null() { return; }
    if let Ok(s) = unsafe { CStr::from_ptr(text) }.to_str() { if !s.is_empty() { v.push(Event::Text(s.to_string())); } }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn t1_view_paste(v: *mut T1View, text: *const c_char) {
    let v = view!(v);
    if text.is_null() { return; }
    if let Ok(s) = unsafe { CStr::from_ptr(text) }.to_str() { v.push(Event::Paste(s.to_string())); }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn t1_view_focus(v: *mut T1View, focused: i32) { view!(v).push(Event::WindowFocused(focused != 0)) }

/// Cursor egui wants: 0 arrow, 1 pointing hand, 2 text, 3 resize horizontal, 4 resize vertical,
/// 5 crosshair, 6 grab, 7 grabbing, 8 not allowed, 9 resize diagonal NW-SE, 10 NE-SW.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn t1_view_cursor(v: *mut T1View) -> i32 {
    use egui::CursorIcon as C;
    match view!(v).cursor {
        C::PointingHand => 1, C::Text => 2, C::ResizeHorizontal | C::ResizeColumn | C::ResizeEast | C::ResizeWest => 3,
        C::ResizeVertical | C::ResizeRow | C::ResizeNorth | C::ResizeSouth => 4, C::Crosshair => 5, C::Grab => 6, C::Grabbing => 7,
        C::NotAllowed | C::NoDrop => 8, C::ResizeNwSe | C::ResizeNorthWest | C::ResizeSouthEast => 9, C::ResizeNeSw | C::ResizeNorthEast | C::ResizeSouthWest => 10,
        _ => 0,
    }
}

/// Text egui copied since the last call (caller frees with t1_free), or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn t1_view_take_copied(v: *mut T1View) -> *mut c_char {
    let v = view!(v);
    match v.copied.take().and_then(|s| std::ffi::CString::new(s).ok()) { Some(c) => c.into_raw(), None => std::ptr::null_mut() }
}

/// Native panels' state as JSON (caller frees with t1_free).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn t1_state(v: *mut T1View) -> *mut c_char {
    let v = view!(v);
    let Ok(js) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| v.app.state_json())) else { return std::ptr::null_mut() };
    std::ffi::CString::new(js).map(|c| c.into_raw()).unwrap_or(std::ptr::null_mut())
}

/// One action ({"op": ...} JSON); returns {"ok": bool, ...} (caller frees with t1_free).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn t1_call(v: *mut T1View, req: *const c_char) -> *mut c_char {
    let v = view!(v);
    if req.is_null() { return std::ptr::null_mut(); }
    let req = unsafe { CStr::from_ptr(req) }.to_string_lossy();
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| v.app.call_json(&v.ctx, &req)))
        .unwrap_or_else(|_| r#"{"ok":false,"error":"internal error (see ~/Library/Logs/TerminalOne/panic.log)"}"#.to_string());
    // read-only queries polled at display rate must not force a chart redraw each time
    let read_only = ["\"book_levels\"", "\"venue_share\"", "\"tickers\""].iter().any(|op| req.contains(op));
    if !read_only { v.dirty = true; }
    std::ffi::CString::new(out).map(|c| c.into_raw()).unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn t1_free(s: *mut c_char) { if !s.is_null() { drop(unsafe { std::ffi::CString::from_raw(s) }); } }
