// Minimal RFB client probe to validate the server's framing/encoding.
// Usage: vnc_probe [--host HOST] [--port PORT] [--bpp MODE] [--passwd PASS]
//   bpp omitted -> use server's native format (no SetPixelFormat)
//   bpp = 16    -> send a RealVNC-style 16bpp 565 SetPixelFormat
//   bpp = 32rgbx-> send a 32bpp RGBX (non-native shift) SetPixelFormat
//   bpp = hextile -> request Hextile encoding
// Reads several continuous frames, checks each FramebufferUpdate stays aligned,
// and writes the last frame to %TEMP%\vnc_probe.bmp.

use std::env;
use std::io::{self, Read, Write};
use std::net::TcpStream;

use des::Des;
use des::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};

fn read_exact(s: &mut TcpStream, n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf).expect("read_exact");
    buf
}

fn main() {
    let opts = ProbeCli::parse().unwrap_or_else(|e| {
        eprintln!("{e}");
        print_probe_help();
        std::process::exit(2);
    });
    if opts.help {
        print_probe_help();
        return;
    }

    let mut s = TcpStream::connect((opts.host.as_str(), opts.port)).expect("connect");
    s.set_nodelay(true).ok();

    let ver = read_exact(&mut s, 12);
    println!("server version: {}", String::from_utf8_lossy(&ver).trim());
    s.write_all(b"RFB 003.008\n").unwrap();

    let sec = read_exact(&mut s, 2);
    println!("sec count={} type={}", sec[0], sec[1]);
    let chosen = if opts.passwd.is_some() { 2u8 } else { 1u8 };
    if sec[1] != chosen {
        panic!(
            "server offered security type {}, but probe wants {}",
            sec[1], chosen
        );
    }
    s.write_all(&[chosen]).unwrap();
    if chosen == 2 {
        let challenge = read_exact(&mut s, 16);
        let mut challenge_arr = [0u8; 16];
        challenge_arr.copy_from_slice(&challenge);
        let response = vnc_password_response(opts.passwd.as_deref().unwrap(), challenge_arr);
        s.write_all(&response).unwrap();
    }
    let secres = read_exact(&mut s, 4);
    println!("SecurityResult={:?}", secres);
    assert_eq!(secres, [0, 0, 0, 0], "security failed");

    s.write_all(&[1u8]).unwrap(); // ClientInit shared

    let init = read_exact(&mut s, 24);
    let w = u16::from_be_bytes([init[0], init[1]]);
    let h = u16::from_be_bytes([init[2], init[3]]);
    let namelen = u32::from_be_bytes([init[20], init[21], init[22], init[23]]) as usize;
    let name = read_exact(&mut s, namelen);
    println!(
        "ServerInit {}x{} server-bpp={} depth={} name='{}'",
        w,
        h,
        init[4],
        init[5],
        String::from_utf8_lossy(&name)
    );

    // Optionally override pixel format like a real client.
    let bytespp: usize = match opts.mode.as_str() {
        "16" => {
            // 16bpp 565: bpp=16 depth=16 BE=0 TC=1 rmax=31 gmax=63 bmax=31 rs=11 gs=5 bs=0
            let pf = [16u8, 16, 0, 1, 0, 31, 0, 63, 0, 31, 11, 5, 0, 0, 0, 0];
            let mut msg = vec![0u8, 0, 0, 0];
            msg.extend_from_slice(&pf);
            s.write_all(&msg).unwrap();
            println!("sent SetPixelFormat 16bpp 565");
            2
        }
        "32rgbx" => {
            // 32bpp with RGBX layout: rs=0 gs=8 bs=16 (non-native).
            let pf = [32u8, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0];
            let mut msg = vec![0u8, 0, 0, 0];
            msg.extend_from_slice(&pf);
            s.write_all(&msg).unwrap();
            println!("sent SetPixelFormat 32bpp RGBX");
            4
        }
        "hextile" | _ => {
            println!("using server native format (32bpp)");
            4
        }
    };
    if opts.mode == "hextile" {
        let mut msg = vec![2u8, 0, 0, 2];
        msg.extend_from_slice(&5i32.to_be_bytes());
        msg.extend_from_slice(&0i32.to_be_bytes());
        s.write_all(&msg).unwrap();
        println!("sent SetEncodings Hextile, Raw");
    }

    // One initial (non-incremental) FramebufferUpdateRequest.
    let mut req = vec![3u8, 0, 0, 0, 0, 0];
    req.extend_from_slice(&w.to_be_bytes());
    req.extend_from_slice(&h.to_be_bytes());
    s.write_all(&req).unwrap();

    let frames = 5;
    let mut last_pixels = Vec::new();
    for f in 0..frames {
        let hdr = read_exact(&mut s, 4);
        if hdr[0] != 0 {
            println!(
                "FRAME {f}: MISALIGNED! first byte = {} (expected 0)",
                hdr[0]
            );
            return;
        }
        let nrect = u16::from_be_bytes([hdr[2], hdr[3]]);
        let mut total = 0usize;
        for _ in 0..nrect {
            let r = read_exact(&mut s, 12);
            let rw = u16::from_be_bytes([r[4], r[5]]) as usize;
            let rh = u16::from_be_bytes([r[6], r[7]]) as usize;
            let enc = i32::from_be_bytes([r[8], r[9], r[10], r[11]]);
            let px = if enc == 5 {
                read_hextile_rect(&mut s, rw, rh, bytespp)
            } else {
                read_exact(&mut s, rw * rh * bytespp)
            };
            let bytes = px.len();
            total += bytes;
            let sum: u64 = px.iter().map(|&b| b as u64).sum();
            println!("FRAME {f}: rect {rw}x{rh} enc={enc} bytes={bytes} checksum={sum}");
            if f == frames - 1 {
                last_pixels = px;
            }
        }
        println!("FRAME {f}: aligned ok, {nrect} rect(s), {total} pixel bytes");
        // Send an incremental request like a real client (server ignores it).
        let mut ireq = vec![3u8, 1, 0, 0, 0, 0];
        ireq.extend_from_slice(&w.to_be_bytes());
        ireq.extend_from_slice(&h.to_be_bytes());
        let _ = s.write_all(&ireq);
    }

    // Dump last frame to BMP (only meaningful for 32bpp BGRX native).
    if bytespp == 4 {
        let path = std::env::temp_dir().join("vnc_probe.bmp");
        write_bmp(&path, &last_pixels, w as u32, h as u32);
        println!("wrote {}", path.display());
    }
}

struct ProbeCli {
    host: String,
    port: u16,
    mode: String,
    passwd: Option<String>,
    help: bool,
}

impl ProbeCli {
    fn parse() -> io::Result<Self> {
        let mut out = Self {
            host: "127.0.0.1".to_string(),
            port: 5901,
            mode: String::new(),
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
                "--bpp" | "--mode" | "--encoding" => {
                    out.mode = args.next().ok_or_else(|| missing_value("--bpp"))?;
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
                _ if arg.starts_with("--bpp=") => {
                    out.mode = arg["--bpp=".len()..].to_string();
                }
                _ if arg.starts_with("--mode=") => {
                    out.mode = arg["--mode=".len()..].to_string();
                }
                _ if arg.starts_with("--encoding=") => {
                    out.mode = arg["--encoding=".len()..].to_string();
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
                        format!("unknown option: {arg}"),
                    ));
                }
                _ => positional.push(arg),
            }
        }
        if let Some(port) = positional.first() {
            out.port = parse_port(port)?;
        }
        if let Some(mode) = positional.get(1) {
            out.mode = mode.clone();
        }
        if let Some(passwd) = positional.get(2) {
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

fn print_probe_help() {
    println!(
        "Usage: cargo run --example vnc_probe -- [OPTIONS]\n\nOptions:\n  --host HOST          Connect host/address (default: 127.0.0.1)\n  --port PORT          Connect port (default: 5901)\n  --bpp MODE           Pixel/encoding mode: native, 16, 32rgbx, hextile\n  --encoding MODE      Alias for --bpp\n  --passwd PASS        VNC password\n  --password PASS      Alias for --passwd\n  -h, --help           Show this help\n\nBackward-compatible positional form:\n  cargo run --example vnc_probe -- [port] [bpp] [password]\n"
    );
}

fn read_hextile_rect(s: &mut TcpStream, width: usize, height: usize, bytespp: usize) -> Vec<u8> {
    let mut pixels = vec![0u8; width * height * bytespp];
    for tile_y in (0..height).step_by(16) {
        for tile_x in (0..width).step_by(16) {
            let tile_w = (width - tile_x).min(16);
            let tile_h = (height - tile_y).min(16);
            let subencoding = read_exact(s, 1)[0];
            assert_eq!(subencoding & 1, 1, "probe only supports raw Hextile tiles");
            let tile = read_exact(s, tile_w * tile_h * bytespp);
            for row in 0..tile_h {
                let dst = ((tile_y + row) * width + tile_x) * bytespp;
                let src = row * tile_w * bytespp;
                pixels[dst..dst + tile_w * bytespp]
                    .copy_from_slice(&tile[src..src + tile_w * bytespp]);
            }
        }
    }
    pixels
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

fn write_bmp(path: &std::path::Path, bgrx: &[u8], width: u32, height: u32) {
    let w = width as usize;
    let h = height as usize;
    let row = (w * 3 + 3) & !3;
    let data = row * h;
    let mut buf = Vec::with_capacity(54 + data);
    buf.extend_from_slice(b"BM");
    buf.extend_from_slice(&((54 + data) as u32).to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&54u32.to_le_bytes());
    buf.extend_from_slice(&40u32.to_le_bytes());
    buf.extend_from_slice(&(width as i32).to_le_bytes());
    buf.extend_from_slice(&(height as i32).to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&24u16.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&(data as u32).to_le_bytes());
    buf.extend_from_slice(&2835u32.to_le_bytes());
    buf.extend_from_slice(&2835u32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    let pad = row - w * 3;
    for y in (0..h).rev() {
        let line = &bgrx[y * w * 4..(y + 1) * w * 4];
        for px in line.chunks_exact(4) {
            buf.push(px[0]);
            buf.push(px[1]);
            buf.push(px[2]);
        }
        buf.extend(std::iter::repeat_n(0u8, pad));
    }
    std::fs::write(path, &buf).unwrap();
}
