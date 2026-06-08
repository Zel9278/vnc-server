// VNC input demo server.
//
// Usage:
//   cargo run --example vnc_input_demo -- [port] [password]
//
// Then connect a VNC client to 127.0.0.1:<port>.
// Pointer movement, mouse buttons, clipboard text, and keyboard events are
// printed to the console. Pointer and mouse button state are also rendered into
// the streamed framebuffer.


use std::env;
use std::io;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};
use vnc_server::{start_vnc_server, SharedFrame, VncInputEvent, VncServerConfig};

const WIDTH: u16 = 800;
const HEIGHT: u16 = 480;

#[derive(Clone)]
struct DemoState {
    pointer_x: u16,
    pointer_y: u16,
    button_mask: u8,
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
    let mut args = env::args().skip(1);
    let port: u16 = args
        .next()
        .unwrap_or_else(|| "5902".to_string())
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "port must be a number"))?;
    let password = args.next();

    let frame = SharedFrame::new(WIDTH, HEIGHT);
    let (tx, rx) = mpsc::channel::<VncInputEvent>();
    let input = Arc::new(move |event: VncInputEvent| {
        let _ = tx.send(event);
    });

    let mut config = VncServerConfig::new().with_input_callback(input);
    if let Some(password) = password {
        config = config.with_password(password);
        println!("VNC password authentication enabled");
    }

    start_vnc_server(
        format!("127.0.0.1:{port}"),
        Arc::clone(&frame),
        "vnc-input-demo".to_string(),
        config,
    )?;

    println!("VNC input demo listening on 127.0.0.1:{port}");
    println!("Move the pointer, click, type keys, or paste clipboard text in your VNC client.");

    let mut state = DemoState::new();
    let mut pixels = vec![0u8; WIDTH as usize * HEIGHT as usize * 4];
    loop {
        while let Ok(event) = rx.try_recv() {
            state.event_count += 1;
            state.flash_until = Instant::now() + Duration::from_millis(120);
            match event {
                VncInputEvent::Pointer { button_mask, x, y } => {
                    state.pointer_x = x.min(WIDTH.saturating_sub(1));
                    state.pointer_y = y.min(HEIGHT.saturating_sub(1));
                    let wheel_up = (button_mask & (1 << 3)) != 0
                        && (state.button_mask & (1 << 3)) == 0;
                    let wheel_down = (button_mask & (1 << 4)) != 0
                        && (state.button_mask & (1 << 4)) == 0;
                    if wheel_up {
                        state.wheel_up_count += 1;
                        state.wheel_flash_until = Instant::now() + Duration::from_millis(500);
                    }
                    if wheel_down {
                        state.wheel_down_count += 1;
                        state.wheel_flash_until = Instant::now() + Duration::from_millis(500);
                    }
                    state.button_mask = button_mask;
                    println!(
                        "pointer x={} y={} buttons=0b{:08b} wheel_up={} wheel_down={}",
                        state.pointer_x, state.pointer_y, button_mask, wheel_up, wheel_down
                    );
                }
                VncInputEvent::Key { down, key } => {
                    state.last_key = Some(key);
                    state.key_down = down;
                    if down {
                        apply_key_to_text(&mut state.typed_text, key);
                    }
                    println!("key {} keysym=0x{key:08x}", if down { "down" } else { "up" });
                }
                VncInputEvent::ClientCutText(bytes) => {
                    state.cut_text_len = bytes.len();
                    println!(
                        "client cut text: {} bytes: {}",
                        bytes.len(),
                        String::from_utf8_lossy(&bytes)
                    );
                }
            }
        }

        render(&mut pixels, &state);
        frame.publish(&pixels);
        thread::sleep(Duration::from_millis(33));
    }
}

fn render(pixels: &mut [u8], state: &DemoState) {
    clear(pixels, [24, 28, 34, 255]);
    draw_grid(pixels);

    let flash = Instant::now() < state.flash_until;
    let panel = if flash { [46, 84, 112, 255] } else { [36, 43, 52, 255] };
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
        ("LEFT", 0u8),
        ("MID", 1u8),
        ("RIGHT", 2u8),
        ("WHL+", 3u8),
        ("WHL-", 4u8),
    ];
    for (idx, (label, bit)) in buttons.iter().enumerate() {
        let active = (state.button_mask & (1 << bit)) != 0;
        let wheel = *bit >= 3 && Instant::now() < state.wheel_flash_until;
        let x = 48 + idx as i32 * 112;
        fill_rect(
            pixels,
            x,
            base_y,
            88,
            72,
            if active || wheel { [53, 178, 112, 255] } else { [54, 61, 70, 255] },
        );
        stroke_rect(pixels, x, base_y, 88, 72, [103, 116, 130, 255]);
        draw_text(
            pixels,
            x + 10,
            base_y + 26,
            label,
            if active || wheel { [12, 24, 18, 255] } else { [204, 211, 218, 255] },
            2,
        );
    }
    draw_text(
        pixels,
        48,
        320,
        &format!("WHEEL UP {}  DOWN {}", state.wheel_up_count, state.wheel_down_count),
        [214, 222, 230, 255],
        2,
    );

    draw_crosshair(pixels, state.pointer_x as i32, state.pointer_y as i32);
}

fn apply_key_to_text(text: &mut String, key: u32) {
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
        0x20..=0x7e => {
            if let Some(ch) = char::from_u32(key) {
                text.push(ch);
            }
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

fn draw_crosshair(pixels: &mut [u8], x: i32, y: i32) {
    fill_rect(pixels, x - 16, y, 33, 2, [255, 235, 118, 255]);
    fill_rect(pixels, x, y - 16, 2, 33, [255, 235, 118, 255]);
    stroke_rect(pixels, x - 7, y - 7, 16, 16, [255, 235, 118, 255]);
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

