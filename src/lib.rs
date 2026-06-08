use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    Arc, Condvar, Mutex,
};
use std::thread;

use des::cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit};
use des::Des;

const DAMAGE_HISTORY_LIMIT: usize = 128;

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

    fn union(self, other: Self) -> Self {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let x0 = self.x.min(other.x);
        let y0 = self.y.min(other.y);
        let x1 = self.x.saturating_add(self.width).max(other.x.saturating_add(other.width));
        let y1 = self.y.saturating_add(self.height).max(other.y.saturating_add(other.height));
        Self {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub enum VncInputEvent {
    Key {
        down: bool,
        key: u32,
    },
    Pointer {
        button_mask: u8,
        x: u16,
        y: u16,
    },
    ClientCutText(Vec<u8>),
}

pub type VncInputCallback = Arc<dyn Fn(VncInputEvent) + Send + Sync + 'static>;

#[derive(Clone, Default)]
pub struct VncServerConfig {
    pub input_callback: Option<VncInputCallback>,
    pub auth: VncAuth,
}

impl VncServerConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_input_callback(mut self, callback: VncInputCallback) -> Self {
        self.input_callback = Some(callback);
        self
    }

    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.auth = VncAuth::Password(password.into());
        self
    }
}

#[derive(Clone, Default)]
pub enum VncAuth {
    #[default]
    None,
    Password(String),
}

impl VncAuth {
    fn security_type(&self) -> u8 {
        match self {
            Self::None => 1,
            Self::Password(_) => 2,
        }
    }
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

#[derive(Clone, Copy)]
struct FrameDamage {
    seq: u64,
    rect: Option<Rect>,
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
        let damage = compute_damage_rect(
            &inner.data,
            frame,
            inner.width as usize,
            inner.height as usize,
        );
        inner.data.clear();
        inner.data.extend_from_slice(frame);
        inner.seq = inner.seq.wrapping_add(1);
        let seq = inner.seq;
        inner.damages.push_back(FrameDamage { seq, rect: damage });
        while inner.damages.len() > DAMAGE_HISTORY_LIMIT {
            inner.damages.pop_front();
        }
        drop(inner);
        self.cond.notify_all();
    }
}

/// Start a minimal RFB 3.8 (VNC) server on `addr`.
///
/// Each client is served Raw-encoded frames. Use [`VncServerConfig`] to enable
/// input callbacks and optional VNC password authentication.
///
/// Returns once the listener is bound; client handling happens on spawned
/// threads.
pub fn start_vnc_server(
    addr: String,
    frame: Arc<SharedFrame>,
    name: String,
    config: VncServerConfig,
) -> io::Result<()> {
    let listener = TcpListener::bind(&addr)?;
    println!("VNC server listening on {addr}");
    thread::Builder::new()
        .name("vnc-listener".to_string())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let peer = stream
                            .peer_addr()
                            .map(|a| a.to_string())
                            .unwrap_or_else(|_| "?".to_string());
                        println!("VNC client connected: {peer}");
                        let frame = Arc::clone(&frame);
                        let name = name.clone();
                        let config = config.clone();
                        thread::spawn(move || {
                            if let Err(e) =
                                handle_vnc_client(stream, frame, name, config)
                            {
                                println!("VNC client {peer} disconnected: {e}");
                            } else {
                                println!("VNC client {peer} disconnected");
                            }
                        });
                    }
                    Err(e) => eprintln!("VNC accept error: {e}"),
                }
            }
        })?;
    Ok(())
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

/// A client's requested RFB pixel format. Defaults to our native layout, which
/// matches the Bgra8 readback byte order exactly (zero-copy fast path).
#[derive(Clone, Copy)]
struct VncPixelFormat {
    bpp: u8,
    big_endian: bool,
    r_max: u16,
    g_max: u16,
    b_max: u16,
    r_shift: u8,
    g_shift: u8,
    b_shift: u8,
}

impl VncPixelFormat {
    /// 32 bpp, little-endian, R<<16 | G<<8 | B => bytes [B,G,R,X].
    fn native() -> Self {
        Self {
            bpp: 32,
            big_endian: false,
            r_max: 255,
            g_max: 255,
            b_max: 255,
            r_shift: 16,
            g_shift: 8,
            b_shift: 0,
        }
    }

    /// Parse the 16-byte PIXEL_FORMAT structure sent by SetPixelFormat.
    fn parse(b: &[u8; 16]) -> Self {
        Self {
            bpp: b[0],
            big_endian: b[2] != 0,
            r_max: u16::from_be_bytes([b[4], b[5]]),
            g_max: u16::from_be_bytes([b[6], b[7]]),
            b_max: u16::from_be_bytes([b[8], b[9]]),
            r_shift: b[10],
            g_shift: b[11],
            b_shift: b[12],
        }
    }

    fn is_native(&self) -> bool {
        self.bpp == 32
            && !self.big_endian
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
    let nbytes = (fmt.bpp / 8).max(1) as usize;
    out.reserve(w * h * nbytes);
    for row in y..y + h {
        let start = (row * width + x) * 4;
        let end = start + w * 4;
        for px in bgra[start..end].chunks_exact(4) {
            let b = px[0] as u32;
            let g = px[1] as u32;
            let r = px[2] as u32;
            let rc = r * fmt.r_max as u32 / 255;
            let gc = g * fmt.g_max as u32 / 255;
            let bc = b * fmt.b_max as u32 / 255;
            let val = ((rc << fmt.r_shift) | (gc << fmt.g_shift) | (bc << fmt.b_shift)) as u64;
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
    }
}

fn handle_vnc_client(
    mut stream: TcpStream,
    frame: Arc<SharedFrame>,
    name: String,
    config: VncServerConfig,
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
    init.extend_from_slice(&[
        32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0,
    ]);
    init.extend_from_slice(&(name.len() as u32).to_be_bytes());
    init.extend_from_slice(name.as_bytes());
    stream.write_all(&init)?;

    let client_request = Arc::new((Mutex::new(None::<UpdateRequest>), Condvar::new()));
    let pixel_format = Arc::new(Mutex::new(VncPixelFormat::native()));
    {
        let client_request = Arc::clone(&client_request);
        let pixel_format = Arc::clone(&pixel_format);
        let input_callback = input_callback.clone();
        let mut reader = stream.try_clone()?;
        thread::spawn(move || {
            let mut msg = [0u8; 1];
            while reader.read_exact(&mut msg).is_ok() {
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
                                if parsed.is_native() { " (native)" } else { " (converting)" },
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
                            read_exact_discard(&mut reader, n * 4)
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
                            if let Some(cb) = &input_callback {
                                cb(VncInputEvent::Pointer {
                                    button_mask: body[0],
                                    x: u16::from_be_bytes([body[1], body[2]]),
                                    y: u16::from_be_bytes([body[3], body[4]]),
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
                            let len =
                                u32::from_be_bytes([hdr[3], hdr[4], hdr[5], hdr[6]]) as usize;
                            let mut text = vec![0u8; len];
                            if reader.read_exact(&mut text).is_err() {
                                false
                            } else {
                                if let Some(cb) = &input_callback {
                                    cb(VncInputEvent::ClientCutText(text));
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
    let mut encoded: Vec<u8> = Vec::new();
    loop {
        let request = {
            let (lock, cond) = &*client_request;
            let mut request = lock.lock().unwrap();
            while request.is_none() {
                request = cond.wait(request).unwrap();
            }
            request.take().unwrap()
        };

        let request_rect = request.rect.intersect(Rect::full(width, height));
        let Some(request_rect) = request_rect else {
            write_empty_update(&mut stream)?;
            continue;
        };
        let (raw, rect) = loop {
            let mut inner = frame.inner.lock().unwrap();
            let rect = if request.incremental {
                while damage_since(&inner, last_seq).is_none() {
                    inner = frame.cond.wait(inner).unwrap();
                }
                damage_since(&inner, last_seq)
                    .flatten()
                    .and_then(|r| r.intersect(request_rect))
            } else {
                Some(request_rect)
            };
            last_seq = inner.seq;
            if let Some(rect) = rect {
                break (inner.data.clone(), rect);
            }
            while inner.seq == last_seq {
                inner = frame.cond.wait(inner).unwrap();
            }
        };
        let fmt = *pixel_format.lock().unwrap();
        encode_rect(&raw, width as usize, rect, &fmt, &mut encoded);
        write_update_header(&mut stream, rect)?;
        stream.write_all(&encoded)?;
    }
}

#[derive(Clone, Copy)]
struct UpdateRequest {
    incremental: bool,
    rect: Rect,
}

fn write_update_header(stream: &mut TcpStream, rect: Rect) -> io::Result<()> {
    stream.write_all(&[0u8, 0u8])?;
    stream.write_all(&1u16.to_be_bytes())?;
    stream.write_all(&rect.x.to_be_bytes())?;
    stream.write_all(&rect.y.to_be_bytes())?;
    stream.write_all(&rect.width.to_be_bytes())?;
    stream.write_all(&rect.height.to_be_bytes())?;
    stream.write_all(&0i32.to_be_bytes())
}

fn write_empty_update(stream: &mut TcpStream) -> io::Result<()> {
    stream.write_all(&[0u8, 0u8])?;
    stream.write_all(&0u16.to_be_bytes())
}

fn damage_since(inner: &FrameInner, last_seq: u64) -> Option<Option<Rect>> {
    if inner.seq <= last_seq {
        return Some(None);
    }
    let first_seq = inner.damages.front().map(|d| d.seq)?;
    if last_seq < first_seq.saturating_sub(1) {
        return Some(Some(Rect::full(inner.width, inner.height)));
    }
    let mut damage = None;
    for entry in inner.damages.iter().filter(|entry| entry.seq > last_seq) {
        if let Some(rect) = entry.rect {
            damage = Some(damage.map_or(rect, |acc: Rect| acc.union(rect)));
        }
    }
    Some(damage)
}

fn compute_damage_rect(old: &[u8], new: &[u8], width: usize, height: usize) -> Option<Rect> {
    if width == 0 || height == 0 {
        return None;
    }
    if old.len() != new.len() || new.len() != width * height * 4 {
        return Some(Rect::full(width as u16, height as u16));
    }

    let mut min_x = width;
    let mut min_y = height;
    let mut max_x = 0usize;
    let mut max_y = 0usize;
    let mut changed = false;
    for y in 0..height {
        let row_start = y * width * 4;
        let old_row = &old[row_start..row_start + width * 4];
        let new_row = &new[row_start..row_start + width * 4];
        if old_row == new_row {
            continue;
        }
        let mut row_min = 0usize;
        while row_min < width {
            let i = row_min * 4;
            if old_row[i..i + 4] != new_row[i..i + 4] {
                break;
            }
            row_min += 1;
        }
        let mut row_max = width - 1;
        while row_max > row_min {
            let i = row_max * 4;
            if old_row[i..i + 4] != new_row[i..i + 4] {
                break;
            }
            row_max -= 1;
        }
        changed = true;
        min_x = min_x.min(row_min);
        min_y = min_y.min(y);
        max_x = max_x.max(row_max);
        max_y = max_y.max(y);
    }

    if changed {
        Some(Rect {
            x: min_x as u16,
            y: min_y as u16,
            width: (max_x - min_x + 1) as u16,
            height: (max_y - min_y + 1) as u16,
        })
    } else {
        None
    }
}

fn read_exact_discard<R: Read>(reader: &mut R, mut n: usize) -> bool {
    let mut scratch = [0u8; 1024];
    while n > 0 {
        let chunk = n.min(scratch.len());
        if reader.read_exact(&mut scratch[..chunk]).is_err() {
            return false;
        }
        n -= chunk;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn damage_rect_is_none_for_identical_frames() {
        let frame = vec![0u8; 4 * 3 * 4];
        assert_eq!(compute_damage_rect(&frame, &frame, 4, 3), None);
    }

    #[test]
    fn damage_rect_bounds_changed_pixels() {
        let old = vec![0u8; 4 * 3 * 4];
        let mut new = old.clone();
        new[(1 * 4 + 2) * 4] = 1;
        new[(2 * 4 + 3) * 4] = 1;
        assert_eq!(
            compute_damage_rect(&old, &new, 4, 3),
            Some(Rect {
                x: 2,
                y: 1,
                width: 2,
                height: 2,
            })
        );
    }

    #[test]
    fn damage_rect_is_full_for_size_mismatch() {
        assert_eq!(
            compute_damage_rect(&[0; 4], &[0; 8], 2, 1),
            Some(Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 1,
            })
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
}

