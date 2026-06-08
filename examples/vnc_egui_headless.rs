// Headless egui + wgpu + VNC demo.
//
// Usage:
//   cargo run --example vnc_egui_headless -- [--host HOST] [--port PORT] [--passwd PASS]
//
// Then connect a VNC client to 127.0.0.1:<port>.

use std::env;
use std::io;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use vnc_server::{SharedFrame, VncInputEvent, VncMouseButton, VncServerConfig, start_vnc_server};

const WIDTH: u16 = 960;
const HEIGHT: u16 = 600;

fn main() -> io::Result<()> {
    pollster::block_on(run())
}

async fn run() -> io::Result<()> {
    let opts = ServerCli::parse("vnc_egui_headless", 5903)?;
    if opts.help {
        print_server_help("vnc_egui_headless", 5903);
        return Ok(());
    }

    let frame = SharedFrame::new(WIDTH, HEIGHT);
    let (tx, rx) = mpsc::channel::<VncInputEvent>();
    let input = Arc::new(move |event: VncInputEvent| {
        let _ = tx.send(event);
    });
    let mut config = VncServerConfig::new()
        .with_bind_addr(format!("{}:{}", opts.host, opts.port))
        .with_name("vnc-egui-headless")
        .with_max_clients(4)
        .with_input_callback(input);
    if let Some(password) = opts.passwd {
        config = config.with_password(password);
        println!("VNC password authentication enabled");
    }

    let server = start_vnc_server(Arc::clone(&frame), config)?;
    server.set_clipboard_text("hello from headless egui");
    println!(
        "Headless egui VNC demo listening on {}:{}",
        opts.host, opts.port
    );

    let mut gpu = HeadlessGpu::new(WIDTH as u32, HEIGHT as u32).await?;
    let ctx = egui::Context::default();
    ctx.set_visuals(egui::Visuals::dark());

    let mut app = DemoApp::default();
    let mut input_state = InputState::default();
    let start = Instant::now();
    let mut pixels = Vec::new();
    loop {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            input_state.apply_vnc_event(event, &mut events);
        }

        let raw_input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(WIDTH as f32, HEIGHT as f32),
            )),
            time: Some(start.elapsed().as_secs_f64()),
            predicted_dt: 1.0 / 30.0,
            events,
            modifiers: input_state.modifiers,
            focused: true,
            ..Default::default()
        };

        let ctx_for_ui = ctx.clone();
        let full_output = ctx.run_ui(raw_input, |_| app.ui(&ctx_for_ui, &input_state));
        gpu.render_egui(&ctx, full_output, &mut pixels)?;
        frame.publish(&pixels);
        thread::sleep(Duration::from_millis(33));
    }
}

struct ServerCli {
    host: String,
    port: u16,
    passwd: Option<String>,
    help: bool,
}

impl ServerCli {
    fn parse(example: &str, default_port: u16) -> io::Result<Self> {
        let mut out = Self {
            host: "127.0.0.1".to_string(),
            port: default_port,
            passwd: None,
            help: false,
        };
        let mut positional = Vec::new();
        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => out.help = true,
                "--host" => out.host = args.next().ok_or_else(|| missing_value("--host"))?,
                "--port" => {
                    out.port = parse_port(&args.next().ok_or_else(|| missing_value("--port"))?)?;
                }
                "--passwd" | "--password" => {
                    out.passwd = Some(args.next().ok_or_else(|| missing_value("--passwd"))?);
                }
                _ if arg.starts_with("--host=") => {
                    out.host = arg["--host=".len()..].to_string();
                }
                _ if arg.starts_with("--port=") => {
                    out.port = parse_port(&arg["--port=".len()..])?;
                }
                _ if arg.starts_with("--passwd=") => {
                    out.passwd = Some(arg["--passwd=".len()..].to_string());
                }
                _ if arg.starts_with("--password=") => {
                    out.passwd = Some(arg["--password=".len()..].to_string());
                }
                _ if arg.starts_with('-') => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown option for {example}: {arg}"),
                    ));
                }
                _ => positional.push(arg),
            }
        }
        if let Some(port) = positional.first() {
            out.port = parse_port(port)?;
        }
        if let Some(passwd) = positional.get(1) {
            out.passwd = Some(passwd.clone());
        }
        Ok(out)
    }
}

fn parse_port(value: &str) -> io::Result<u16> {
    value
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "port must be a number"))
}

fn missing_value(name: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{name} requires a value"),
    )
}

fn print_server_help(example: &str, default_port: u16) {
    println!(
        "Usage: cargo run --example {example} -- [OPTIONS]\n\nOptions:\n  --host HOST        Bind host/address, e.g. 127.0.0.1 or 0.0.0.0 (default: 127.0.0.1)\n  --port PORT        Bind port (default: {default_port})\n  --passwd PASS      Enable VNC password authentication\n  --password PASS    Alias for --passwd\n  -h, --help         Show this help\n\nBackward-compatible positional form:\n  cargo run --example {example} -- [port] [password]\n"
    );
}

#[derive(Default)]
struct DemoApp {
    text: String,
    clicks: u64,
    slider: f32,
    check: bool,
}

impl DemoApp {
    fn ui(&mut self, ctx: &egui::Context, input: &InputState) {
        egui::Area::new(egui::Id::new("root"))
            .fixed_pos(egui::pos2(0.0, 0.0))
            .show(ctx, |ui| {
                egui::Frame::default()
                    .fill(egui::Color32::from_rgb(16, 18, 22))
                    .inner_margin(egui::Margin::same(18))
                    .show(ui, |ui| {
                        ui.set_min_size(egui::vec2(WIDTH as f32 - 36.0, HEIGHT as f32 - 36.0));
                        ui.heading("Headless egui over VNC");
                        ui.label("This UI is rendered by egui/wgpu without a native window.");
                        ui.separator();

                        ui.horizontal(|ui| {
                            ui.label(format!(
                                "pointer: {:.0}, {:.0}",
                                input.pointer_pos.x, input.pointer_pos.y
                            ));
                            ui.label(format!("buttons: 0b{:08b}", input.button_mask));
                            ui.label(format!("wheel: {}", input.wheel_ticks));
                        });

                        ui.horizontal(|ui| {
                            if ui.button("Click counter").clicked() {
                                self.clicks += 1;
                            }
                            ui.label(format!("clicked {count} times", count = self.clicks));
                        });

                        ui.checkbox(&mut self.check, "egui checkbox");
                        ui.add(egui::Slider::new(&mut self.slider, 0.0..=100.0).text("slider"));
                        ui.text_edit_singleline(&mut self.text);

                        ui.separator();
                        egui::ScrollArea::vertical()
                            .max_height(220.0)
                            .show(ui, |ui| {
                                for row in 0..24 {
                                    ui.label(format!(
                                        "scroll row {row:02}  slider={:.1}  text='{}'",
                                        self.slider, self.text
                                    ));
                                }
                            });
                    });
            });
    }
}

#[derive(Clone, Copy)]
struct InputState {
    pointer_pos: egui::Pos2,
    button_mask: u8,
    modifiers: egui::Modifiers,
    wheel_ticks: i64,
}

impl Default for InputState {
    fn default() -> Self {
        Self {
            pointer_pos: egui::pos2(WIDTH as f32 * 0.5, HEIGHT as f32 * 0.5),
            button_mask: 0,
            modifiers: egui::Modifiers::default(),
            wheel_ticks: 0,
        }
    }
}

impl InputState {
    fn apply_vnc_event(&mut self, event: VncInputEvent, out: &mut Vec<egui::Event>) {
        match event {
            VncInputEvent::Pointer { button_mask, x, y } => {
                self.pointer_pos = egui::pos2(x as f32, y as f32);
                out.push(egui::Event::PointerMoved(self.pointer_pos));

                for (vnc_button, egui_button) in [
                    (VncMouseButton::Left, egui::PointerButton::Primary),
                    (VncMouseButton::Middle, egui::PointerButton::Middle),
                    (VncMouseButton::Right, egui::PointerButton::Secondary),
                ] {
                    let was = (self.button_mask & vnc_button.mask()) != 0;
                    let now = (button_mask & vnc_button.mask()) != 0;
                    if was != now {
                        out.push(egui::Event::PointerButton {
                            pos: self.pointer_pos,
                            button: egui_button,
                            pressed: now,
                            modifiers: self.modifiers,
                        });
                    }
                }

                let wheel =
                    VncInputEvent::Pointer { button_mask, x, y }.wheel_delta(self.button_mask);
                if wheel != 0 {
                    let delta = 72.0 * wheel as f32;
                    self.wheel_ticks += wheel as i64;
                    out.push(egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        delta: egui::vec2(0.0, delta),
                        phase: egui::TouchPhase::Move,
                        modifiers: self.modifiers,
                    });
                }
                self.button_mask = button_mask;
            }
            VncInputEvent::Key { down, key } => {
                if update_modifier(&mut self.modifiers, key, down) {
                    return;
                }
                if let Some(mapped) = map_key(key) {
                    out.push(egui::Event::Key {
                        key: mapped,
                        physical_key: None,
                        pressed: down,
                        repeat: false,
                        modifiers: self.modifiers,
                    });
                }
                if down {
                    if let Some(ch) = (VncInputEvent::Key { down, key }).text() {
                        out.push(egui::Event::Text(ch.to_string()));
                    }
                }
            }
            VncInputEvent::ClientCutText(bytes) => {
                if let Ok(text) = String::from_utf8(bytes) {
                    out.push(egui::Event::Paste(text));
                }
            }
        }
    }
}

fn update_modifier(mods: &mut egui::Modifiers, key: u32, down: bool) -> bool {
    match key {
        0xffe1 | 0xffe2 => mods.shift = down,
        0xffe3 | 0xffe4 => mods.ctrl = down,
        0xffe7 | 0xffe8 => mods.alt = down,
        0xffeb | 0xffec => {
            mods.mac_cmd = down;
            mods.command = down;
        }
        _ => return false,
    }
    true
}

fn map_key(key: u32) -> Option<egui::Key> {
    Some(match key {
        0xff08 => egui::Key::Backspace,
        0xff09 => egui::Key::Tab,
        0xff0d | 0xff8d => egui::Key::Enter,
        0xff1b => egui::Key::Escape,
        0xff51 => egui::Key::ArrowLeft,
        0xff52 => egui::Key::ArrowUp,
        0xff53 => egui::Key::ArrowRight,
        0xff54 => egui::Key::ArrowDown,
        0x20 => egui::Key::Space,
        0x30 => egui::Key::Num0,
        0x31 => egui::Key::Num1,
        0x32 => egui::Key::Num2,
        0x33 => egui::Key::Num3,
        0x34 => egui::Key::Num4,
        0x35 => egui::Key::Num5,
        0x36 => egui::Key::Num6,
        0x37 => egui::Key::Num7,
        0x38 => egui::Key::Num8,
        0x39 => egui::Key::Num9,
        0x41 | 0x61 => egui::Key::A,
        0x42 | 0x62 => egui::Key::B,
        0x43 | 0x63 => egui::Key::C,
        0x44 | 0x64 => egui::Key::D,
        0x45 | 0x65 => egui::Key::E,
        0x46 | 0x66 => egui::Key::F,
        0x47 | 0x67 => egui::Key::G,
        0x48 | 0x68 => egui::Key::H,
        0x49 | 0x69 => egui::Key::I,
        0x4a | 0x6a => egui::Key::J,
        0x4b | 0x6b => egui::Key::K,
        0x4c | 0x6c => egui::Key::L,
        0x4d | 0x6d => egui::Key::M,
        0x4e | 0x6e => egui::Key::N,
        0x4f | 0x6f => egui::Key::O,
        0x50 | 0x70 => egui::Key::P,
        0x51 | 0x71 => egui::Key::Q,
        0x52 | 0x72 => egui::Key::R,
        0x53 | 0x73 => egui::Key::S,
        0x54 | 0x74 => egui::Key::T,
        0x55 | 0x75 => egui::Key::U,
        0x56 | 0x76 => egui::Key::V,
        0x57 | 0x77 => egui::Key::W,
        0x58 | 0x78 => egui::Key::X,
        0x59 | 0x79 => egui::Key::Y,
        0x5a | 0x7a => egui::Key::Z,
        _ => return None,
    })
}

struct HeadlessGpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    target: HeadlessTarget,
    renderer: egui_wgpu::Renderer,
}

impl HeadlessGpu {
    async fn new(width: u32, height: u32) -> io::Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                force_fallback_adapter: false,
            })
            .await
            .map_err(io_other)?;
        let info = adapter.get_info();
        println!("GPU: {} ({:?})", info.name, info.backend);
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("headless egui device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                memory_hints: wgpu::MemoryHints::Performance,
                trace: wgpu::Trace::Off,
            })
            .await
            .map_err(io_other)?;
        let format = wgpu::TextureFormat::Bgra8Unorm;
        let target = HeadlessTarget::new(&device, width, height, format);
        let renderer =
            egui_wgpu::Renderer::new(&device, format, egui_wgpu::RendererOptions::default());
        Ok(Self {
            device,
            queue,
            target,
            renderer,
        })
    }

    fn render_egui(
        &mut self,
        ctx: &egui::Context,
        full_output: egui::FullOutput,
        out: &mut Vec<u8>,
    ) -> io::Result<()> {
        let pixels_per_point = ctx.pixels_per_point();
        let paint_jobs = ctx.tessellate(full_output.shapes, pixels_per_point);
        let screen_descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.target.width, self.target.height],
            pixels_per_point,
        };
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("headless egui encoder"),
            });

        for (texture_id, image_delta) in &full_output.textures_delta.set {
            self.renderer
                .update_texture(&self.device, &self.queue, *texture_id, image_delta);
        }
        self.renderer.update_buffers(
            &self.device,
            &self.queue,
            &mut encoder,
            &paint_jobs,
            &screen_descriptor,
        );
        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("headless egui pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &self.target.view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.035,
                                g: 0.039,
                                b: 0.047,
                                a: 1.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                .forget_lifetime();
            self.renderer
                .render(&mut pass, &paint_jobs, &screen_descriptor);
        }
        for texture_id in &full_output.textures_delta.free {
            self.renderer.free_texture(texture_id);
        }

        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.target.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.target.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(self.target.padded_bytes_per_row),
                    rows_per_image: Some(self.target.height),
                },
            },
            wgpu::Extent3d {
                width: self.target.width,
                height: self.target.height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));
        self.readback(out)?;
        Ok(())
    }

    fn readback(&self, out: &mut Vec<u8>) -> io::Result<()> {
        let width = self.target.width as usize;
        let height = self.target.height as usize;
        let padded = self.target.padded_bytes_per_row as usize;
        let slice = self.target.readback.slice(..);
        let (tx, rx) = mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |res| {
            let _ = tx.send(res);
        });
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        rx.recv().map_err(io_other)?.map_err(io_other)?;
        {
            let data = slice.get_mapped_range();
            out.clear();
            out.reserve(width * height * 4);
            for row in 0..height {
                let start = row * padded;
                out.extend_from_slice(&data[start..start + width * 4]);
            }
        }
        self.target.readback.unmap();
        Ok(())
    }
}

struct HeadlessTarget {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    readback: wgpu::Buffer,
    padded_bytes_per_row: u32,
    width: u32,
    height: u32,
}

impl HeadlessTarget {
    fn new(device: &wgpu::Device, width: u32, height: u32, format: wgpu::TextureFormat) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("headless egui target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let unpadded = width * 4;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_bytes_per_row = unpadded.div_ceil(align) * align;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("headless egui readback"),
            size: (padded_bytes_per_row * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Self {
            texture,
            view,
            readback,
            padded_bytes_per_row,
            width,
            height,
        }
    }
}

fn io_other(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e.to_string())
}
