// VNC input demo server.
//
// Usage:
//   cargo run --example vnc_input_demo -- [--host HOST] [--port PORT] [--passwd PASS] [--low-bandwidth] [--jpeg-quality N]
//
// Then connect a VNC client to 127.0.0.1:<port>.
// Pointer movement, mouse buttons, clipboard text, and keyboard events are
// printed to the console. Pointer and mouse button state are also rendered into
// the streamed framebuffer.

use std::collections::HashMap;
use std::env;
use std::io;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use vnc_server::{
    SharedFrame, VncClientEvent, VncCursor, VncInputEvent, VncMouseButton, VncServerConfig,
    start_vnc_server,
};

const WIDTH: u16 = 800;
const HEIGHT: u16 = 480;

#[derive(Clone)]
struct DemoState {
    pointer_x: u16,
    pointer_y: u16,
    button_mask: u8,
    active_client_id: Option<u64>,
    client_button_masks: HashMap<u64, u8>,
    last_key: Option<u32>,
    key_down: bool,
    cut_text_len: usize,
    typed_text: String,
    event_count: u64,
    wheel_up_count: u64,
    wheel_down_count: u64,
    wheel_flash_until: Instant,
    flash_until: Instant,
}

impl DemoState {
    fn new() -> Self {
        Self {
            pointer_x: WIDTH / 2,
            pointer_y: HEIGHT / 2,
            button_mask: 0,
            active_client_id: None,
            client_button_masks: HashMap::new(),
            last_key: None,
            key_down: false,
            cut_text_len: 0,
            typed_text: String::new(),
            event_count: 0,
            wheel_up_count: 0,
            wheel_down_count: 0,
            wheel_flash_until: Instant::now(),
            flash_until: Instant::now(),
        }
    }
}

fn main() -> io::Result<()> {
    let opts = ServerCli::parse("vnc_input_demo", 5902)?;
    if opts.help {
        print_server_help("vnc_input_demo", 5902);
        return Ok(());
    }

    let frame = SharedFrame::new(WIDTH, HEIGHT);
    let (tx, rx) = mpsc::channel::<VncInputEvent>();
    let input = Arc::new(move |event: VncInputEvent| {
        let _ = tx.send(event);
    });
    let client_events = Arc::new(move |event: VncClientEvent| {
        println!("client event: {event:?}");
    });

    let mut config = VncServerConfig::new()
        .with_bind_addr(format!("{}:{}", opts.host, opts.port))
        .with_name("vnc-input-demo")
        .with_max_clients(4)
        .with_client_callback(client_events)
        .with_input_callback(input);
    if let Some(password) = opts.passwd {
        config = config.with_password(password);
        println!("VNC password authentication enabled");
    }
    if opts.low_bandwidth {
        config = config.with_low_bandwidth();
        println!("Low-bandwidth mode enabled: preferred pixel format is RGB565");
    }
    if let Some(quality) = opts.jpeg_quality {
        config = config.with_tight_jpeg_quality(quality);
        println!("Tight JPEG quality set to {quality}");
    }

    let server = start_vnc_server(Arc::clone(&frame), config)?;
    server.set_clipboard_text("hello from vnc-server");

    println!("VNC input demo listening on {}:{}", opts.host, opts.port);
    println!("Move the pointer, click, type keys, or paste clipboard text in your VNC client.");

    let mut state = DemoState::new();
    let mut pixels = vec![0u8; WIDTH as usize * HEIGHT as usize * 4];
    loop {
        while let Ok(event) = rx.try_recv() {
            state.event_count += 1;
            state.flash_until = Instant::now() + Duration::from_millis(120);
            match event {
                event @ VncInputEvent::Pointer {
                    client_id,
                    button_mask,
                    x,
                    y,
                    ..
                } => {
                    state.pointer_x = x.min(WIDTH.saturating_sub(1));
                    state.pointer_y = y.min(HEIGHT.saturating_sub(1));
                    state.active_client_id = Some(client_id);
                    let previous_mask = state
                        .client_button_masks
                        .get(&client_id)
                        .copied()
                        .unwrap_or(0);
                    let wheel = event.wheel_delta(previous_mask);
                    if wheel > 0 {
                        state.wheel_up_count += 1;
                        state.wheel_flash_until = Instant::now() + Duration::from_millis(500);
                    }
                    if wheel < 0 {
                        state.wheel_down_count += 1;
                        state.wheel_flash_until = Instant::now() + Duration::from_millis(500);
                    }
                    state.button_mask = button_mask;
                    state.client_button_masks.insert(client_id, button_mask);
                    println!(
                        "client #{client_id} pointer x={} y={} buttons=0b{:08b} wheel_delta={}",
                        state.pointer_x, state.pointer_y, button_mask, wheel
                    );
                }
                event @ VncInputEvent::Key {
                    client_id,
                    down,
                    key,
                    ..
                } => {
                    state.active_client_id = Some(client_id);
                    state.last_key = Some(key);
                    state.key_down = down;
                    if let Some(ch) = event.text() {
                        state.typed_text.push(ch);
                    } else if down {
                        apply_control_key_to_text(&mut state.typed_text, key);
                    }
                    println!(
                        "client #{client_id} key {} keysym=0x{key:08x}",
                        if down { "down" } else { "up" }
                    );
                }
                VncInputEvent::ClientCutText {
                    client_id, text, ..
                } => {
                    state.active_client_id = Some(client_id);
                    state.cut_text_len = text.len();
                    println!(
                        "client #{client_id} cut text: {} bytes: {}",
                        text.len(),
                        String::from_utf8_lossy(&text)
                    );
                }
            }
        }

        let cursors = server.client_cursors();
        render(&mut pixels, &state, &cursors);
        frame.publish(&pixels);
        thread::sleep(Duration::from_millis(33));
    }
}

struct ServerCli {
    host: String,
    port: u16,
    passwd: Option<String>,
    low_bandwidth: bool,
    jpeg_quality: Option<u8>,
    help: bool,
}

impl ServerCli {
    fn parse(example: &str, default_port: u16) -> io::Result<Self> {
        let mut out = Self {
            host: "127.0.0.1".to_string(),
            port: default_port,
            passwd: None,
            low_bandwidth: false,
            jpeg_quality: None,
            help: false,
        };
        let mut positional = Vec::new();
        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => out.help = true,
                "--low-bandwidth" => out.low_bandwidth = true,
                "--jpeg-quality" => {
                    out.jpeg_quality = Some(parse_jpeg_quality(
                        &args.next().ok_or_else(|| missing_value("--jpeg-quality"))?,
                    )?);
                }
                "--host" => {
                    out.host = args.next().ok_or_else(|| missing_value("--host"))?;
                }
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
                _ if arg.starts_with("--jpeg-quality=") => {
                    out.jpeg_quality = Some(parse_jpeg_quality(&arg["--jpeg-quality=".len()..])?);
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

fn parse_jpeg_quality(value: &str) -> io::Result<u8> {
    let quality: u8 = value.parse().map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "jpeg quality must be a number")
    })?;
    if (1..=100).contains(&quality) {
        Ok(quality)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "jpeg quality must be between 1 and 100",
        ))
    }
}

fn missing_value(name: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{name} requires a value"),
    )
}

fn print_server_help(example: &str, default_port: u16) {
    println!(
        "Usage: cargo run --example {example} -- [OPTIONS]\n\nOptions:\n  --host HOST          Bind host/address, e.g. 127.0.0.1 or 0.0.0.0 (default: 127.0.0.1)\n  --port PORT          Bind port (default: {default_port})\n  --passwd PASS        Enable VNC password authentication\n  --password PASS      Alias for --passwd\n  --low-bandwidth      Prefer 16bpp RGB565 for clients that accept it\n  --jpeg-quality N     Tight/JPEG quality, 1..100 (default: 75)\n  -h, --help           Show this help\n\nBackward-compatible positional form:\n  cargo run --example {example} -- [port] [password]\n"
    );
}

fn render(pixels: &mut [u8], state: &DemoState, cursors: &[VncCursor]) {
    clear(pixels, [24, 28, 34, 255]);
    draw_grid(pixels);

    let flash = Instant::now() < state.flash_until;
    let panel = if flash {
        [46, 84, 112, 255]
    } else {
        [36, 43, 52, 255]
    };
    fill_rect(pixels, 24, 24, 752, 122, panel);
    stroke_rect(pixels, 24, 24, 752, 122, [86, 100, 114, 255]);

    draw_text(pixels, 42, 42, "VNC INPUT DEMO", [236, 241, 245, 255], 3);
    draw_text(
        pixels,
        42,
        82,
        &format!(
            "POINTER {:03},{:03}  BUTTONS {:08b}",
            state.pointer_x, state.pointer_y, state.button_mask
        ),
        [214, 222, 230, 255],
        2,
    );
    draw_text(
        pixels,
        42,
        108,
        &format!(
            "KEY {}  CUTTEXT {}B  EVENTS {}",
            state
                .last_key
                .map(|key| format!("0X{key:08X} {}", if state.key_down { "DOWN" } else { "UP" }))
                .unwrap_or_else(|| "NONE".to_string()),
            state.cut_text_len,
            state.event_count
        ),
        [214, 222, 230, 255],
        2,
    );
    draw_text(
        pixels,
        588,
        42,
        &format!(
            "CLIENTS {} ACTIVE {}",
            cursors.len(),
            state
                .active_client_id
                .map(|id| format!("#{id}"))
                .unwrap_or_else(|| "-".to_string())
        ),
        [185, 198, 210, 255],
        1,
    );
    fill_rect(pixels, 24, 150, 752, 40, [22, 25, 30, 255]);
    stroke_rect(pixels, 24, 150, 752, 40, [86, 100, 114, 255]);
    draw_text(
        pixels,
        42,
        162,
        &format!("TEXT {}", visible_text(&state.typed_text)),
        [236, 241, 245, 255],
        2,
    );

    let base_y = 220;
    let buttons = [
        ("LEFT", VncMouseButton::Left),
        ("MID", VncMouseButton::Middle),
        ("RIGHT", VncMouseButton::Right),
        ("WHL+", VncMouseButton::WheelUp),
        ("WHL-", VncMouseButton::WheelDown),
    ];
    for (idx, (label, button)) in buttons.iter().enumerate() {
        let active = (state.button_mask & button.mask()) != 0;
        let wheel = matches!(button, VncMouseButton::WheelUp | VncMouseButton::WheelDown)
            && Instant::now() < state.wheel_flash_until;
        let x = 48 + idx as i32 * 112;
        fill_rect(
            pixels,
            x,
            base_y,
            88,
            72,
            if active || wheel {
                [53, 178, 112, 255]
            } else {
                [54, 61, 70, 255]
            },
        );
        stroke_rect(pixels, x, base_y, 88, 72, [103, 116, 130, 255]);
        draw_text(
            pixels,
            x + 10,
            base_y + 26,
            label,
            if active || wheel {
                [12, 24, 18, 255]
            } else {
                [204, 211, 218, 255]
            },
            2,
        );
    }
    draw_text(
        pixels,
        48,
        320,
        &format!(
            "WHEEL UP {}  DOWN {}",
            state.wheel_up_count, state.wheel_down_count
        ),
        [214, 222, 230, 255],
        2,
    );

    for cursor in cursors.iter().filter(|cursor| cursor.position_known) {
        let color = cursor_color(cursor.client_id);
        draw_crosshair(pixels, cursor.x as i32, cursor.y as i32, color);
        draw_text(
            pixels,
            cursor.x as i32 + 12,
            cursor.y as i32 + 12,
            &format!("C{}", cursor.client_id),
            color,
            1,
        );
    }
}

fn apply_control_key_to_text(text: &mut String, key: u32) {
    match key {
        0xff08 => {
            text.pop();
        }
        0xff0d | 0xff8d => {
            text.push(' ');
        }
        0xff1b => {
            text.clear();
        }
        _ => {}
    }
    const MAX_CHARS: usize = 56;
    if text.chars().count() > MAX_CHARS {
        let keep_from = text
            .char_indices()
            .nth(text.chars().count() - MAX_CHARS)
            .map(|(idx, _)| idx)
            .unwrap_or(0);
        text.drain(..keep_from);
    }
}

fn visible_text(text: &str) -> String {
    if text.is_empty() {
        "_".to_string()
    } else {
        text.chars()
            .map(|ch| {
                if ch.is_ascii_graphic() || ch == ' ' {
                    ch
                } else {
                    '?'
                }
            })
            .collect()
    }
}

fn clear(pixels: &mut [u8], color: [u8; 4]) {
    for px in pixels.chunks_exact_mut(4) {
        px.copy_from_slice(&color);
    }
}

fn draw_grid(pixels: &mut [u8]) {
    for x in (0..WIDTH as i32).step_by(40) {
        fill_rect(pixels, x, 0, 1, HEIGHT as i32, [34, 39, 47, 255]);
    }
    for y in (0..HEIGHT as i32).step_by(40) {
        fill_rect(pixels, 0, y, WIDTH as i32, 1, [34, 39, 47, 255]);
    }
}

fn draw_crosshair(pixels: &mut [u8], x: i32, y: i32, color: [u8; 4]) {
    fill_rect(pixels, x - 16, y, 33, 2, color);
    fill_rect(pixels, x, y - 16, 2, 33, color);
    stroke_rect(pixels, x - 7, y - 7, 16, 16, color);
}

fn cursor_color(client_id: u64) -> [u8; 4] {
    const COLORS: [[u8; 4]; 6] = [
        [255, 235, 118, 255],
        [86, 214, 190, 255],
        [255, 139, 148, 255],
        [150, 185, 255, 255],
        [184, 231, 111, 255],
        [231, 163, 255, 255],
    ];
    COLORS[(client_id as usize).wrapping_sub(1) % COLORS.len()]
}

fn fill_rect(pixels: &mut [u8], x: i32, y: i32, w: i32, h: i32, color: [u8; 4]) {
    let x0 = x.max(0).min(WIDTH as i32) as usize;
    let y0 = y.max(0).min(HEIGHT as i32) as usize;
    let x1 = (x + w).max(0).min(WIDTH as i32) as usize;
    let y1 = (y + h).max(0).min(HEIGHT as i32) as usize;
    for yy in y0..y1 {
        for xx in x0..x1 {
            let i = (yy * WIDTH as usize + xx) * 4;
            pixels[i..i + 4].copy_from_slice(&color);
        }
    }
}

fn stroke_rect(pixels: &mut [u8], x: i32, y: i32, w: i32, h: i32, color: [u8; 4]) {
    fill_rect(pixels, x, y, w, 1, color);
    fill_rect(pixels, x, y + h - 1, w, 1, color);
    fill_rect(pixels, x, y, 1, h, color);
    fill_rect(pixels, x + w - 1, y, 1, h, color);
}

fn draw_text(pixels: &mut [u8], x: i32, y: i32, text: &str, color: [u8; 4], scale: i32) {
    let mut cx = x;
    for ch in text.chars() {
        if ch == ' ' {
            cx += 4 * scale;
            continue;
        }
        draw_char(pixels, cx, y, ch, color, scale);
        cx += 6 * scale;
    }
}

fn draw_char(pixels: &mut [u8], x: i32, y: i32, ch: char, color: [u8; 4], scale: i32) {
    let glyph = glyph(ch);
    for (row, bits) in glyph.iter().enumerate() {
        for col in 0..5 {
            if (bits >> (4 - col)) & 1 != 0 {
                fill_rect(
                    pixels,
                    x + col * scale,
                    y + row as i32 * scale,
                    scale,
                    scale,
                    color,
                );
            }
        }
    }
}

fn glyph(ch: char) -> [u8; 7] {
    match ch.to_ascii_uppercase() {
        'A' => [0x0e, 0x11, 0x11, 0x1f, 0x11, 0x11, 0x11],
        'B' => [0x1e, 0x11, 0x11, 0x1e, 0x11, 0x11, 0x1e],
        'C' => [0x0e, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0e],
        'D' => [0x1e, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1e],
        'E' => [0x1f, 0x10, 0x10, 0x1e, 0x10, 0x10, 0x1f],
        'F' => [0x1f, 0x10, 0x10, 0x1e, 0x10, 0x10, 0x10],
        'G' => [0x0e, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0f],
        'H' => [0x11, 0x11, 0x11, 0x1f, 0x11, 0x11, 0x11],
        'I' => [0x0e, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0e],
        'J' => [0x07, 0x02, 0x02, 0x02, 0x12, 0x12, 0x0c],
        'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1f],
        'M' => [0x11, 0x1b, 0x15, 0x15, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x19, 0x15, 0x13, 0x11, 0x11, 0x11],
        'O' => [0x0e, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0e],
        'P' => [0x1e, 0x11, 0x11, 0x1e, 0x10, 0x10, 0x10],
        'R' => [0x1e, 0x11, 0x11, 0x1e, 0x14, 0x12, 0x11],
        'S' => [0x0f, 0x10, 0x10, 0x0e, 0x01, 0x01, 0x1e],
        'T' => [0x1f, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0e],
        'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0a, 0x04],
        'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x1b, 0x11],
        'X' => [0x11, 0x11, 0x0a, 0x04, 0x0a, 0x11, 0x11],
        'Y' => [0x11, 0x11, 0x0a, 0x04, 0x04, 0x04, 0x04],
        'Z' => [0x1f, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1f],
        '0' => [0x0e, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0e],
        '1' => [0x04, 0x0c, 0x04, 0x04, 0x04, 0x04, 0x0e],
        '2' => [0x0e, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1f],
        '3' => [0x1e, 0x01, 0x01, 0x0e, 0x01, 0x01, 0x1e],
        '4' => [0x02, 0x06, 0x0a, 0x12, 0x1f, 0x02, 0x02],
        '5' => [0x1f, 0x10, 0x10, 0x1e, 0x01, 0x01, 0x1e],
        '6' => [0x0e, 0x10, 0x10, 0x1e, 0x11, 0x11, 0x0e],
        '7' => [0x1f, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0e, 0x11, 0x11, 0x0e, 0x11, 0x11, 0x0e],
        '9' => [0x0e, 0x11, 0x11, 0x0f, 0x01, 0x01, 0x0e],
        ':' => [0x00, 0x04, 0x04, 0x00, 0x04, 0x04, 0x00],
        ',' => [0x00, 0x00, 0x00, 0x00, 0x04, 0x04, 0x08],
        '-' => [0x00, 0x00, 0x00, 0x1f, 0x00, 0x00, 0x00],
        '_' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1f],
        _ => [0x1f, 0x01, 0x02, 0x04, 0x04, 0x00, 0x04],
    }
}
