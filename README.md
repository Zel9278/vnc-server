# vnc-server

Small standalone RFB/VNC server module.

This crate streams top-down BGRA/BGRX frames through VNC. It supports output-only mode, input callbacks, client connect/disconnect callbacks, max-client limits, bidirectional clipboard messages, standard VNC password authentication, Raw, Hextile, Zlib, and ZRLE encodings, optional 16bpp RGB565 preferred output, and tile-based damage tracking.

## Use

```rust,no_run
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use vnc_server::{start_vnc_server, SharedFrame, VncServerConfig};

fn main() -> std::io::Result<()> {
    const WIDTH: u16 = 800;
    const HEIGHT: u16 = 480;

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
```

## Configuration

Input events:

```rust
use std::sync::Arc;
use vnc_server::{VncInputEvent, VncMouseButton, VncServerConfig};

let callback = Arc::new(|event: VncInputEvent| {
    match event {
        VncInputEvent::Pointer {
            client_id,
            button_mask,
            x,
            y,
            ..
        } => {
            let left = (button_mask & VncMouseButton::Left.mask()) != 0;
            println!("client #{client_id} pointer {x},{y} left={left}");
        }
        VncInputEvent::Key {
            client_id,
            down,
            key,
            ..
        } => {
            println!("client #{client_id} key {key:#x} down={down}");
        }
        VncInputEvent::ClientCutText {
            client_id, text, ..
        } => {
            println!("client #{client_id} clipboard {} bytes", text.len());
        }
    }
});

let config = VncServerConfig::new().with_input_callback(callback);
```

Each input event includes `client_id` and `peer`, so a server can maintain separate state per connected VNC client.

Current client cursors:

```rust,no_run
# use std::sync::Arc;
# use vnc_server::{start_vnc_server, SharedFrame, VncServerConfig};
# fn main() -> std::io::Result<()> {
# let frame = SharedFrame::new(800, 480);
# let server = start_vnc_server(Arc::clone(&frame), VncServerConfig::new())?;
for cursor in server.client_cursors() {
    if cursor.position_known {
        println!(
            "client #{} at {},{} buttons=0b{:08b}",
            cursor.client_id, cursor.x, cursor.y, cursor.button_mask
        );
    }
}
# Ok(())
# }
```

Client connect/disconnect/reject events:

```rust
use std::sync::Arc;
use vnc_server::{VncClientEvent, VncServerConfig};

let callback = Arc::new(|event: VncClientEvent| {
    println!("client: {event:?}");
});

let config = VncServerConfig::new().with_client_callback(callback);
```

Password authentication:

```rust
use vnc_server::VncServerConfig;

let config = VncServerConfig::new().with_password("secret");
```

VNC password authentication uses the classic VNC challenge-response scheme. Only the first 8 password bytes are used by the protocol. You can check this with `VncAuth::password_is_truncated()`.

Low-bandwidth mode:

```rust
use vnc_server::{VncPixelFormat, VncServerConfig};

let config = VncServerConfig::new()
    .with_low_bandwidth()
    .with_preferred_pixel_format(VncPixelFormat::rgb565());
```

`with_low_bandwidth()` advertises 16bpp RGB565 in ServerInit. Clients may still send `SetPixelFormat`; the server follows the client's requested format when it does. If a client requests ZRLE or Zlib in `SetEncodings`, the server prefers `ZRLE`, then `Zlib`, then `Hextile`, then `Raw`.

## Examples

Basic framebuffer server:

```powershell
cargo run --example basic_server
```

Input/click/keyboard demo:

```powershell
cargo run --example vnc_input_demo -- --host 127.0.0.1 --port 5902
```

Multi-cursor and multi-keyboard test server:

```powershell
cargo run --example vnc_multi_input_demo -- --host 127.0.0.1 --port 5904
```

Connect two or more VNC clients to port `5904`. Each client has an independent cursor color, button state, and typed text buffer.

Low-bandwidth input demo:

```powershell
cargo run --example vnc_input_demo -- --host 0.0.0.0 --port 5902 --low-bandwidth
```

Listen on every network interface with password auth:

```powershell
cargo run --example vnc_input_demo -- --host 0.0.0.0 --port 5902 --passwd secret
```

Headless egui + wgpu demo:

```powershell
cargo run --example vnc_egui_headless -- --host 127.0.0.1 --port 5903
```

Headless egui demo with password auth:

```powershell
cargo run --example vnc_egui_headless -- --host 0.0.0.0 --port 5903 --passwd secret
```

Minimal RFB probe client for testing a running server:

```powershell
cargo run --example vnc_probe -- --host 127.0.0.1 --port 5902
```

Probe with Hextile encoding:

```powershell
cargo run --example vnc_probe -- --host 127.0.0.1 --port 5902 --encoding hextile
```

Probe with Zlib or ZRLE encoding:

```powershell
cargo run --example vnc_probe -- --host 127.0.0.1 --port 5902 --encoding zlib
cargo run --example vnc_probe -- --host 127.0.0.1 --port 5902 --encoding zrle
```

Probe a password-protected server:

```powershell
cargo run --example vnc_probe -- --host 127.0.0.1 --port 5902 --passwd secret
```

Show an example's CLI help:

```powershell
cargo run --example vnc_egui_headless -- --help
```

## Notes

- RFB 3.8 with compatibility for 3.7/3.3 clients.
- No TLS; bind to `127.0.0.1` unless you add an access-control layer.
- Raw, Hextile, Zlib, and ZRLE encodings.
- `VncServerConfig::with_low_bandwidth()` advertises 16bpp RGB565 before clients override the pixel format.
- Incremental requests use tile-based dirty rectangles.
- `VncInputEvent` includes client IDs, peer addresses, and helpers for pointer position, button masks, wheel deltas, and printable key text.
- `VncServerHandle` exposes shutdown, active client count, local address, per-client cursors, and server-to-client clipboard sending.

## Validation

README Rust snippets are included in crate docs and checked by rustdoc:

```powershell
cargo test --doc
```

Examples are checked with:

```powershell
cargo check --examples
```
