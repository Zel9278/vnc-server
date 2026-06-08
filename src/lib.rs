#![doc = include_str!("../README.md")]

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use std::thread;
use std::time::Duration;

use des::Des;
use des::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};
use flate2::{Compress, Compression, FlushCompress};
use jpeg_encoder::{ColorType, Encoder as JpegEncoder};

const DAMAGE_HISTORY_LIMIT: usize = 128;
const DAMAGE_TILE_SIZE: usize = 32;
const MAX_UPDATE_RECTS: usize = 1024;
const ENCODING_RAW: i32 = 0;
const ENCODING_HEXTILE: i32 = 5;
const ENCODING_ZLIB: i32 = 6;
const ENCODING_TIGHT: i32 = 7;
const ENCODING_ZRLE: i32 = 16;
const ENCODING_TIGHT_QUALITY_LEVEL_0: i32 = -32;
const ENCODING_TIGHT_QUALITY_LEVEL_9: i32 = -23;
const HEXTILE_RAW: u8 = 1;
const TIGHT_JPEG: u8 = 0x90;
const TIGHT_MAX_RECT_WIDTH: u16 = 2048;
const ZRLE_TILE_SIZE: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    fn full(width: u16, height: u16) -> Self {
        Self {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }

    fn intersect(self, other: Self) -> Option<Self> {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = self
            .x
            .saturating_add(self.width)
            .min(other.x.saturating_add(other.width));
        let y1 = self
            .y
            .saturating_add(self.height)
            .min(other.y.saturating_add(other.height));
        (x1 > x0 && y1 > y0).then_some(Self {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
        })
    }
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub enum VncInputEvent {
    Key {
        client_id: u64,
        peer: Option<SocketAddr>,
        down: bool,
        key: u32,
    },
    Pointer {
        client_id: u64,
        peer: Option<SocketAddr>,
        button_mask: u8,
        x: u16,
        y: u16,
    },
    ClientCutText {
        client_id: u64,
        peer: Option<SocketAddr>,
        text: Vec<u8>,
    },
}

impl VncInputEvent {
    pub fn client_id(&self) -> u64 {
        match self {
            Self::Key { client_id, .. }
            | Self::Pointer { client_id, .. }
            | Self::ClientCutText { client_id, .. } => *client_id,
        }
    }

    pub fn peer(&self) -> Option<SocketAddr> {
        match self {
            Self::Key { peer, .. }
            | Self::Pointer { peer, .. }
            | Self::ClientCutText { peer, .. } => *peer,
        }
    }

    pub fn pointer_position(&self) -> Option<(u16, u16)> {
        match *self {
            Self::Pointer { x, y, .. } => Some((x, y)),
            _ => None,
        }
    }

    pub fn button_mask(&self) -> Option<u8> {
        match *self {
            Self::Pointer { button_mask, .. } => Some(button_mask),
            _ => None,
        }
    }

    pub fn is_button_down(&self, button: VncMouseButton) -> bool {
        self.button_mask()
            .map(|mask| mask & button.mask() != 0)
            .unwrap_or(false)
    }

    pub fn wheel_delta(&self, previous_button_mask: u8) -> i32 {
        let Some(mask) = self.button_mask() else {
            return 0;
        };
        let wheel_up = (mask & VncMouseButton::WheelUp.mask()) != 0
            && (previous_button_mask & VncMouseButton::WheelUp.mask()) == 0;
        let wheel_down = (mask & VncMouseButton::WheelDown.mask()) != 0
            && (previous_button_mask & VncMouseButton::WheelDown.mask()) == 0;
        i32::from(wheel_up) - i32::from(wheel_down)
    }

    pub fn key(&self) -> Option<VncKey> {
        match *self {
            Self::Key { key, .. } => Some(VncKey::from_keysym(key)),
            _ => None,
        }
    }

    pub fn text(&self) -> Option<char> {
        match *self {
            Self::Key {
                down: true, key, ..
            } => VncKey::from_keysym(key).text(),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VncCursor {
    pub client_id: u64,
    pub peer: Option<SocketAddr>,
    pub x: u16,
    pub y: u16,
    pub button_mask: u8,
    pub position_known: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VncMouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

impl VncMouseButton {
    pub fn mask(self) -> u8 {
        match self {
            Self::Left => 1 << 0,
            Self::Middle => 1 << 1,
            Self::Right => 1 << 2,
            Self::WheelUp => 1 << 3,
            Self::WheelDown => 1 << 4,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VncKey {
    Backspace,
    Tab,
    Enter,
    Escape,
    ArrowLeft,
    ArrowUp,
    ArrowRight,
    ArrowDown,
    Shift,
    Control,
    Alt,
    Meta,
    Character(char),
    Other(u32),
}

impl VncKey {
    pub fn from_keysym(keysym: u32) -> Self {
        match keysym {
            0xff08 => Self::Backspace,
            0xff09 => Self::Tab,
            0xff0d | 0xff8d => Self::Enter,
            0xff1b => Self::Escape,
            0xff51 => Self::ArrowLeft,
            0xff52 => Self::ArrowUp,
            0xff53 => Self::ArrowRight,
            0xff54 => Self::ArrowDown,
            0xffe1 | 0xffe2 => Self::Shift,
            0xffe3 | 0xffe4 => Self::Control,
            0xffe7 | 0xffe8 => Self::Alt,
            0xffeb | 0xffec => Self::Meta,
            0x20..=0x7e => char::from_u32(keysym)
                .map(Self::Character)
                .unwrap_or(Self::Other(keysym)),
            _ => Self::Other(keysym),
        }
    }

    pub fn text(self) -> Option<char> {
        match self {
            Self::Character(ch) => Some(ch),
            _ => None,
        }
    }
}

pub type VncInputCallback = Arc<dyn Fn(VncInputEvent) + Send + Sync + 'static>;
pub type VncClientCallback = Arc<dyn Fn(VncClientEvent) + Send + Sync + 'static>;

#[derive(Clone, Debug)]
pub enum VncClientEvent {
    Connected {
        id: u64,
        peer: Option<SocketAddr>,
    },
    Disconnected {
        id: u64,
        peer: Option<SocketAddr>,
        reason: Option<String>,
    },
    Rejected {
        peer: Option<SocketAddr>,
        reason: String,
    },
}

#[derive(Clone)]
pub struct VncServerConfig {
    pub bind_addr: String,
    pub name: String,
    pub input_callback: Option<VncInputCallback>,
    pub client_callback: Option<VncClientCallback>,
    pub auth: VncAuth,
    pub max_clients: Option<usize>,
    pub preferred_pixel_format: VncPixelFormat,
    pub tight_jpeg_quality: u8,
}

impl VncServerConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_bind_addr(mut self, bind_addr: impl Into<String>) -> Self {
        self.bind_addr = bind_addr.into();
        self
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn with_input_callback(mut self, callback: VncInputCallback) -> Self {
        self.input_callback = Some(callback);
        self
    }

    pub fn with_client_callback(mut self, callback: VncClientCallback) -> Self {
        self.client_callback = Some(callback);
        self
    }

    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.auth = VncAuth::Password(password.into());
        self
    }

    pub fn with_max_clients(mut self, max_clients: usize) -> Self {
        self.max_clients = Some(max_clients);
        self
    }

    pub fn with_preferred_pixel_format(mut self, pixel_format: VncPixelFormat) -> Self {
        self.preferred_pixel_format = pixel_format;
        self
    }

    pub fn with_low_bandwidth(mut self) -> Self {
        self.preferred_pixel_format = VncPixelFormat::rgb565();
        self
    }

    pub fn with_tight_jpeg_quality(mut self, quality: u8) -> Self {
        self.tight_jpeg_quality = quality.clamp(1, 100);
        self
    }
}

impl Default for VncServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1:5900".to_string(),
            name: "vnc-server".to_string(),
            input_callback: None,
            client_callback: None,
            auth: VncAuth::None,
            max_clients: None,
            preferred_pixel_format: VncPixelFormat::native(),
            tight_jpeg_quality: 75,
        }
    }
}

#[derive(Clone, Default)]
pub enum VncAuth {
    #[default]
    None,
    Password(String),
}

impl VncAuth {
    pub const PASSWORD_MAX_BYTES: usize = 8;

    fn security_type(&self) -> u8 {
        match self {
            Self::None => 1,
            Self::Password(_) => 2,
        }
    }

    pub fn password_is_truncated(&self) -> bool {
        match self {
            Self::None => false,
            Self::Password(password) => password.as_bytes().len() > Self::PASSWORD_MAX_BYTES,
        }
    }
}

#[derive(Clone)]
pub struct VncServerHandle {
    state: Arc<ServerState>,
    local_addr: SocketAddr,
}

impl VncServerHandle {
    pub fn shutdown(&self) {
        self.state.shutdown.store(true, Ordering::Release);
        self.state.clipboard.cond.notify_all();
    }

    pub fn is_shutdown(&self) -> bool {
        self.state.shutdown.load(Ordering::Acquire)
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn active_clients(&self) -> usize {
        self.state.active_clients.load(Ordering::Acquire)
    }

    pub fn client_cursors(&self) -> Vec<VncCursor> {
        self.state
            .cursors
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect()
    }

    pub fn set_clipboard_text(&self, text: impl Into<Vec<u8>>) {
        let mut inner = self.state.clipboard.inner.lock().unwrap();
        inner.seq = inner.seq.wrapping_add(1);
        inner.text = text.into();
        drop(inner);
        self.state.clipboard.cond.notify_all();
    }
}

struct ServerState {
    shutdown: AtomicBool,
    active_clients: AtomicUsize,
    next_client_id: AtomicU64,
    clipboard: ServerClipboard,
    cursors: Mutex<HashMap<u64, VncCursor>>,
}

impl ServerState {
    fn new() -> Self {
        Self {
            shutdown: AtomicBool::new(false),
            active_clients: AtomicUsize::new(0),
            next_client_id: AtomicU64::new(1),
            clipboard: ServerClipboard {
                inner: Mutex::new(ClipboardInner {
                    seq: 0,
                    text: Vec::new(),
                }),
                cond: Condvar::new(),
            },
            cursors: Mutex::new(HashMap::new()),
        }
    }
}

struct ServerClipboard {
    inner: Mutex<ClipboardInner>,
    cond: Condvar,
}

struct ClipboardInner {
    seq: u64,
    text: Vec<u8>,
}

/// Latest rendered frame, shared between the render loop (producer) and any
/// number of connected VNC clients (consumers). `data` is BGRX, width*height*4.
pub struct SharedFrame {
    inner: Mutex<FrameInner>,
    cond: Condvar,
}

struct FrameInner {
    width: u16,
    height: u16,
    data: Vec<u8>,
    seq: u64,
    damages: VecDeque<FrameDamage>,
}

#[derive(Clone)]
struct FrameDamage {
    seq: u64,
    rects: Vec<Rect>,
}

impl SharedFrame {
    pub fn new(width: u16, height: u16) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(FrameInner {
                width,
                height,
                data: vec![0u8; width as usize * height as usize * 4],
                seq: 0,
                damages: VecDeque::new(),
            }),
            cond: Condvar::new(),
        })
    }

    pub fn publish(&self, frame: &[u8]) {
        let mut inner = self.inner.lock().unwrap();
        let damage = compute_damage_rects(
            &inner.data,
            frame,
            inner.width as usize,
            inner.height as usize,
        );
        inner.data.clear();
        inner.data.extend_from_slice(frame);
        inner.seq = inner.seq.wrapping_add(1);
        let seq = inner.seq;
        inner.damages.push_back(FrameDamage { seq, rects: damage });
        while inner.damages.len() > DAMAGE_HISTORY_LIMIT {
            inner.damages.pop_front();
        }
        drop(inner);
        self.cond.notify_all();
    }
}

/// Start a minimal RFB 3.8 (VNC) server.
///
/// Each client is served Raw-encoded frames. Use [`VncServerConfig`] to set the
/// bind address, desktop name, callbacks, authentication, and client limit.
///
/// Returns once the listener is bound; client handling happens on background
/// threads. Use the returned [`VncServerHandle`] to shut the server down or send
/// clipboard text to clients.
pub fn start_vnc_server(
    frame: Arc<SharedFrame>,
    config: VncServerConfig,
) -> io::Result<VncServerHandle> {
    let listener = TcpListener::bind(&config.bind_addr)?;
    listener.set_nonblocking(true)?;
    let local_addr = listener.local_addr()?;
    println!("VNC server listening on {local_addr}");
    let state = Arc::new(ServerState::new());
    let handle = VncServerHandle {
        state: Arc::clone(&state),
        local_addr,
    };
    let listener_state = Arc::clone(&state);
    thread::Builder::new()
        .name("vnc-listener".to_string())
        .spawn(move || {
            while !listener_state.shutdown.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, peer_addr)) => {
                        if let Err(e) = stream.set_nonblocking(false) {
                            if let Some(cb) = &config.client_callback {
                                cb(VncClientEvent::Rejected {
                                    peer: Some(peer_addr),
                                    reason: format!(
                                        "failed to switch client socket to blocking: {e}"
                                    ),
                                });
                            }
                            drop(stream);
                            continue;
                        }
                        let peer = Some(peer_addr);
                        if let Some(max_clients) = config.max_clients {
                            if listener_state.active_clients.load(Ordering::Acquire) >= max_clients
                            {
                                if let Some(cb) = &config.client_callback {
                                    cb(VncClientEvent::Rejected {
                                        peer,
                                        reason: "client limit reached".to_string(),
                                    });
                                }
                                drop(stream);
                                continue;
                            }
                        }
                        let peer = stream.peer_addr().ok().or(peer);
                        let frame = Arc::clone(&frame);
                        let config = config.clone();
                        let client_state = Arc::clone(&listener_state);
                        let client_id = client_state.next_client_id.fetch_add(1, Ordering::AcqRel);
                        client_state.active_clients.fetch_add(1, Ordering::AcqRel);
                        client_state.cursors.lock().unwrap().insert(
                            client_id,
                            VncCursor {
                                client_id,
                                peer,
                                x: 0,
                                y: 0,
                                button_mask: 0,
                                position_known: false,
                            },
                        );
                        if let Some(cb) = &config.client_callback {
                            cb(VncClientEvent::Connected {
                                id: client_id,
                                peer,
                            });
                        }
                        thread::spawn(move || {
                            let result = handle_vnc_client(
                                stream,
                                frame,
                                config.name.clone(),
                                config.clone(),
                                Arc::clone(&client_state),
                                client_id,
                                peer,
                            );
                            client_state.active_clients.fetch_sub(1, Ordering::AcqRel);
                            client_state.cursors.lock().unwrap().remove(&client_id);
                            let reason = result.as_ref().err().map(|e| e.to_string());
                            if let Some(cb) = &config.client_callback {
                                cb(VncClientEvent::Disconnected {
                                    id: client_id,
                                    peer,
                                    reason: reason.clone(),
                                });
                            }
                            if let Err(e) = result {
                                println!("VNC client {peer:?} disconnected: {e}");
                            } else {
                                println!("VNC client {peer:?} disconnected");
                            }
                        });
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => {
                        if !listener_state.shutdown.load(Ordering::Acquire) {
                            eprintln!("VNC accept error: {e}");
                        }
                        thread::sleep(Duration::from_millis(50));
                    }
                }
            }
        })?;
    Ok(handle)
}

fn run_vnc_password_auth(
    stream: &mut TcpStream,
    password: &str,
    send_security_result: bool,
) -> io::Result<()> {
    let challenge: [u8; 16] = rand::random();
    stream.write_all(&challenge)?;
    let mut response = [0u8; 16];
    stream.read_exact(&mut response)?;
    let expected = vnc_password_response(password, challenge);
    if response == expected {
        if send_security_result {
            stream.write_all(&0u32.to_be_bytes())?;
        }
        Ok(())
    } else {
        if send_security_result {
            stream.write_all(&1u32.to_be_bytes())?;
        }
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "VNC password authentication failed",
        ))
    }
}

fn vnc_password_response(password: &str, challenge: [u8; 16]) -> [u8; 16] {
    let mut key = [0u8; 8];
    for (dst, src) in key.iter_mut().zip(password.as_bytes().iter().copied()) {
        *dst = src.reverse_bits();
    }

    let cipher = Des::new(GenericArray::from_slice(&key));
    let mut response = challenge;
    for block in response.chunks_exact_mut(8) {
        let block = GenericArray::from_mut_slice(block);
        cipher.encrypt_block(block);
    }
    response
}

/// RFB pixel format used for ServerInit and client-requested SetPixelFormat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VncPixelFormat {
    pub bpp: u8,
    pub depth: u8,
    pub big_endian: bool,
    pub true_color: bool,
    pub r_max: u16,
    pub g_max: u16,
    pub b_max: u16,
    pub r_shift: u8,
    pub g_shift: u8,
    pub b_shift: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VncEncoding {
    Raw,
    Hextile,
    Zlib,
    TightJpeg,
    Zrle,
}

impl VncEncoding {
    fn wire_value(self) -> i32 {
        match self {
            Self::Raw => ENCODING_RAW,
            Self::Hextile => ENCODING_HEXTILE,
            Self::Zlib => ENCODING_ZLIB,
            Self::TightJpeg => ENCODING_TIGHT,
            Self::Zrle => ENCODING_ZRLE,
        }
    }
}

impl VncPixelFormat {
    /// 32 bpp, little-endian, R<<16 | G<<8 | B => bytes [B,G,R,X].
    pub fn native() -> Self {
        Self {
            bpp: 32,
            depth: 24,
            big_endian: false,
            true_color: true,
            r_max: 255,
            g_max: 255,
            b_max: 255,
            r_shift: 16,
            g_shift: 8,
            b_shift: 0,
        }
    }

    /// 16 bpp, little-endian RGB565. Many VNC clients accept this as the
    /// server's preferred low-bandwidth format.
    pub fn rgb565() -> Self {
        Self {
            bpp: 16,
            depth: 16,
            big_endian: false,
            true_color: true,
            r_max: 31,
            g_max: 63,
            b_max: 31,
            r_shift: 11,
            g_shift: 5,
            b_shift: 0,
        }
    }

    /// Parse the 16-byte PIXEL_FORMAT structure sent by SetPixelFormat.
    fn parse(b: &[u8; 16]) -> Self {
        Self {
            bpp: b[0],
            depth: b[1],
            big_endian: b[2] != 0,
            true_color: b[3] != 0,
            r_max: u16::from_be_bytes([b[4], b[5]]),
            g_max: u16::from_be_bytes([b[6], b[7]]),
            b_max: u16::from_be_bytes([b[8], b[9]]),
            r_shift: b[10],
            g_shift: b[11],
            b_shift: b[12],
        }
    }

    fn write_to(self, out: &mut Vec<u8>) {
        out.extend_from_slice(&[
            self.bpp,
            self.depth,
            u8::from(self.big_endian),
            u8::from(self.true_color),
        ]);
        out.extend_from_slice(&self.r_max.to_be_bytes());
        out.extend_from_slice(&self.g_max.to_be_bytes());
        out.extend_from_slice(&self.b_max.to_be_bytes());
        out.extend_from_slice(&[self.r_shift, self.g_shift, self.b_shift, 0, 0, 0]);
    }

    fn bytes_per_pixel(self) -> usize {
        (self.bpp / 8).max(1) as usize
    }

    fn is_native(&self) -> bool {
        self.bpp == 32
            && self.depth == 24
            && !self.big_endian
            && self.true_color
            && self.r_max == 255
            && self.g_max == 255
            && self.b_max == 255
            && self.r_shift == 16
            && self.g_shift == 8
            && self.b_shift == 0
    }
}

/// Convert a top-down BGRA frame into `fmt`'s on-wire pixel encoding.
fn encode_rect(bgra: &[u8], width: usize, rect: Rect, fmt: &VncPixelFormat, out: &mut Vec<u8>) {
    out.clear();
    let x = rect.x as usize;
    let y = rect.y as usize;
    let w = rect.width as usize;
    let h = rect.height as usize;
    if fmt.is_native() {
        out.reserve(w * h * 4);
        for row in y..y + h {
            let start = (row * width + x) * 4;
            let end = start + w * 4;
            out.extend_from_slice(&bgra[start..end]);
        }
        return;
    }
    let nbytes = fmt.bytes_per_pixel();
    out.reserve(w * h * nbytes);
    for row in y..y + h {
        let start = (row * width + x) * 4;
        let end = start + w * 4;
        for px in bgra[start..end].chunks_exact(4) {
            encode_pixel(px, fmt, out);
        }
    }
}

fn encode_pixel(px: &[u8], fmt: &VncPixelFormat, out: &mut Vec<u8>) {
    let b = px[0] as u32;
    let g = px[1] as u32;
    let r = px[2] as u32;
    let rc = r * fmt.r_max as u32 / 255;
    let gc = g * fmt.g_max as u32 / 255;
    let bc = b * fmt.b_max as u32 / 255;
    let val = ((rc << fmt.r_shift) | (gc << fmt.g_shift) | (bc << fmt.b_shift)) as u64;
    let nbytes = fmt.bytes_per_pixel();
    if fmt.big_endian {
        for i in (0..nbytes).rev() {
            out.push((val >> (i * 8)) as u8);
        }
    } else {
        for i in 0..nbytes {
            out.push((val >> (i * 8)) as u8);
        }
    }
}

fn encode_compact_pixel(px: &[u8], fmt: &VncPixelFormat, out: &mut Vec<u8>) {
    let before = out.len();
    encode_pixel(px, fmt, out);
    if zrle_compact_pixel_bytes(fmt) == 3 && out.len() - before == 4 {
        if fmt.big_endian {
            out.remove(before);
        } else {
            out.pop();
        }
    }
}

fn zrle_compact_pixel_bytes(fmt: &VncPixelFormat) -> usize {
    if fmt.bpp == 32 && fmt.true_color && fmt.r_max <= 255 && fmt.g_max <= 255 && fmt.b_max <= 255 {
        3
    } else {
        fmt.bytes_per_pixel()
    }
}

fn encode_hextile_rect(
    bgra: &[u8],
    width: usize,
    rect: Rect,
    fmt: &VncPixelFormat,
    out: &mut Vec<u8>,
) {
    out.clear();
    let x0 = rect.x as usize;
    let y0 = rect.y as usize;
    let x1 = x0 + rect.width as usize;
    let y1 = y0 + rect.height as usize;
    let mut tile_pixels = Vec::new();

    for tile_y in (y0..y1).step_by(16) {
        for tile_x in (x0..x1).step_by(16) {
            let tile = Rect {
                x: tile_x as u16,
                y: tile_y as u16,
                width: (x1 - tile_x).min(16) as u16,
                height: (y1 - tile_y).min(16) as u16,
            };
            out.push(HEXTILE_RAW);
            encode_rect(bgra, width, tile, fmt, &mut tile_pixels);
            out.extend_from_slice(&tile_pixels);
        }
    }
}

fn encode_zlib_rect(
    bgra: &[u8],
    width: usize,
    rect: Rect,
    fmt: &VncPixelFormat,
    compressor: &mut Compress,
    out: &mut Vec<u8>,
) -> io::Result<()> {
    let mut raw = Vec::new();
    encode_rect(bgra, width, rect, fmt, &mut raw);
    encode_zlib_payload(&raw, compressor, out)
}

fn encode_zrle_rect(
    bgra: &[u8],
    width: usize,
    rect: Rect,
    fmt: &VncPixelFormat,
    compressor: &mut Compress,
    out: &mut Vec<u8>,
) -> io::Result<()> {
    let x0 = rect.x as usize;
    let y0 = rect.y as usize;
    let x1 = x0 + rect.width as usize;
    let y1 = y0 + rect.height as usize;
    let compact = zrle_compact_pixel_bytes(fmt);
    let mut raw = Vec::new();
    raw.reserve(rect.width as usize * rect.height as usize * compact + 64);

    for tile_y in (y0..y1).step_by(ZRLE_TILE_SIZE) {
        for tile_x in (x0..x1).step_by(ZRLE_TILE_SIZE) {
            let tile_w = (x1 - tile_x).min(ZRLE_TILE_SIZE);
            let tile_h = (y1 - tile_y).min(ZRLE_TILE_SIZE);
            raw.push(0);
            for row in tile_y..tile_y + tile_h {
                let start = (row * width + tile_x) * 4;
                let end = start + tile_w * 4;
                for px in bgra[start..end].chunks_exact(4) {
                    encode_compact_pixel(px, fmt, &mut raw);
                }
            }
        }
    }

    encode_zlib_payload(&raw, compressor, out)
}

fn encode_tight_jpeg_rect(
    bgra: &[u8],
    width: usize,
    rect: Rect,
    quality: u8,
    out: &mut Vec<u8>,
) -> io::Result<()> {
    let x = rect.x as usize;
    let y = rect.y as usize;
    let w = rect.width as usize;
    let h = rect.height as usize;
    let mut rgb = Vec::with_capacity(w * h * 3);
    for row in y..y + h {
        let start = (row * width + x) * 4;
        let end = start + w * 4;
        for px in bgra[start..end].chunks_exact(4) {
            rgb.push(px[2]);
            rgb.push(px[1]);
            rgb.push(px[0]);
        }
    }

    let mut jpeg = Vec::new();
    JpegEncoder::new(&mut jpeg, quality)
        .encode(&rgb, rect.width, rect.height, ColorType::Rgb)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    out.clear();
    out.push(TIGHT_JPEG);
    write_tight_compact_len(jpeg.len(), out);
    out.extend_from_slice(&jpeg);
    Ok(())
}

fn write_tight_compact_len(mut len: usize, out: &mut Vec<u8>) {
    let mut byte = (len & 0x7f) as u8;
    len >>= 7;
    if len != 0 {
        byte |= 0x80;
    }
    out.push(byte);
    if len == 0 {
        return;
    }

    byte = (len & 0x7f) as u8;
    len >>= 7;
    if len != 0 {
        byte |= 0x80;
    }
    out.push(byte);
    if len != 0 {
        out.push((len & 0xff) as u8);
    }
}

fn requested_tight_quality(requested: &[i32]) -> Option<u8> {
    requested
        .iter()
        .copied()
        .filter(|enc| {
            (ENCODING_TIGHT_QUALITY_LEVEL_0..=ENCODING_TIGHT_QUALITY_LEVEL_9).contains(enc)
        })
        .max()
        .map(|enc| {
            let level = enc - ENCODING_TIGHT_QUALITY_LEVEL_0;
            (10 + level * 10).clamp(1, 100) as u8
        })
}

fn split_tight_rects(rects: Vec<Rect>) -> Vec<Rect> {
    let mut out = Vec::new();
    for rect in rects {
        if rect.width <= TIGHT_MAX_RECT_WIDTH {
            out.push(rect);
            continue;
        }
        let mut x = rect.x;
        let mut remaining = rect.width;
        while remaining > 0 {
            let width = remaining.min(TIGHT_MAX_RECT_WIDTH);
            out.push(Rect {
                x,
                y: rect.y,
                width,
                height: rect.height,
            });
            x = x.saturating_add(width);
            remaining -= width;
        }
    }
    out
}

fn encode_zlib_payload(raw: &[u8], compressor: &mut Compress, out: &mut Vec<u8>) -> io::Result<()> {
    let mut compressed = Vec::with_capacity(raw.len().saturating_add(1024).max(128));
    let mut offset = 0usize;
    while offset < raw.len() {
        compressed.reserve(raw.len().saturating_sub(offset).saturating_add(1024));
        let before_in = compressor.total_in();
        compressor
            .compress_vec(&raw[offset..], &mut compressed, FlushCompress::None)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let consumed = (compressor.total_in() - before_in) as usize;
        if consumed == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "zlib compressor made no input progress",
            ));
        }
        offset += consumed;
    }
    compressed.reserve(1024);
    compressor
        .compress_vec(&[], &mut compressed, FlushCompress::Sync)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    out.clear();
    out.extend_from_slice(&(compressed.len() as u32).to_be_bytes());
    out.extend_from_slice(&compressed);
    Ok(())
}

fn handle_vnc_client(
    mut stream: TcpStream,
    frame: Arc<SharedFrame>,
    name: String,
    config: VncServerConfig,
    server_state: Arc<ServerState>,
    client_id: u64,
    peer: Option<SocketAddr>,
) -> io::Result<()> {
    stream.set_nodelay(true).ok();

    // ProtocolVersion: offer 3.8, honor whatever (<=) the client replies.
    stream.write_all(b"RFB 003.008\n")?;
    let mut client_ver = [0u8; 12];
    stream.read_exact(&mut client_ver)?;
    let minor = if client_ver.starts_with(b"RFB 003.") {
        match client_ver[10] {
            b'8' => 8u8,
            b'7' => 7,
            _ => 3,
        }
    } else {
        3
    };

    let input_callback = config.input_callback.clone();
    if minor >= 7 {
        let security_type = config.auth.security_type();
        stream.write_all(&[1u8, security_type])?;
        let mut chosen = [0u8; 1];
        stream.read_exact(&mut chosen)?;
        if chosen[0] != security_type {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "VNC client selected an unsupported security type",
            ));
        }
        match &config.auth {
            VncAuth::None => {}
            VncAuth::Password(password) => run_vnc_password_auth(&mut stream, password, true)?,
        }
        if minor >= 8 && matches!(config.auth, VncAuth::None) {
            stream.write_all(&0u32.to_be_bytes())?;
        }
    } else {
        match &config.auth {
            VncAuth::None => stream.write_all(&1u32.to_be_bytes())?,
            VncAuth::Password(password) => {
                stream.write_all(&2u32.to_be_bytes())?;
                run_vnc_password_auth(&mut stream, password, true)?;
            }
        }
    }
    println!("VNC client negotiated RFB 003.00{minor}");

    let mut shared = [0u8; 1];
    stream.read_exact(&mut shared)?;

    let (width, height) = {
        let inner = frame.inner.lock().unwrap();
        (inner.width, inner.height)
    };
    let mut init = Vec::with_capacity(24 + name.len());
    init.extend_from_slice(&width.to_be_bytes());
    init.extend_from_slice(&height.to_be_bytes());
    config.preferred_pixel_format.write_to(&mut init);
    init.extend_from_slice(&(name.len() as u32).to_be_bytes());
    init.extend_from_slice(name.as_bytes());
    stream.write_all(&init)?;

    let client_request = Arc::new((Mutex::new(None::<UpdateRequest>), Condvar::new()));
    let pixel_format = Arc::new(Mutex::new(config.preferred_pixel_format));
    let encoding = Arc::new(Mutex::new(VncEncoding::Raw));
    let tight_jpeg_quality = Arc::new(Mutex::new(config.tight_jpeg_quality));
    let configured_tight_jpeg_quality = config.tight_jpeg_quality;
    {
        let client_request = Arc::clone(&client_request);
        let pixel_format = Arc::clone(&pixel_format);
        let encoding = Arc::clone(&encoding);
        let tight_jpeg_quality = Arc::clone(&tight_jpeg_quality);
        let input_callback = input_callback.clone();
        let reader_state = Arc::clone(&server_state);
        let mut reader = stream.try_clone()?;
        thread::spawn(move || {
            let mut msg = [0u8; 1];
            while !reader_state.shutdown.load(Ordering::Acquire)
                && reader.read_exact(&mut msg).is_ok()
            {
                let ok = match msg[0] {
                    0 => {
                        let mut body = [0u8; 19];
                        if reader.read_exact(&mut body).is_err() {
                            false
                        } else {
                            let mut pf = [0u8; 16];
                            pf.copy_from_slice(&body[3..19]);
                            let parsed = VncPixelFormat::parse(&pf);
                            println!(
                                "VNC SetPixelFormat: {}bpp big_endian={} shifts r{} g{} b{} max {}/{}/{}{}",
                                parsed.bpp,
                                parsed.big_endian,
                                parsed.r_shift,
                                parsed.g_shift,
                                parsed.b_shift,
                                parsed.r_max,
                                parsed.g_max,
                                parsed.b_max,
                                if parsed.is_native() {
                                    " (native)"
                                } else {
                                    " (converting)"
                                },
                            );
                            *pixel_format.lock().unwrap() = parsed;
                            true
                        }
                    }
                    2 => {
                        let mut hdr = [0u8; 3];
                        if reader.read_exact(&mut hdr).is_err() {
                            false
                        } else {
                            let n = u16::from_be_bytes([hdr[1], hdr[2]]) as usize;
                            let mut raw = vec![0u8; n * 4];
                            if reader.read_exact(&mut raw).is_err() {
                                false
                            } else {
                                let mut requested = Vec::with_capacity(n);
                                for enc in raw.chunks_exact(4) {
                                    requested
                                        .push(i32::from_be_bytes([enc[0], enc[1], enc[2], enc[3]]));
                                }
                                let requested_quality = requested_tight_quality(&requested);
                                let selected = if requested.contains(&ENCODING_TIGHT)
                                    && requested_quality.is_some()
                                {
                                    *tight_jpeg_quality.lock().unwrap() =
                                        configured_tight_jpeg_quality
                                            .min(requested_quality.unwrap());
                                    VncEncoding::TightJpeg
                                } else if requested.contains(&ENCODING_ZRLE) {
                                    VncEncoding::Zrle
                                } else if requested.contains(&ENCODING_ZLIB) {
                                    VncEncoding::Zlib
                                } else if requested.contains(&ENCODING_HEXTILE) {
                                    VncEncoding::Hextile
                                } else {
                                    VncEncoding::Raw
                                };
                                println!(
                                    "VNC SetEncodings: selected {:?} from {:?}",
                                    selected, requested
                                );
                                *encoding.lock().unwrap() = selected;
                                true
                            }
                        }
                    }
                    3 => {
                        let mut body = [0u8; 9];
                        if reader.read_exact(&mut body).is_err() {
                            false
                        } else {
                            let request = UpdateRequest {
                                incremental: body[0] != 0,
                                rect: Rect {
                                    x: u16::from_be_bytes([body[1], body[2]]),
                                    y: u16::from_be_bytes([body[3], body[4]]),
                                    width: u16::from_be_bytes([body[5], body[6]]),
                                    height: u16::from_be_bytes([body[7], body[8]]),
                                },
                            };
                            let (lock, cond) = &*client_request;
                            *lock.lock().unwrap() = Some(request);
                            cond.notify_one();
                            true
                        }
                    }
                    4 => {
                        let mut body = [0u8; 7];
                        if reader.read_exact(&mut body).is_err() {
                            false
                        } else {
                            if let Some(cb) = &input_callback {
                                cb(VncInputEvent::Key {
                                    client_id,
                                    peer,
                                    down: body[0] != 0,
                                    key: u32::from_be_bytes([body[3], body[4], body[5], body[6]]),
                                });
                            }
                            true
                        }
                    }
                    5 => {
                        let mut body = [0u8; 5];
                        if reader.read_exact(&mut body).is_err() {
                            false
                        } else {
                            let button_mask = body[0];
                            let x = u16::from_be_bytes([body[1], body[2]]);
                            let y = u16::from_be_bytes([body[3], body[4]]);
                            if let Some(cursor) =
                                reader_state.cursors.lock().unwrap().get_mut(&client_id)
                            {
                                cursor.x = x;
                                cursor.y = y;
                                cursor.button_mask = button_mask;
                                cursor.position_known = true;
                            }
                            if let Some(cb) = &input_callback {
                                cb(VncInputEvent::Pointer {
                                    client_id,
                                    peer,
                                    button_mask,
                                    x,
                                    y,
                                });
                            }
                            true
                        }
                    }
                    6 => {
                        let mut hdr = [0u8; 7];
                        if reader.read_exact(&mut hdr).is_err() {
                            false
                        } else {
                            let len = u32::from_be_bytes([hdr[3], hdr[4], hdr[5], hdr[6]]) as usize;
                            let mut text = vec![0u8; len];
                            if reader.read_exact(&mut text).is_err() {
                                false
                            } else {
                                if let Some(cb) = &input_callback {
                                    cb(VncInputEvent::ClientCutText {
                                        client_id,
                                        peer,
                                        text,
                                    });
                                }
                                true
                            }
                        }
                    }
                    _ => false,
                };
                if !ok {
                    break;
                }
            }
        });
    }

    let mut last_seq = 0u64;
    let mut last_clipboard_seq = 0u64;
    let mut encoded: Vec<u8> = Vec::new();
    let mut zlib_compressor = Compress::new(Compression::fast(), true);
    let mut zrle_compressor = Compress::new(Compression::fast(), true);
    while !server_state.shutdown.load(Ordering::Acquire) {
        if let Some(text) = clipboard_since(&server_state, last_clipboard_seq) {
            last_clipboard_seq = text.0;
            write_server_cut_text(&mut stream, &text.1)?;
        }

        let Some(request) = ({
            let (lock, cond) = &*client_request;
            let mut request = lock.lock().unwrap();
            while request.is_none() && !server_state.shutdown.load(Ordering::Acquire) {
                let (next, _) = cond
                    .wait_timeout(request, Duration::from_millis(50))
                    .unwrap();
                request = next;
                if let Some(text) = clipboard_since(&server_state, last_clipboard_seq) {
                    last_clipboard_seq = text.0;
                    drop(request);
                    write_server_cut_text(&mut stream, &text.1)?;
                    request = lock.lock().unwrap();
                }
            }
            request.take()
        }) else {
            continue;
        };

        let request_rect = request.rect.intersect(Rect::full(width, height));
        let Some(request_rect) = request_rect else {
            write_empty_update(&mut stream)?;
            continue;
        };
        let (raw, rects) = loop {
            let mut inner = frame.inner.lock().unwrap();
            let rects = if request.incremental {
                while damage_since(&inner, last_seq).is_none()
                    && !server_state.shutdown.load(Ordering::Acquire)
                {
                    let (next, _) = frame
                        .cond
                        .wait_timeout(inner, Duration::from_millis(50))
                        .unwrap();
                    inner = next;
                }
                damage_since(&inner, last_seq).map(|rects| {
                    rects
                        .into_iter()
                        .filter_map(|r| r.intersect(request_rect))
                        .collect::<Vec<_>>()
                })
            } else {
                Some(vec![request_rect])
            };
            last_seq = inner.seq;
            if let Some(rects) = rects {
                if !rects.is_empty() {
                    break (inner.data.clone(), coalesce_rects(rects, width, height));
                }
            }
            if server_state.shutdown.load(Ordering::Acquire) {
                return Ok(());
            }
            while inner.seq == last_seq && !server_state.shutdown.load(Ordering::Acquire) {
                let (next, _) = frame
                    .cond
                    .wait_timeout(inner, Duration::from_millis(50))
                    .unwrap();
                inner = next;
            }
        };
        let fmt = *pixel_format.lock().unwrap();
        let encoding = *encoding.lock().unwrap();
        let jpeg_quality = *tight_jpeg_quality.lock().unwrap();
        let rects = if encoding == VncEncoding::TightJpeg {
            split_tight_rects(rects)
        } else {
            rects
        };
        write_update_start(&mut stream, rects.len())?;
        for rect in rects {
            match encoding {
                VncEncoding::Raw => encode_rect(&raw, width as usize, rect, &fmt, &mut encoded),
                VncEncoding::Hextile => {
                    encode_hextile_rect(&raw, width as usize, rect, &fmt, &mut encoded)
                }
                VncEncoding::Zlib => encode_zlib_rect(
                    &raw,
                    width as usize,
                    rect,
                    &fmt,
                    &mut zlib_compressor,
                    &mut encoded,
                )?,
                VncEncoding::TightJpeg => {
                    encode_tight_jpeg_rect(&raw, width as usize, rect, jpeg_quality, &mut encoded)?
                }
                VncEncoding::Zrle => encode_zrle_rect(
                    &raw,
                    width as usize,
                    rect,
                    &fmt,
                    &mut zrle_compressor,
                    &mut encoded,
                )?,
            }
            write_rect_header(&mut stream, rect, encoding)?;
            stream.write_all(&encoded)?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct UpdateRequest {
    incremental: bool,
    rect: Rect,
}

fn write_update_start(stream: &mut TcpStream, rect_count: usize) -> io::Result<()> {
    stream.write_all(&[0u8, 0u8])?;
    stream.write_all(&(rect_count as u16).to_be_bytes())
}

fn write_rect_header(stream: &mut TcpStream, rect: Rect, encoding: VncEncoding) -> io::Result<()> {
    stream.write_all(&rect.x.to_be_bytes())?;
    stream.write_all(&rect.y.to_be_bytes())?;
    stream.write_all(&rect.width.to_be_bytes())?;
    stream.write_all(&rect.height.to_be_bytes())?;
    stream.write_all(&encoding.wire_value().to_be_bytes())
}

fn write_empty_update(stream: &mut TcpStream) -> io::Result<()> {
    stream.write_all(&[0u8, 0u8])?;
    stream.write_all(&0u16.to_be_bytes())
}

fn write_server_cut_text(stream: &mut TcpStream, text: &[u8]) -> io::Result<()> {
    stream.write_all(&[3u8, 0, 0, 0])?;
    stream.write_all(&(text.len() as u32).to_be_bytes())?;
    stream.write_all(text)
}

fn clipboard_since(state: &ServerState, last_seq: u64) -> Option<(u64, Vec<u8>)> {
    let inner = state.clipboard.inner.lock().unwrap();
    (inner.seq != 0 && inner.seq > last_seq).then(|| (inner.seq, inner.text.clone()))
}

fn damage_since(inner: &FrameInner, last_seq: u64) -> Option<Vec<Rect>> {
    if inner.seq <= last_seq {
        return Some(Vec::new());
    }
    let first_seq = inner.damages.front().map(|d| d.seq)?;
    if last_seq < first_seq.saturating_sub(1) {
        return Some(vec![Rect::full(inner.width, inner.height)]);
    }
    let mut damage = Vec::new();
    for entry in inner.damages.iter().filter(|entry| entry.seq > last_seq) {
        damage.extend_from_slice(&entry.rects);
    }
    Some(damage)
}

fn coalesce_rects(mut rects: Vec<Rect>, width: u16, height: u16) -> Vec<Rect> {
    rects.retain(|r| !r.is_empty());
    if rects.is_empty() {
        return rects;
    }
    if rects.len() > MAX_UPDATE_RECTS {
        return vec![Rect::full(width, height)];
    }
    rects
}

fn compute_damage_rects(old: &[u8], new: &[u8], width: usize, height: usize) -> Vec<Rect> {
    if width == 0 || height == 0 {
        return Vec::new();
    }
    if old.len() != new.len() || new.len() != width * height * 4 {
        return vec![Rect::full(width as u16, height as u16)];
    }

    let mut rects = Vec::new();
    for tile_y in (0..height).step_by(DAMAGE_TILE_SIZE) {
        for tile_x in (0..width).step_by(DAMAGE_TILE_SIZE) {
            let tile_w = (width - tile_x).min(DAMAGE_TILE_SIZE);
            let tile_h = (height - tile_y).min(DAMAGE_TILE_SIZE);
            let mut changed = false;
            'rows: for row in tile_y..tile_y + tile_h {
                let start = (row * width + tile_x) * 4;
                let end = start + tile_w * 4;
                if old[start..end] != new[start..end] {
                    changed = true;
                    break 'rows;
                }
            }
            if changed {
                rects.push(Rect {
                    x: tile_x as u16,
                    y: tile_y as u16,
                    width: tile_w as u16,
                    height: tile_h as u16,
                });
            }
        }
    }
    rects
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn damage_rect_is_none_for_identical_frames() {
        let frame = vec![0u8; 4 * 3 * 4];
        assert_eq!(compute_damage_rects(&frame, &frame, 4, 3), Vec::new());
    }

    #[test]
    fn damage_rects_track_changed_tiles() {
        let old = vec![0u8; 64 * 64 * 4];
        let mut new = old.clone();
        new[(1 * 64 + 2) * 4] = 1;
        new[(40 * 64 + 40) * 4] = 1;
        assert_eq!(
            compute_damage_rects(&old, &new, 64, 64),
            vec![
                Rect {
                    x: 0,
                    y: 0,
                    width: 32,
                    height: 32,
                },
                Rect {
                    x: 32,
                    y: 32,
                    width: 32,
                    height: 32,
                }
            ]
        );
    }

    #[test]
    fn damage_rect_is_full_for_size_mismatch() {
        assert_eq!(
            compute_damage_rects(&[0; 4], &[0; 8], 2, 1),
            vec![Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 1,
            }]
        );
    }

    #[test]
    fn vnc_password_uses_only_first_8_bytes() {
        let challenge = [0x5au8; 16];
        assert_eq!(
            vnc_password_response("password-extra", challenge),
            vnc_password_response("password", challenge)
        );
    }

    #[test]
    fn vnc_password_changes_response() {
        let challenge = [0x33u8; 16];
        assert_ne!(
            vnc_password_response("password", challenge),
            vnc_password_response("passwore", challenge)
        );
    }

    #[test]
    fn hextile_raw_tiles_include_subencoding_bytes() {
        let frame = vec![0u8; 20 * 20 * 4];
        let mut out = Vec::new();
        encode_hextile_rect(
            &frame,
            20,
            Rect {
                x: 0,
                y: 0,
                width: 20,
                height: 20,
            },
            &VncPixelFormat::native(),
            &mut out,
        );
        // 20x20 splits into 4 tiles: 16x16, 4x16, 16x4, 4x4.
        assert_eq!(out.len(), 4 + 20 * 20 * 4);
        assert_eq!(out[0], HEXTILE_RAW);
        assert_eq!(out[1 + 16 * 16 * 4], HEXTILE_RAW);
    }

    #[test]
    fn compressed_encodings_write_length_prefixed_payloads() {
        let frame = vec![0u8; 8 * 8 * 4];
        let rect = Rect {
            x: 0,
            y: 0,
            width: 8,
            height: 8,
        };
        let mut compressor = Compress::new(Compression::fast(), true);
        let mut out = Vec::new();
        encode_zlib_rect(
            &frame,
            8,
            rect,
            &VncPixelFormat::rgb565(),
            &mut compressor,
            &mut out,
        )
        .unwrap();
        assert!(out.len() > 4);
        let len = u32::from_be_bytes([out[0], out[1], out[2], out[3]]) as usize;
        assert_eq!(len, out.len() - 4);

        let mut compressor = Compress::new(Compression::fast(), true);
        encode_zrle_rect(
            &frame,
            8,
            rect,
            &VncPixelFormat::rgb565(),
            &mut compressor,
            &mut out,
        )
        .unwrap();
        assert!(out.len() > 4);
        let len = u32::from_be_bytes([out[0], out[1], out[2], out[3]]) as usize;
        assert_eq!(len, out.len() - 4);
    }

    #[test]
    fn zlib_encoding_handles_full_demo_frame() {
        let frame = vec![0u8; 800 * 480 * 4];
        let rect = Rect {
            x: 0,
            y: 0,
            width: 800,
            height: 480,
        };
        let mut compressor = Compress::new(Compression::fast(), true);
        let mut out = Vec::new();
        encode_zlib_rect(
            &frame,
            800,
            rect,
            &VncPixelFormat::rgb565(),
            &mut compressor,
            &mut out,
        )
        .unwrap();
        assert!(out.len() > 4);
    }

    #[test]
    fn tight_jpeg_writes_control_and_compact_length() {
        let frame = vec![0u8; 16 * 16 * 4];
        let rect = Rect {
            x: 0,
            y: 0,
            width: 16,
            height: 16,
        };
        let mut out = Vec::new();
        encode_tight_jpeg_rect(&frame, 16, rect, 70, &mut out).unwrap();
        assert_eq!(out[0], TIGHT_JPEG);
        let (len, used) = read_test_tight_compact_len(&out[1..]);
        assert_eq!(len, out.len() - 1 - used);
        assert!(out[1 + used..].starts_with(&[0xff, 0xd8]));
    }

    #[test]
    fn tight_rectangles_are_split_at_protocol_width_limit() {
        let rects = split_tight_rects(vec![Rect {
            x: 10,
            y: 20,
            width: 5000,
            height: 30,
        }]);
        assert_eq!(rects.len(), 3);
        assert_eq!(rects[0].width, 2048);
        assert_eq!(rects[1].x, 2058);
        assert_eq!(rects[1].width, 2048);
        assert_eq!(rects[2].x, 4106);
        assert_eq!(rects[2].width, 904);
    }

    fn read_test_tight_compact_len(bytes: &[u8]) -> (usize, usize) {
        let b0 = bytes[0];
        let mut len = (b0 & 0x7f) as usize;
        if b0 & 0x80 == 0 {
            return (len, 1);
        }
        let b1 = bytes[1];
        len |= ((b1 & 0x7f) as usize) << 7;
        if b1 & 0x80 == 0 {
            return (len, 2);
        }
        (len | ((bytes[2] as usize) << 14), 3)
    }

    #[test]
    fn input_helpers_decode_buttons_wheel_and_text() {
        let event = VncInputEvent::Pointer {
            client_id: 7,
            peer: None,
            button_mask: VncMouseButton::Left.mask() | VncMouseButton::WheelUp.mask(),
            x: 10,
            y: 20,
        };
        assert_eq!(event.client_id(), 7);
        assert_eq!(event.pointer_position(), Some((10, 20)));
        assert!(event.is_button_down(VncMouseButton::Left));
        assert_eq!(event.wheel_delta(VncMouseButton::Left.mask()), 1);

        let key = VncInputEvent::Key {
            client_id: 7,
            peer: None,
            down: true,
            key: b'a' as u32,
        };
        assert_eq!(key.key(), Some(VncKey::Character('a')));
        assert_eq!(key.text(), Some('a'));
    }
}
