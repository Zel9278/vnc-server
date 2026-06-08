use std::sync::Arc;
use std::thread;
use std::time::Duration;
use vnc_server::{SharedFrame, VncServerConfig, start_vnc_server};

const WIDTH: u16 = 800;
const HEIGHT: u16 = 480;

fn main() -> std::io::Result<()> {
    let frame = SharedFrame::new(WIDTH, HEIGHT);
    let config = VncServerConfig::new()
        .with_bind_addr("127.0.0.1:5900")
        .with_name("example")
        .with_max_clients(4);

    let server = start_vnc_server(Arc::clone(&frame), config)?;
    server.set_clipboard_text("hello from server");

    println!("connect a VNC client to {}", server.local_addr());

    let mut pixels = vec![0u8; WIDTH as usize * HEIGHT as usize * 4];
    let mut tick = 0u8;
    loop {
        for y in 0..HEIGHT as usize {
            for x in 0..WIDTH as usize {
                let i = (y * WIDTH as usize + x) * 4;
                pixels[i] = tick;
                pixels[i + 1] = x as u8;
                pixels[i + 2] = y as u8;
                pixels[i + 3] = 0;
            }
        }
        frame.publish(&pixels);
        tick = tick.wrapping_add(1);
        thread::sleep(Duration::from_millis(33));
    }
}
