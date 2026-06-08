// Multi-client VNC input test server.
//
// Usage:
//   cargo run --example vnc_multi_input_demo -- [--host HOST] [--port PORT] [--passwd PASS] [--low-bandwidth]
//
// Connect two or more VNC clients to the same address. Each client gets its own
// cursor color, button state, and typed text buffer.

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

const WIDTH: u16 = 960;
const HEIGHT: u16 = 540;

enum DemoEvent {
    Input(VncInputEvent),
    Client(VncClientEvent),
}

#[derive(Default)]
struct ClientInput {
    peer: String,
    button_mask: u8,
    key_down: bool,
    last_key: Option<u32>,
    typed_text: String,
    pointer_events: u64,
    key_events: u64,
    click_events: u64,
    wheel_events: i64,
    previous_button_mask: u8,
    flash_until: Option<Instant>,
}

struct DemoState {
    clients: HashMap<u64, ClientInput>,
    total_events: u64,
}

impl DemoState {
    fn new() -> Self {
        Self {
            clients: HashMap::new(),
            total_events: 0,
        }
    }

    fn apply(&mut self, event: DemoEvent) {
        match event {
            DemoEvent::Client(event) => match event {
                VncClientEvent::Connected { id, peer } => {
                    self.clients.entry(id).or_insert_with(|| ClientInput {
                        peer: peer
                            .map(|peer| peer.to_string())
                            .unwrap_or_else(|| "-".to_string()),
                        ..Default::default()
                    });
                }
                VncClientEvent::Disconnected { id, .. } => {
                    self.clients.remove(&id);
                }
                VncClientEvent::Rejected { .. } => {}
            },
            DemoEvent::Input(event) => {
                self.total_events += 1;
                let client_id = event.client_id();
                let client = self
                    .clients
                    .entry(client_id)
                    .or_insert_with(|| ClientInput {
                        peer: event
                            .peer()
                            .map(|peer| peer.to_string())
                            .unwrap_or_else(|| "-".to_string()),
                        ..Default::default()
                    });
                client.flash_until = Some(Instant::now() + Duration::from_millis(140));

                match event {
                    VncInputEvent::Pointer {
                        button_mask,
                        x: _,
                        y: _,
                        ..
                    } => {
                        client.pointer_events += 1;
                        let wheel = VncInputEvent::Pointer {
                            client_id,
                            peer: None,
                            button_mask,
                            x: 0,
                            y: 0,
                        }
                        .wheel_delta(client.previous_button_mask);
                        client.wheel_events += wheel as i64;
                        for button in [
                            VncMouseButton::Left,
                            VncMouseButton::Middle,
                            VncMouseButton::Right,
                        ] {
                            let was = (client.previous_button_mask & button.mask()) != 0;
                            let now = (button_mask & button.mask()) != 0;
                            if !was && now {
                                client.click_events += 1;
                            }
                        }
                        client.previous_button_mask = button_mask;
                        client.button_mask = button_mask;
                    }
                    VncInputEvent::Key { down, key, .. } => {
                        client.key_events += 1;
                        client.key_down = down;
                        client.last_key = Some(key);
                        if down {
                            if let Some(ch) = VncKeyText::from_keysym(key) {
                                client.typed_text.push(ch);
                            } else {
                                apply_control_key(&mut client.typed_text, key);
                            }
                            trim_text(&mut client.typed_text, 34);
                        }
                    }
                    VncInputEvent::ClientCutText { text, .. } => {
                        client.typed_text = String::from_utf8_lossy(&text).to_string();
                        trim_text(&mut client.typed_text, 34);
                    }
                }
            }
        }
    }
}

fn main() -> io::Result<()> {
    let opts = ServerCli::parse("vnc_multi_input_demo", 5904)?;
    if opts.help {
        print_server_help("vnc_multi_input_demo", 5904);
        return Ok(());
    }

    let frame = SharedFrame::new(WIDTH, HEIGHT);
    let (tx, rx) = mpsc::channel::<DemoEvent>();
    let input_tx = tx.clone();
    let input = Arc::new(move |event: VncInputEvent| {
        let _ = input_tx.send(DemoEvent::Input(event));
    });
    let client_tx = tx.clone();
    let clients = Arc::new(move |event: VncClientEvent| {
        println!("client event: {event:?}");
        let _ = client_tx.send(DemoEvent::Client(event));
    });

    let mut config = VncServerConfig::new()
        .with_bind_addr(format!("{}:{}", opts.host, opts.port))
        .with_name("vnc-multi-input-demo")
        .with_max_clients(6)
        .with_client_callback(clients)
        .with_input_callback(input);
    if let Some(password) = opts.passwd {
        config = config.with_password(password);
        println!("VNC password authentication enabled");
    }
    if opts.low_bandwidth {
        config = config.with_low_bandwidth();
        println!("Low-bandwidth mode enabled: preferred pixel format is RGB565");
    }

    let server = start_vnc_server(Arc::clone(&frame), config)?;
    println!(
        "Multi-input VNC demo listening on {}:{}",
        opts.host, opts.port
    );
    println!("Connect multiple VNC clients, move both pointers, click, and type in each client.");

    let mut state = DemoState::new();
    let mut pixels = vec![0u8; WIDTH as usize * HEIGHT as usize * 4];
    loop {
        while let Ok(event) = rx.try_recv() {
            state.apply(event);
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
    help: bool,
}

impl ServerCli {
    fn parse(example: &str, default_port: u16) -> io::Result<Self> {
        let mut out = Self {
            host: "127.0.0.1".to_string(),
            port: default_port,
            passwd: None,
            low_bandwidth: false,
            help: false,
        };
        let mut positional = Vec::new();
        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => out.help = true,
                "--low-bandwidth" => out.low_bandwidth = true,
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
        "Usage: cargo run --example {example} -- [OPTIONS]\n\nOptions:\n  --host HOST        Bind host/address, e.g. 127.0.0.1 or 0.0.0.0 (default: 127.0.0.1)\n  --port PORT        Bind port (default: {default_port})\n  --passwd PASS      Enable VNC password authentication\n  --password PASS    Alias for --passwd\n  --low-bandwidth    Prefer 16bpp RGB565 for clients that accept it\n  -h, --help         Show this help\n\nBackward-compatible positional form:\n  cargo run --example {example} -- [port] [password]\n"
    );
}

fn render(pixels: &mut [u8], state: &DemoState, cursors: &[VncCursor]) {
    clear(pixels, [18, 22, 28, 255]);
    draw_grid(pixels);

    draw_text(
        pixels,
        28,
        24,
        "MULTI VNC INPUT TEST",
        [240, 244, 248, 255],
        3,
    );
    draw_text(
        pixels,
        30,
        64,
        &format!("CLIENTS {}  EVENTS {}", cursors.len(), state.total_events),
        [200, 210, 220, 255],
        2,
    );

    for (index, cursor) in cursors.iter().enumerate() {
        let y = 106 + index as i32 * 72;
        let color = cursor_color(cursor.client_id);
        let client = state.clients.get(&cursor.client_id);
        let flash = client
            .and_then(|client| client.flash_until)
            .is_some_and(|until| Instant::now() < until);
        fill_rect(
            pixels,
            28,
            y,
            456,
            62,
            if flash {
                [42, 62, 74, 255]
            } else {
                [30, 36, 44, 255]
            },
        );
        stroke_rect(pixels, 28, y, 456, 58, color);
        fill_rect(pixels, 42, y + 18, 16, 16, color);
        draw_text(
            pixels,
            70,
            y + 10,
            &format!(
                "C{} {} POS {:03},{:03} BTN {:08b}",
                cursor.client_id,
                client.map(|client| client.peer.as_str()).unwrap_or("-"),
                cursor.x,
                cursor.y,
                cursor.button_mask
            ),
            [225, 231, 237, 255],
            1,
        );
        draw_text(
            pixels,
            70,
            y + 30,
            &format!(
                "KEY {} TEXT {}",
                client
                    .and_then(|client| client.last_key)
                    .map(|key| format!("0X{key:08X}"))
                    .unwrap_or_else(|| "-".to_string()),
                client
                    .map(|client| visible_text(&client.typed_text))
                    .unwrap_or_else(|| "_".to_string())
            ),
            [200, 210, 220, 255],
            1,
        );
        draw_text(
            pixels,
            70,
            y + 45,
            &format!(
                "MOVE {} KEYEV {} CLICK {} WHEEL {}",
                client.map(|client| client.pointer_events).unwrap_or(0),
                client.map(|client| client.key_events).unwrap_or(0),
                client.map(|client| client.click_events).unwrap_or(0),
                client.map(|client| client.wheel_events).unwrap_or(0)
            ),
            [170, 185, 198, 255],
            1,
        );
        if cursor.position_known {
            draw_cursor(pixels, cursor, color);
        }
    }

    draw_text(
        pixels,
        540,
        90,
        "OPEN TWO VNC CLIENTS",
        [235, 239, 243, 255],
        2,
    );
    draw_text(
        pixels,
        540,
        122,
        "EACH CLIENT HAS ITS OWN CURSOR AND TEXT",
        [196, 207, 218, 255],
        1,
    );
    draw_text(
        pixels,
        540,
        144,
        "TYPE DIFFERENT LETTERS IN EACH WINDOW",
        [196, 207, 218, 255],
        1,
    );
}

fn draw_cursor(pixels: &mut [u8], cursor: &VncCursor, color: [u8; 4]) {
    let x = cursor.x as i32;
    let y = cursor.y as i32;
    fill_rect(pixels, x - 18, y, 37, 2, color);
    fill_rect(pixels, x, y - 18, 2, 37, color);
    stroke_rect(pixels, x - 8, y - 8, 17, 17, color);
    draw_text(
        pixels,
        x + 14,
        y + 12,
        &format!("C{}", cursor.client_id),
        color,
        1,
    );
}

struct VncKeyText;

impl VncKeyText {
    fn from_keysym(key: u32) -> Option<char> {
        match key {
            0x20..=0x7e => char::from_u32(key),
            _ => None,
        }
    }
}

fn apply_control_key(text: &mut String, key: u32) {
    match key {
        0xff08 => {
            text.pop();
        }
        0xff0d | 0xff8d => text.push(' '),
        0xff1b => text.clear(),
        _ => {}
    }
}

fn trim_text(text: &mut String, max_chars: usize) {
    let count = text.chars().count();
    if count <= max_chars {
        return;
    }
    let keep_from = text
        .char_indices()
        .nth(count - max_chars)
        .map(|(idx, _)| idx)
        .unwrap_or(0);
    text.drain(..keep_from);
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

fn cursor_color(client_id: u64) -> [u8; 4] {
    const COLORS: [[u8; 4]; 8] = [
        [255, 235, 118, 255],
        [86, 214, 190, 255],
        [255, 139, 148, 255],
        [150, 185, 255, 255],
        [184, 231, 111, 255],
        [231, 163, 255, 255],
        [255, 181, 109, 255],
        [120, 225, 255, 255],
    ];
    COLORS[(client_id as usize).wrapping_sub(1) % COLORS.len()]
}

fn clear(pixels: &mut [u8], color: [u8; 4]) {
    for px in pixels.chunks_exact_mut(4) {
        px.copy_from_slice(&color);
    }
}

fn draw_grid(pixels: &mut [u8]) {
    for x in (0..WIDTH as i32).step_by(40) {
        fill_rect(pixels, x, 0, 1, HEIGHT as i32, [28, 34, 42, 255]);
    }
    for y in (0..HEIGHT as i32).step_by(40) {
        fill_rect(pixels, 0, y, WIDTH as i32, 1, [28, 34, 42, 255]);
    }
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
        'Q' => [0x0e, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0d],
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
        '#' => [0x0a, 0x1f, 0x0a, 0x0a, 0x1f, 0x0a, 0x00],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x0c, 0x0c],
        _ => [0x1f, 0x01, 0x02, 0x04, 0x04, 0x00, 0x04],
    }
}
