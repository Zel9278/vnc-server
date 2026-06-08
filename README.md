# vnc-server

Small standalone RFB/VNC server module.

This crate streams top-down BGRA/BGRX frames through VNC and exposes optional input callbacks for keyboard, pointer, wheel, and clipboard events. It supports output-only mode, input callbacks, and standard VNC password authentication.

## Use

```rust
use std::sync::Arc;
use vnc_server::{start_vnc_server, SharedFrame, VncServerConfig};

let frame = SharedFrame::new(800, 480);
start_vnc_server(
    "127.0.0.1:5900".to_string(),
    Arc::clone(&frame),
    "example".to_string(),
    VncServerConfig::default(),
)?;

// Publish top-down BGRA/BGRX bytes, width * height * 4.
frame.publish(&pixels);
# Ok::<(), std::io::Error>(())
```

For input events, use `VncServerConfig::new().with_input_callback(callback)` and handle `VncInputEvent`.

For password authentication, use:

```rust
let config = VncServerConfig::new().with_password("secret");
```

VNC password authentication uses the classic VNC challenge-response scheme. Only the first 8 password bytes are used by the protocol. You can check this with `VncAuth::password_is_truncated()`.

## Examples

Input/click/keyboard demo:

```powershell
cargo run --example vnc_input_demo -- 5902
```

Input demo with password auth:

```powershell
cargo run --example vnc_input_demo -- 5902 secret
```

Headless egui + wgpu demo:

```powershell
cargo run --example vnc_egui_headless -- 5903
```

Headless egui demo with password auth:

```powershell
cargo run --example vnc_egui_headless -- 5903 secret
```

Minimal RFB probe client for testing a running server:

```powershell
cargo run --example vnc_probe -- 5902
```

Probe with Hextile encoding:

```powershell
cargo run --example vnc_probe -- 5902 hextile
```

Probe a password-protected server:

```powershell
cargo run --example vnc_probe -- 5902 native secret
```

## Notes

- RFB 3.8 with compatibility for 3.7/3.3 clients.
- No TLS; bind to `127.0.0.1` unless you add an access-control layer.
- Raw and Hextile encodings.
- Incremental requests use tracked dirty rectangles.
- `VncInputEvent` includes helpers for pointer position, button masks, wheel deltas, and printable key text.


