//! A small RFB 3.8 (VNC) client: enough to grab the framebuffer of a Lume VM
//! and to send pointer and keyboard events to it.
//!
//! Lume runs a VNC server next to every VM it starts with VNC enabled, bound
//! to 127.0.0.1 with a random password, and reports it as
//! `vnc://:password@127.0.0.1:port`. The server speaks RFB 3.8 with VNC
//! authentication (a DES challenge). This client negotiates nothing after
//! the handshake: Lume's server (0.7.2) drops the connection and takes the
//! VM down with it when a client sends `SetEncodings`, so the client keeps
//! the server's native pixel format (32-bit true colour, as it happens) and
//! the Raw encoding every server must support unasked, and requests one
//! full, non-incremental update per screenshot. The framebuffer is the
//! Retina-scaled display (1920x1440 for a 1024x768 display); frames larger
//! than [`MAX_SCREENSHOT_SIDE`] on their long side are box-filtered down by
//! an integer factor, and input coordinates, given in screenshot pixels, are
//! scaled back up by the same factor ([`scale_factor`]). Input is a `PointerEvent` per movement, press and
//! release and a `KeyEvent` per key, with X11 keysyms from
//! [`warren_core::screen`]. One connection per request keeps the state
//! simple; the handshake costs about a millisecond on the loopback.

use std::time::Duration;

use des::{
    Des,
    cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
};
use warren_core::{
    Error, Result,
    screen::{InputAction, Modifier, key_press_for_char, parse_chord},
};

/// Time to connect and complete the handshake.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Time for the server to deliver a full frame.
pub const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);
/// Time for every event of one `input` request to be written (a full
/// `type` of [`warren_core::screen::MAX_TEXT_CHARS`] characters fits).
pub const INPUT_TIMEOUT: Duration = Duration::from_secs(120);
/// Pause between a press and its release, and between typed characters, so
/// the guest's window server sees distinct events.
pub const EVENT_GAP: Duration = Duration::from_millis(20);
/// Pause after pressing a modifier and after the key of a chord, before
/// anything is released: Lume's own client holds chords for 300 ms.
pub const CHORD_GAP: Duration = Duration::from_millis(60);
/// Pause between the clicks of a double or triple click.
pub const CLICK_GAP: Duration = Duration::from_millis(80);

/// X11 keysyms of the modifier keys.
pub const KEYSYM_SHIFT_L: u32 = 0xffe1;
pub const KEYSYM_CONTROL_L: u32 = 0xffe3;
pub const KEYSYM_ALT_L: u32 = 0xffe9;
pub const KEYSYM_META_L: u32 = 0xffe7;
pub const KEYSYM_SUPER_L: u32 = 0xffeb;

/// The keysym sent for the Command modifier. Apple's VNC server follows the
/// Apple Remote Desktop convention: `Alt_L` is Command and `Meta_L` is
/// Option (verified against a Lume VM: `Alt_L`+space opens Spotlight,
/// `Meta_L`+a types `å`; `Super_L` and `Hyper_L` are ignored as modifiers).
pub const COMMAND_KEYSYM: u32 = KEYSYM_ALT_L;
/// The keysym sent for the Option modifier (see [`COMMAND_KEYSYM`]).
pub const OPTION_KEYSYM: u32 = KEYSYM_META_L;

/// Longest side of a screenshot before the frame is downscaled by an
/// integer factor (a 1920x1440 framebuffer becomes 960x720).
pub const MAX_SCREENSHOT_SIDE: u32 = 1280;

/// The integer factor a framebuffer of this size is downscaled by, and
/// input coordinates are multiplied by: 1 up to [`MAX_SCREENSHOT_SIDE`],
/// then the smallest factor that fits.
pub fn scale_factor(width: u32, height: u32) -> u32 {
    width.max(height).div_ceil(MAX_SCREENSHOT_SIDE).max(1)
}

/// Wheel "buttons" in a `PointerEvent` mask: up, down, left, right.
const WHEEL_UP: u8 = 1 << 3;
const WHEEL_DOWN: u8 = 1 << 4;
const WHEEL_LEFT: u8 = 1 << 5;
const WHEEL_RIGHT: u8 = 1 << 6;

const ENCODING_RAW: i32 = 0;
const ENCODING_COPY_RECT: i32 = 1;

/// Decoded `vnc://[user[:password]@]host:port`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub password: String,
}

/// Parse the URL Lume reports. The password may be percent-encoded.
pub fn parse_url(url: &str) -> Result<Endpoint> {
    let rest = url
        .trim()
        .strip_prefix("vnc://")
        .ok_or_else(|| Error::internal(format!("VNC URL {url:?} does not start with vnc://")))?;
    let rest = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let (userinfo, hostport) = match rest.rfind('@') {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => ("", rest),
    };
    let password = match userinfo.find(':') {
        Some(i) => percent_decode(&userinfo[i + 1..]),
        None => String::new(),
    };
    let (host, port) = if let Some(end) = hostport.strip_prefix('[').and_then(|h| h.find(']')) {
        // `[::1]:5900`: `end` indexes the bracket within the stripped text.
        let host = &hostport[1..=end];
        let port = hostport[end + 2..].strip_prefix(':').unwrap_or("");
        (host.to_string(), port)
    } else {
        match hostport.rfind(':') {
            Some(i) => (hostport[..i].to_string(), &hostport[i + 1..]),
            None => (hostport.to_string(), ""),
        }
    };
    let port: u16 = port
        .parse()
        .map_err(|_| Error::internal(format!("VNC URL {url:?} has no valid port")))?;
    if host.is_empty() {
        return Err(Error::internal(format!("VNC URL {url:?} has no host")));
    }
    Ok(Endpoint {
        host,
        port,
        password,
    })
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The VNC authentication answer: the 16-byte challenge encrypted with DES
/// in ECB mode under the password, truncated or zero-padded to 8 bytes with
/// every byte bit-reversed (the RFB quirk).
pub fn auth_response(password: &str, challenge: &[u8; 16]) -> [u8; 16] {
    let mut key = [0u8; 8];
    for (k, b) in key.iter_mut().zip(password.bytes()) {
        *k = b.reverse_bits();
    }
    let cipher = Des::new(GenericArray::from_slice(&key));
    let mut out = [0u8; 16];
    for (i, chunk) in challenge.chunks(8).enumerate() {
        let mut block = GenericArray::clone_from_slice(chunk);
        cipher.encrypt_block(&mut block);
        out[i * 8..i * 8 + 8].copy_from_slice(&block);
    }
    out
}

/// A true-colour pixel format (RFB `PIXEL_FORMAT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelFormat {
    pub bits_per_pixel: u8,
    pub depth: u8,
    pub big_endian: bool,
    pub true_colour: bool,
    pub red_max: u16,
    pub green_max: u16,
    pub blue_max: u16,
    pub red_shift: u8,
    pub green_shift: u8,
    pub blue_shift: u8,
}

impl PixelFormat {
    /// 32 bits per pixel, 8 bits per channel, red in bits 16..24 (little
    /// endian: bytes B, G, R, pad). What the client asks the server for.
    pub const RGBX32: PixelFormat = PixelFormat {
        bits_per_pixel: 32,
        depth: 24,
        big_endian: false,
        true_colour: true,
        red_max: 255,
        green_max: 255,
        blue_max: 255,
        red_shift: 16,
        green_shift: 8,
        blue_shift: 0,
    };

    pub fn encode(&self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0] = self.bits_per_pixel;
        b[1] = self.depth;
        b[2] = u8::from(self.big_endian);
        b[3] = u8::from(self.true_colour);
        b[4..6].copy_from_slice(&self.red_max.to_be_bytes());
        b[6..8].copy_from_slice(&self.green_max.to_be_bytes());
        b[8..10].copy_from_slice(&self.blue_max.to_be_bytes());
        b[10] = self.red_shift;
        b[11] = self.green_shift;
        b[12] = self.blue_shift;
        b
    }

    pub fn decode(b: &[u8; 16]) -> PixelFormat {
        PixelFormat {
            bits_per_pixel: b[0],
            depth: b[1],
            big_endian: b[2] != 0,
            true_colour: b[3] != 0,
            red_max: u16::from_be_bytes([b[4], b[5]]),
            green_max: u16::from_be_bytes([b[6], b[7]]),
            blue_max: u16::from_be_bytes([b[8], b[9]]),
            red_shift: b[10],
            green_shift: b[11],
            blue_shift: b[12],
        }
    }

    pub fn bytes_per_pixel(&self) -> usize {
        usize::from(self.bits_per_pixel.max(8)) / 8
    }

    /// The RGB value of one pixel in this format.
    pub fn rgb(&self, px: &[u8]) -> [u8; 3] {
        let mut v: u32 = 0;
        if self.big_endian {
            for b in px {
                v = (v << 8) | u32::from(*b);
            }
        } else {
            for b in px.iter().rev() {
                v = (v << 8) | u32::from(*b);
            }
        }
        let chan = |shift: u8, max: u16| -> u8 {
            if max == 0 {
                return 0;
            }
            let c = (v >> shift) & u32::from(max);
            ((c * 255 + u32::from(max) / 2) / u32::from(max)) as u8
        };
        [
            chan(self.red_shift, self.red_max),
            chan(self.green_shift, self.green_max),
            chan(self.blue_shift, self.blue_max),
        ]
    }
}

/// A captured frame: RGB, row-major, three bytes per pixel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

impl Frame {
    /// The frame reduced by an integer `factor` with a box filter (each
    /// output pixel averages a factor x factor block); `factor <= 1` returns
    /// the frame unchanged. Trailing rows and columns that do not fill a
    /// block are dropped.
    pub fn downscale(self, factor: u32) -> Frame {
        if factor <= 1 || self.width < factor || self.height < factor {
            return self;
        }
        let f = factor as usize;
        let (w, h) = (self.width as usize, self.height as usize);
        let (ow, oh) = (w / f, h / f);
        let mut rgb = vec![0u8; ow * oh * 3];
        let area = (f * f) as u32;
        for oy in 0..oh {
            for ox in 0..ow {
                let mut sum = [0u32; 3];
                for dy in 0..f {
                    let row = (oy * f + dy) * w;
                    for dx in 0..f {
                        let i = (row + ox * f + dx) * 3;
                        sum[0] += u32::from(self.rgb[i]);
                        sum[1] += u32::from(self.rgb[i + 1]);
                        sum[2] += u32::from(self.rgb[i + 2]);
                    }
                }
                let o = (oy * ow + ox) * 3;
                rgb[o] = ((sum[0] + area / 2) / area) as u8;
                rgb[o + 1] = ((sum[1] + area / 2) / area) as u8;
                rgb[o + 2] = ((sum[2] + area / 2) / area) as u8;
            }
        }
        Frame {
            width: ow as u32,
            height: oh as u32,
            rgb,
        }
    }
}

/// Encode a frame as an 8-bit RGB PNG.
pub fn encode_png(frame: &Frame) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(frame.rgb.len() / 4);
    let mut enc = png::Encoder::new(&mut out, frame.width, frame.height);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    let mut w = enc
        .write_header()
        .map_err(|e| Error::internal(format!("png header: {e}")))?;
    w.write_image_data(&frame.rgb)
        .map_err(|e| Error::internal(format!("png data: {e}")))?;
    w.finish()
        .map_err(|e| Error::internal(format!("png finish: {e}")))?;
    Ok(out)
}

/// What the client negotiates after the handshake. The defaults send
/// nothing, which is what Lume's server survives (see the module notes);
/// the knobs exist for probing other servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConnectOptions {
    /// Send `SetPixelFormat` asking for [`PixelFormat::RGBX32`]; otherwise
    /// keep the server's native format.
    pub set_pixel_format: bool,
    /// Send `SetEncodings` (CopyRect, Raw); otherwise rely on Raw, which
    /// every server must support without being asked.
    pub set_encodings: bool,
    /// Ask for an incremental update first (servers that refuse a full,
    /// non-incremental request still answer the first incremental one with
    /// the whole screen).
    pub incremental_first: bool,
}

/// One connection to a VNC server, past the handshake.
pub struct VncClient {
    stream: BufReader<TcpStream>,
    width: u16,
    height: u16,
    name: String,
    format: PixelFormat,
    server_format: PixelFormat,
    endpoint: Endpoint,
    opts: ConnectOptions,
    event_gap: Duration,
}

impl VncClient {
    /// Connect to the server at `url`, authenticate and negotiate the pixel
    /// format and encodings. Bounded by [`CONNECT_TIMEOUT`].
    pub async fn connect(url: &str) -> Result<VncClient> {
        Self::connect_with(url, ConnectOptions::default()).await
    }

    /// [`VncClient::connect`] with explicit negotiation options.
    pub async fn connect_with(url: &str, opts: ConnectOptions) -> Result<VncClient> {
        let endpoint = parse_url(url)?;
        tokio::time::timeout(CONNECT_TIMEOUT, Self::connect_to(endpoint.clone(), opts))
            .await
            .map_err(|_| {
                Error::internal(format!(
                    "VNC server {}:{} did not complete the handshake within {}s",
                    endpoint.host,
                    endpoint.port,
                    CONNECT_TIMEOUT.as_secs()
                ))
            })?
    }

    async fn connect_to(endpoint: Endpoint, opts: ConnectOptions) -> Result<VncClient> {
        let what = format!("{}:{}", endpoint.host, endpoint.port);
        let tcp = TcpStream::connect((endpoint.host.as_str(), endpoint.port))
            .await
            .map_err(|e| Error::internal(format!("connect to VNC server {what}: {e}")))?;
        tcp.set_nodelay(true).ok();
        let mut stream = BufReader::new(tcp);
        let io = |e: std::io::Error| Error::internal(format!("VNC {what}: {e}"));
        let at = |phase: &'static str| {
            let what = what.clone();
            move |e: std::io::Error| Error::internal(format!("VNC {what}: {phase}: {e}"))
        };

        // ProtocolVersion.
        let mut version = [0u8; 12];
        stream
            .read_exact(&mut version)
            .await
            .map_err(at("reading the greeting"))?;
        let text = String::from_utf8_lossy(&version);
        let minor: u32 = text
            .strip_prefix("RFB 003.")
            .and_then(|v| v.trim_end().parse().ok())
            .ok_or_else(|| Error::internal(format!("VNC {what}: unexpected greeting {text:?}")))?;
        if minor < 7 {
            return Err(Error::internal(format!(
                "VNC {what}: protocol {} is too old (3.7 or newer needed)",
                text.trim()
            )));
        }
        let minor = minor.min(8);
        stream
            .write_all(format!("RFB 003.00{minor}\n").as_bytes())
            .await
            .map_err(at("sending the version"))?;

        // Security.
        let count = stream
            .read_u8()
            .await
            .map_err(at("reading the security types"))?;
        if count == 0 {
            let reason = read_string(&mut stream).await?;
            return Err(Error::internal(format!(
                "VNC {what} refused the connection: {reason}"
            )));
        }
        let mut types = vec![0u8; usize::from(count)];
        stream.read_exact(&mut types).await.map_err(io)?;
        let chosen = if types.contains(&2) && !endpoint.password.is_empty() {
            2
        } else if types.contains(&1) {
            1
        } else if types.contains(&2) {
            return Err(Error::internal(format!(
                "VNC {what} wants a password and the URL has none"
            )));
        } else {
            return Err(Error::internal(format!(
                "VNC {what} offers no security type this client speaks ({types:?})"
            )));
        };
        stream
            .write_u8(chosen)
            .await
            .map_err(at("choosing the security type"))?;
        if chosen == 2 {
            let mut challenge = [0u8; 16];
            stream
                .read_exact(&mut challenge)
                .await
                .map_err(at("reading the challenge"))?;
            stream
                .write_all(&auth_response(&endpoint.password, &challenge))
                .await
                .map_err(at("answering the challenge"))?;
        }
        if chosen == 2 || minor >= 8 {
            let result = stream
                .read_u32()
                .await
                .map_err(at("reading the security result"))?;
            if result != 0 {
                let reason = if minor >= 8 {
                    read_string(&mut stream).await.unwrap_or_default()
                } else {
                    String::new()
                };
                return Err(Error::internal(format!(
                    "VNC {what} rejected the password{}{reason}",
                    if reason.is_empty() { "" } else { ": " }
                )));
            }
        }

        // ClientInit (shared) and ServerInit.
        stream.write_u8(1).await.map_err(at("sending ClientInit"))?;
        let width = stream.read_u16().await.map_err(at("reading ServerInit"))?;
        let height = stream.read_u16().await.map_err(at("reading ServerInit"))?;
        let mut pf = [0u8; 16];
        stream
            .read_exact(&mut pf)
            .await
            .map_err(at("reading ServerInit"))?;
        let name = read_string(&mut stream).await?;
        if width == 0 || height == 0 {
            return Err(Error::internal(format!(
                "VNC {what} reports an empty framebuffer ({width}x{height})"
            )));
        }

        // SetPixelFormat, then SetEncodings.
        let server_format = PixelFormat::decode(&pf);
        let format = if opts.set_pixel_format {
            let mut msg = vec![0u8, 0, 0, 0];
            msg.extend_from_slice(&PixelFormat::RGBX32.encode());
            stream
                .write_all(&msg)
                .await
                .map_err(at("sending SetPixelFormat"))?;
            PixelFormat::RGBX32
        } else {
            server_format
        };
        if opts.set_encodings {
            let mut msg = vec![2u8, 0];
            msg.extend_from_slice(&2u16.to_be_bytes());
            msg.extend_from_slice(&ENCODING_COPY_RECT.to_be_bytes());
            msg.extend_from_slice(&ENCODING_RAW.to_be_bytes());
            stream
                .write_all(&msg)
                .await
                .map_err(at("sending SetEncodings"))?;
        }
        stream.flush().await.map_err(io)?;
        tracing::debug!(server = %what, width, height, name = %name, ?server_format, "vnc connected");
        Ok(VncClient {
            stream,
            width,
            height,
            name,
            format,
            server_format,
            endpoint,
            opts,
            event_gap: EVENT_GAP,
        })
    }

    pub fn width(&self) -> u32 {
        u32::from(self.width)
    }

    pub fn height(&self) -> u32 {
        u32::from(self.height)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The pixel format the server announced in `ServerInit`.
    pub fn server_format(&self) -> PixelFormat {
        self.server_format
    }

    /// Change the pause between key and button events (default
    /// [`EVENT_GAP`]).
    pub fn set_event_gap(&mut self, gap: Duration) {
        self.event_gap = gap;
    }

    fn io(&self, e: std::io::Error) -> Error {
        Error::internal(format!(
            "VNC {}:{}: {e}",
            self.endpoint.host, self.endpoint.port
        ))
    }

    /// Request one full update and assemble the frame from the rectangles the
    /// server sends. Bounded by [`CAPTURE_TIMEOUT`]; a server that stops
    /// short of the whole screen yields a frame with the rest black.
    pub async fn capture(&mut self) -> Result<Frame> {
        tokio::time::timeout(CAPTURE_TIMEOUT, self.capture_inner())
            .await
            .map_err(|_| {
                Error::internal(format!(
                    "VNC {}:{} did not deliver a frame within {}s",
                    self.endpoint.host,
                    self.endpoint.port,
                    CAPTURE_TIMEOUT.as_secs()
                ))
            })?
    }

    async fn capture_inner(&mut self) -> Result<Frame> {
        let (w, h) = (usize::from(self.width), usize::from(self.height));
        let mut rgb = vec![0u8; w * h * 3];
        let mut covered = vec![false; w * h];
        let mut covered_count = 0usize;
        self.request_update(self.opts.incremental_first).await?;
        let mut idle_updates = 0;
        while covered_count < w * h {
            let kind = self.stream.read_u8().await.map_err(|e| self.io(e))?;
            match kind {
                0 => {
                    let before = covered_count;
                    self.read_framebuffer_update(&mut rgb, &mut covered, &mut covered_count)
                        .await?;
                    if covered_count == before {
                        idle_updates += 1;
                        if idle_updates >= 2 {
                            tracing::debug!(
                                covered = covered_count,
                                total = w * h,
                                "vnc server sent no new pixels twice; keeping the partial frame"
                            );
                            break;
                        }
                        self.request_update(false).await?;
                    }
                }
                1 => {
                    // SetColourMapEntries: padding, first colour, count, then
                    // six bytes per colour.
                    let mut head = [0u8; 5];
                    self.stream
                        .read_exact(&mut head)
                        .await
                        .map_err(|e| self.io(e))?;
                    let n = usize::from(u16::from_be_bytes([head[3], head[4]]));
                    let mut skip = vec![0u8; n * 6];
                    self.stream
                        .read_exact(&mut skip)
                        .await
                        .map_err(|e| self.io(e))?;
                }
                2 => {}
                3 => {
                    let mut pad = [0u8; 3];
                    self.stream
                        .read_exact(&mut pad)
                        .await
                        .map_err(|e| self.io(e))?;
                    let n = self.stream.read_u32().await.map_err(|e| self.io(e))? as usize;
                    let mut skip = vec![0u8; n];
                    self.stream
                        .read_exact(&mut skip)
                        .await
                        .map_err(|e| self.io(e))?;
                }
                other => {
                    return Err(Error::internal(format!(
                        "VNC server sent message type {other}, which this client does not know"
                    )));
                }
            }
        }
        Ok(Frame {
            width: self.width(),
            height: self.height(),
            rgb,
        }
        .downscale(self.scale()))
    }

    /// The factor between screenshot pixels and framebuffer pixels.
    pub fn scale(&self) -> u32 {
        scale_factor(self.width(), self.height())
    }

    async fn request_update(&mut self, incremental: bool) -> Result<()> {
        let mut msg = vec![3u8, u8::from(incremental)];
        msg.extend_from_slice(&0u16.to_be_bytes());
        msg.extend_from_slice(&0u16.to_be_bytes());
        msg.extend_from_slice(&self.width.to_be_bytes());
        msg.extend_from_slice(&self.height.to_be_bytes());
        self.stream.write_all(&msg).await.map_err(|e| self.io(e))?;
        self.stream.flush().await.map_err(|e| self.io(e))
    }

    /// The body of a `FramebufferUpdate` (the type byte is consumed).
    async fn read_framebuffer_update(
        &mut self,
        rgb: &mut [u8],
        covered: &mut [bool],
        covered_count: &mut usize,
    ) -> Result<()> {
        let (w, h) = (usize::from(self.width), usize::from(self.height));
        let _pad = self.stream.read_u8().await.map_err(|e| self.io(e))?;
        let rects = self.stream.read_u16().await.map_err(|e| self.io(e))?;
        for _ in 0..rects {
            let mut head = [0u8; 12];
            self.stream
                .read_exact(&mut head)
                .await
                .map_err(|e| self.io(e))?;
            let x = usize::from(u16::from_be_bytes([head[0], head[1]]));
            let y = usize::from(u16::from_be_bytes([head[2], head[3]]));
            let rw = usize::from(u16::from_be_bytes([head[4], head[5]]));
            let rh = usize::from(u16::from_be_bytes([head[6], head[7]]));
            let encoding = i32::from_be_bytes([head[8], head[9], head[10], head[11]]);
            if x + rw > w || y + rh > h {
                return Err(Error::internal(format!(
                    "VNC rectangle {rw}x{rh} at {x},{y} lies outside the {w}x{h} framebuffer"
                )));
            }
            match encoding {
                ENCODING_RAW => {
                    let bpp = self.format.bytes_per_pixel();
                    let mut row = vec![0u8; rw * bpp];
                    for ry in 0..rh {
                        self.stream
                            .read_exact(&mut row)
                            .await
                            .map_err(|e| self.io(e))?;
                        let base = (y + ry) * w + x;
                        for rx in 0..rw {
                            let px = self.format.rgb(&row[rx * bpp..(rx + 1) * bpp]);
                            let i = base + rx;
                            rgb[i * 3..i * 3 + 3].copy_from_slice(&px);
                            if !covered[i] {
                                covered[i] = true;
                                *covered_count += 1;
                            }
                        }
                    }
                }
                ENCODING_COPY_RECT => {
                    let sx = usize::from(self.stream.read_u16().await.map_err(|e| self.io(e))?);
                    let sy = usize::from(self.stream.read_u16().await.map_err(|e| self.io(e))?);
                    if sx + rw > w || sy + rh > h {
                        return Err(Error::internal(
                            "VNC CopyRect source lies outside the framebuffer",
                        ));
                    }
                    let snapshot = rgb.to_vec();
                    for ry in 0..rh {
                        let src = ((sy + ry) * w + sx) * 3;
                        let dst = ((y + ry) * w + x) * 3;
                        rgb[dst..dst + rw * 3].copy_from_slice(&snapshot[src..src + rw * 3]);
                        for rx in 0..rw {
                            let i = (y + ry) * w + x + rx;
                            if !covered[i] {
                                covered[i] = true;
                                *covered_count += 1;
                            }
                        }
                    }
                }
                other => {
                    return Err(Error::internal(format!(
                        "VNC server used encoding {other}, which this client did not ask for"
                    )));
                }
            }
        }
        Ok(())
    }

    /// `PointerEvent`: the pointer at (x, y) with `mask` buttons held.
    pub async fn pointer(&mut self, x: u16, y: u16, mask: u8) -> Result<()> {
        let mut msg = vec![5u8, mask];
        msg.extend_from_slice(&x.to_be_bytes());
        msg.extend_from_slice(&y.to_be_bytes());
        self.stream.write_all(&msg).await.map_err(|e| self.io(e))?;
        self.stream.flush().await.map_err(|e| self.io(e))
    }

    /// `KeyEvent` for one keysym.
    pub async fn key(&mut self, keysym: u32, down: bool) -> Result<()> {
        let mut msg = vec![4u8, u8::from(down), 0, 0];
        msg.extend_from_slice(&keysym.to_be_bytes());
        self.stream.write_all(&msg).await.map_err(|e| self.io(e))?;
        self.stream.flush().await.map_err(|e| self.io(e))
    }

    async fn tap(&mut self, keysym: u32) -> Result<()> {
        self.key(keysym, true).await?;
        tokio::time::sleep(self.event_gap).await;
        self.key(keysym, false).await?;
        tokio::time::sleep(self.event_gap).await;
        Ok(())
    }

    /// Screenshot coordinates to framebuffer coordinates, clamped to the
    /// screen.
    fn clamp(&self, x: u32, y: u32) -> (u16, u16) {
        let f = self.scale();
        (
            x.saturating_mul(f).min(u32::from(self.width) - 1) as u16,
            y.saturating_mul(f).min(u32::from(self.height) - 1) as u16,
        )
    }

    /// Perform one action. Coordinates are screenshot pixels (see
    /// [`scale_factor`]); beyond the screen they are clamped to its edge.
    pub async fn perform(&mut self, action: &InputAction) -> Result<()> {
        match action {
            InputAction::Click {
                x,
                y,
                button,
                count,
            } => {
                let (x, y) = self.clamp(*x, *y);
                self.pointer(x, y, 0).await?;
                tokio::time::sleep(self.event_gap).await;
                for i in 0..*count {
                    self.pointer(x, y, button.rfb_mask()).await?;
                    tokio::time::sleep(self.event_gap).await;
                    self.pointer(x, y, 0).await?;
                    if i + 1 < *count {
                        tokio::time::sleep(CLICK_GAP).await;
                    }
                }
            }
            InputAction::Move { x, y } => {
                let (x, y) = self.clamp(*x, *y);
                self.pointer(x, y, 0).await?;
            }
            InputAction::Scroll { x, y, dx, dy } => {
                let (x, y) = self.clamp(*x, *y);
                self.pointer(x, y, 0).await?;
                let vertical = if *dy > 0 { WHEEL_DOWN } else { WHEEL_UP };
                for _ in 0..dy.unsigned_abs() {
                    self.pointer(x, y, vertical).await?;
                    self.pointer(x, y, 0).await?;
                    tokio::time::sleep(self.event_gap).await;
                }
                let horizontal = if *dx > 0 { WHEEL_RIGHT } else { WHEEL_LEFT };
                for _ in 0..dx.unsigned_abs() {
                    self.pointer(x, y, horizontal).await?;
                    self.pointer(x, y, 0).await?;
                    tokio::time::sleep(self.event_gap).await;
                }
            }
            InputAction::Type { text } => {
                for c in text.chars() {
                    let (keysym, shifted) = key_press_for_char(c);
                    if shifted {
                        self.key(KEYSYM_SHIFT_L, true).await?;
                        tokio::time::sleep(self.event_gap).await;
                    }
                    self.tap(keysym).await?;
                    if shifted {
                        self.key(KEYSYM_SHIFT_L, false).await?;
                        tokio::time::sleep(self.event_gap).await;
                    }
                }
            }
            InputAction::Key { key } => {
                let chord = parse_chord(key)?;
                for m in &chord.modifiers {
                    self.key(modifier_keysym(*m), true).await?;
                    tokio::time::sleep(CHORD_GAP).await;
                }
                self.key(chord.keysym, true).await?;
                tokio::time::sleep(CHORD_GAP).await;
                self.key(chord.keysym, false).await?;
                tokio::time::sleep(self.event_gap).await;
                for m in chord.modifiers.iter().rev() {
                    self.key(modifier_keysym(*m), false).await?;
                    tokio::time::sleep(self.event_gap).await;
                }
            }
        }
        Ok(())
    }

    /// Perform every action in order, bounded by [`INPUT_TIMEOUT`].
    pub async fn perform_all(&mut self, actions: &[InputAction]) -> Result<()> {
        tokio::time::timeout(INPUT_TIMEOUT, async {
            for a in actions {
                self.perform(a).await?;
            }
            Ok(())
        })
        .await
        .map_err(|_| {
            Error::internal(format!(
                "VNC input did not complete within {}s",
                INPUT_TIMEOUT.as_secs()
            ))
        })?
    }

    /// Close the connection (best effort).
    pub async fn close(mut self) {
        let _ = self.stream.shutdown().await;
    }
}

/// The keysym held for a chord modifier.
pub fn modifier_keysym(m: Modifier) -> u32 {
    match m {
        Modifier::Shift => KEYSYM_SHIFT_L,
        Modifier::Control => KEYSYM_CONTROL_L,
        Modifier::Option => OPTION_KEYSYM,
        Modifier::Command => COMMAND_KEYSYM,
    }
}

/// A u32-length-prefixed string (reason text, desktop name).
async fn read_string(stream: &mut BufReader<TcpStream>) -> Result<String> {
    let n = stream
        .read_u32()
        .await
        .map_err(|e| Error::internal(format!("VNC: {e}")))? as usize;
    if n > 1 << 20 {
        return Err(Error::internal(format!(
            "VNC string of {n} bytes is absurd"
        )));
    }
    let mut buf = vec![0u8; n];
    stream
        .read_exact(&mut buf)
        .await
        .map_err(|e| Error::internal(format!("VNC: {e}")))?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// A fake RFB server for tests in this crate.
#[cfg(test)]
pub(crate) mod fake {
    use std::sync::{Arc, Mutex};

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::{ENCODING_COPY_RECT, ENCODING_RAW, PixelFormat, auth_response};

    /// What the fake server saw from the client.
    #[derive(Default, Debug)]
    pub struct Seen {
        pub pixel_format: Option<PixelFormat>,
        pub encodings: Vec<i32>,
        pub updates: Vec<(bool, u16, u16, u16, u16)>,
        pub keys: Vec<(u32, bool)>,
        pub pointer: Vec<(u16, u16, u8)>,
        pub auth_ok: bool,
    }

    /// An RFB 3.8 server with VNC authentication and a 4x3 framebuffer
    /// whose top row is red and the two below green (row 1 sent Raw, row 2
    /// as a CopyRect of row 1), preceded by a Bell, so both encodings and
    /// an unrelated message are exercised. Serves any number of connections.
    pub async fn server(password: &'static str, seen: Arc<Mutex<Seen>>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((s, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(serve_one(s, password, seen.clone()));
            }
        });
        port
    }

    async fn serve_one(mut s: tokio::net::TcpStream, password: &str, seen: Arc<Mutex<Seen>>) {
        s.write_all(b"RFB 003.008\n").await.unwrap();
        let mut v = [0u8; 12];
        s.read_exact(&mut v).await.unwrap();
        assert_eq!(&v, b"RFB 003.008\n");
        s.write_all(&[1u8, 2]).await.unwrap();
        assert_eq!(s.read_u8().await.unwrap(), 2);
        let challenge = [0x5au8; 16];
        s.write_all(&challenge).await.unwrap();
        let mut resp = [0u8; 16];
        s.read_exact(&mut resp).await.unwrap();
        let ok = resp == auth_response(password, &challenge);
        seen.lock().unwrap().auth_ok = ok;
        if !ok {
            s.write_u32(1).await.unwrap();
            let reason = b"wrong password";
            s.write_u32(reason.len() as u32).await.unwrap();
            s.write_all(reason).await.unwrap();
            return;
        }
        s.write_u32(0).await.unwrap();
        assert_eq!(s.read_u8().await.unwrap(), 1, "shared flag");
        s.write_u16(4).await.unwrap();
        s.write_u16(3).await.unwrap();
        // Advertise a big-endian format the client will replace.
        let mut native = PixelFormat::RGBX32;
        native.big_endian = true;
        s.write_all(&native.encode()).await.unwrap();
        s.write_u32(4).await.unwrap();
        s.write_all(b"fake").await.unwrap();
        let mut format = native;
        loop {
            let Ok(kind) = s.read_u8().await else {
                return;
            };
            match kind {
                0 => {
                    let mut b = [0u8; 19];
                    s.read_exact(&mut b).await.unwrap();
                    let mut pf = [0u8; 16];
                    pf.copy_from_slice(&b[3..]);
                    format = PixelFormat::decode(&pf);
                    seen.lock().unwrap().pixel_format = Some(format);
                }
                2 => {
                    s.read_u8().await.unwrap();
                    let n = s.read_u16().await.unwrap();
                    let mut encs = Vec::new();
                    for _ in 0..n {
                        encs.push(s.read_i32().await.unwrap());
                    }
                    seen.lock().unwrap().encodings = encs;
                }
                3 => {
                    let inc = s.read_u8().await.unwrap() != 0;
                    let x = s.read_u16().await.unwrap();
                    let y = s.read_u16().await.unwrap();
                    let w = s.read_u16().await.unwrap();
                    let h = s.read_u16().await.unwrap();
                    seen.lock().unwrap().updates.push((inc, x, y, w, h));
                    // Bell first, then the update.
                    s.write_u8(2).await.unwrap();
                    let px = |r: u8, g: u8, b: u8| -> Vec<u8> {
                        let v = (u32::from(r) << format.red_shift)
                            | (u32::from(g) << format.green_shift)
                            | (u32::from(b) << format.blue_shift);
                        if format.big_endian {
                            v.to_be_bytes().to_vec()
                        } else {
                            v.to_le_bytes().to_vec()
                        }
                    };
                    let mut msg = vec![0u8, 0];
                    msg.extend_from_slice(&3u16.to_be_bytes());
                    for (y, (r, g, b)) in [(0u16, (255, 0, 0)), (1u16, (0, 255, 0))] {
                        msg.extend_from_slice(&0u16.to_be_bytes());
                        msg.extend_from_slice(&y.to_be_bytes());
                        msg.extend_from_slice(&4u16.to_be_bytes());
                        msg.extend_from_slice(&1u16.to_be_bytes());
                        msg.extend_from_slice(&ENCODING_RAW.to_be_bytes());
                        for _ in 0..4 {
                            msg.extend_from_slice(&px(r, g, b));
                        }
                    }
                    msg.extend_from_slice(&0u16.to_be_bytes());
                    msg.extend_from_slice(&2u16.to_be_bytes());
                    msg.extend_from_slice(&4u16.to_be_bytes());
                    msg.extend_from_slice(&1u16.to_be_bytes());
                    msg.extend_from_slice(&ENCODING_COPY_RECT.to_be_bytes());
                    msg.extend_from_slice(&0u16.to_be_bytes());
                    msg.extend_from_slice(&1u16.to_be_bytes());
                    s.write_all(&msg).await.unwrap();
                }
                4 => {
                    let down = s.read_u8().await.unwrap() != 0;
                    let mut pad = [0u8; 2];
                    s.read_exact(&mut pad).await.unwrap();
                    let keysym = s.read_u32().await.unwrap();
                    seen.lock().unwrap().keys.push((keysym, down));
                }
                5 => {
                    let mask = s.read_u8().await.unwrap();
                    let x = s.read_u16().await.unwrap();
                    let y = s.read_u16().await.unwrap();
                    seen.lock().unwrap().pointer.push((x, y, mask));
                }
                other => panic!("unexpected client message {other}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use warren_core::screen::MouseButton;

    use super::{
        fake::{Seen, server as fake_server},
        *,
    };

    #[test]
    fn urls_parse() {
        let e = parse_url("vnc://:tulip-soft-falcon-light@127.0.0.1:64882").unwrap();
        assert_eq!(
            e,
            Endpoint {
                host: "127.0.0.1".into(),
                port: 64882,
                password: "tulip-soft-falcon-light".into()
            }
        );
        let e = parse_url("vnc://user:p%40ss@localhost:5900/").unwrap();
        assert_eq!(
            (e.host.as_str(), e.port, e.password.as_str()),
            ("localhost", 5900, "p@ss")
        );
        let e = parse_url("vnc://127.0.0.1:5901").unwrap();
        assert_eq!((e.port, e.password.as_str()), (5901, ""));
        assert!(parse_url("http://127.0.0.1:5900").is_err());
        assert!(parse_url("vnc://127.0.0.1").is_err());
    }

    #[test]
    fn auth_response_is_des_with_reversed_key_bits() {
        // Known answer from a libvncclient session: password "secret", the
        // all-zero challenge; DES-ECB of two zero blocks under the reversed
        // key is deterministic, so both halves are equal.
        let r = auth_response("secret", &[0u8; 16]);
        assert_eq!(r[..8], r[8..]);
        assert_ne!(r[..8], [0u8; 8]);
        // A different password answers differently; longer passwords are
        // truncated to eight bytes.
        assert_ne!(auth_response("other", &[0u8; 16]), r);
        assert_eq!(
            auth_response("12345678", &[7u8; 16]),
            auth_response("12345678xyz", &[7u8; 16])
        );
    }

    #[test]
    fn pixel_formats_round_trip_and_convert() {
        let pf = PixelFormat::RGBX32;
        assert_eq!(PixelFormat::decode(&pf.encode()), pf);
        assert_eq!(pf.rgb(&[0x33, 0x22, 0x11, 0x00]), [0x11, 0x22, 0x33]);
        let be = PixelFormat {
            big_endian: true,
            ..pf
        };
        assert_eq!(be.rgb(&[0x00, 0x11, 0x22, 0x33]), [0x11, 0x22, 0x33]);
        let rgb565 = PixelFormat {
            bits_per_pixel: 16,
            depth: 16,
            big_endian: false,
            true_colour: true,
            red_max: 31,
            green_max: 63,
            blue_max: 31,
            red_shift: 11,
            green_shift: 5,
            blue_shift: 0,
        };
        // Pure red in RGB565 little endian: 0xF800 -> bytes 00 F8.
        assert_eq!(rgb565.rgb(&[0x00, 0xf8]), [255, 0, 0]);
        assert_eq!(rgb565.bytes_per_pixel(), 2);
    }

    #[tokio::test]
    async fn negotiation_can_be_switched_on() {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let port = fake_server("hunter2", seen.clone()).await;
        let opts = ConnectOptions {
            set_pixel_format: true,
            set_encodings: true,
            incremental_first: true,
        };
        let mut c = VncClient::connect_with(&format!("vnc://:hunter2@127.0.0.1:{port}"), opts)
            .await
            .unwrap();
        // The fake advertises big endian; the client asked for RGBX32 and
        // decodes accordingly.
        assert!(c.server_format().big_endian);
        let frame = c.capture().await.unwrap();
        assert_eq!(&frame.rgb[..3], &[255, 0, 0]);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let s = seen.lock().unwrap();
        assert_eq!(s.pixel_format, Some(PixelFormat::RGBX32));
        assert_eq!(s.encodings, vec![ENCODING_COPY_RECT, ENCODING_RAW]);
        assert_eq!(s.updates, vec![(true, 0, 0, 4, 3)]);
    }

    #[test]
    fn frames_downscale_by_an_integer_factor() {
        assert_eq!(scale_factor(1024, 768), 1);
        assert_eq!(scale_factor(1280, 960), 1);
        assert_eq!(scale_factor(1920, 1440), 2);
        assert_eq!(scale_factor(2560, 1600), 2);
        assert_eq!(scale_factor(3840, 2160), 3);
        // 4x2 -> 2x1: each output pixel averages a 2x2 block.
        let frame = Frame {
            width: 4,
            height: 2,
            rgb: vec![
                0, 0, 0, 255, 255, 255, 10, 20, 30, 10, 20, 30, //
                255, 255, 255, 0, 0, 0, 10, 20, 30, 10, 20, 30,
            ],
        };
        let small = frame.clone().downscale(2);
        assert_eq!((small.width, small.height), (2, 1));
        assert_eq!(small.rgb, vec![128, 128, 128, 10, 20, 30]);
        assert_eq!(frame.clone().downscale(1), frame);
    }

    #[test]
    fn png_encodes_a_frame() {
        let frame = Frame {
            width: 2,
            height: 2,
            rgb: vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255],
        };
        let png = encode_png(&frame).unwrap();
        assert_eq!(warren_core::screen::png_dimensions(&png), Some((2, 2)));
        let mut decoder = png::Decoder::new(std::io::Cursor::new(&png));
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!(&buf[..info.buffer_size()], &frame.rgb[..]);
    }

    #[tokio::test]
    async fn captures_a_frame_and_sends_input() {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let port = fake_server("hunter2", seen.clone()).await;
        let url = format!("vnc://:hunter2@127.0.0.1:{port}");
        let mut c = VncClient::connect(&url).await.unwrap();
        assert_eq!((c.width(), c.height(), c.name()), (4, 3, "fake"));
        let frame = c.capture().await.unwrap();
        let row = |y: usize| &frame.rgb[y * 12..y * 12 + 12];
        assert_eq!(row(0), &[255, 0, 0].repeat(4)[..]);
        assert_eq!(row(1), &[0, 255, 0].repeat(4)[..]);
        assert_eq!(row(2), &[0, 255, 0].repeat(4)[..], "CopyRect copied row 1");

        c.perform_all(&[
            InputAction::Click {
                x: 2,
                y: 1,
                button: MouseButton::Right,
                count: 2,
            },
            InputAction::Move { x: 99, y: 99 },
            InputAction::Scroll {
                x: 0,
                y: 0,
                dx: 0,
                dy: -1,
            },
            InputAction::Type {
                text: "Hi\n".into(),
            },
            InputAction::Key {
                key: "cmd+space".into(),
            },
        ])
        .await
        .unwrap();
        // Give the server a moment to read the last events.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let s = seen.lock().unwrap();
        assert!(s.auth_ok);
        assert_eq!(s.pixel_format, None, "nothing negotiated by default");
        assert!(s.encodings.is_empty());
        assert_eq!(s.updates, vec![(false, 0, 0, 4, 3)]);
        assert_eq!(
            s.pointer,
            vec![
                (2, 1, 0),
                (2, 1, 4),
                (2, 1, 0),
                (2, 1, 4),
                (2, 1, 0),
                (3, 2, 0),
                (0, 0, 0),
                (0, 0, WHEEL_UP),
                (0, 0, 0),
            ]
        );
        assert_eq!(
            s.keys,
            vec![
                (KEYSYM_SHIFT_L, true),
                (0x68, true),
                (0x68, false),
                (KEYSYM_SHIFT_L, false),
                (0x69, true),
                (0x69, false),
                (0xff0d, true),
                (0xff0d, false),
                (COMMAND_KEYSYM, true),
                (0x20, true),
                (0x20, false),
                (COMMAND_KEYSYM, false),
            ]
        );
    }

    /// Against a live Lume VM: `WARREN_VNC_VM=<vm name> cargo test -p
    /// warren-runtime-mac vnc::tests::live -- --ignored --nocapture` (or
    /// `WARREN_VNC_URL='vnc://:pw@127.0.0.1:port'`). With
    /// `WARREN_VNC_UNLOCK_PASSWORD_FILE` (or `..._PASSWORD`) it first logs
    /// the user in at the login window. Saves /tmp/warren-vnc-live-{1,2}.png
    /// and reports capture times; sends cmd+space, types, then escape.
    #[tokio::test]
    #[ignore]
    async fn live_probe_of_a_lume_vm() {
        let url = match std::env::var("WARREN_VNC_URL") {
            Ok(u) => u,
            Err(_) => {
                let Ok(vm) = std::env::var("WARREN_VNC_VM") else {
                    eprintln!("WARREN_VNC_URL and WARREN_VNC_VM unset; skipping");
                    return;
                };
                let lume =
                    crate::lume::LumeClient::new(reqwest::Client::new(), "http://127.0.0.1:7777");
                let d = lume.get(&vm).await.unwrap().expect("the VM exists");
                d.vnc_url.expect("the VM is running with VNC")
            }
        };
        let password = std::env::var("WARREN_VNC_UNLOCK_PASSWORD")
            .ok()
            .or_else(|| {
                std::env::var("WARREN_VNC_UNLOCK_PASSWORD_FILE")
                    .ok()
                    .and_then(|p| std::fs::read_to_string(p).ok())
            });
        // Negotiation stays off unless asked for: Lume's server dies on it.
        let flag = |name: &str| std::env::var_os(name).is_some();
        let opts = ConnectOptions {
            set_pixel_format: flag("WARREN_VNC_SETPF"),
            set_encodings: flag("WARREN_VNC_SETENC"),
            incremental_first: flag("WARREN_VNC_INCREMENTAL"),
        };
        let t = std::time::Instant::now();
        let mut c = VncClient::connect_with(&url, opts).await.unwrap();
        eprintln!(
            "connected in {:?}: {}x{} {:?} server format {:?} opts {opts:?}",
            t.elapsed(),
            c.width(),
            c.height(),
            c.name(),
            c.server_format()
        );
        if let Ok(ms) = std::env::var("WARREN_VNC_GAP_MS") {
            c.set_event_gap(Duration::from_millis(ms.parse().unwrap()));
        }
        if let Some(password) = password {
            // The base image boots to the login window; log the user in
            // (clearing whatever an earlier run left in the field).
            c.perform_all(&[
                InputAction::Click {
                    x: 480,
                    y: 640,
                    button: MouseButton::Left,
                    count: 1,
                },
                InputAction::Type {
                    text: "\u{8}".repeat(40),
                },
                InputAction::Type {
                    text: format!("{}\n", password.trim()),
                },
            ])
            .await
            .unwrap();
            eprintln!("typed the password; waiting for the desktop");
            tokio::time::sleep(Duration::from_secs(8)).await;
        }
        let t = std::time::Instant::now();
        let frame = c.capture().await.unwrap();
        let png = encode_png(&frame).unwrap();
        eprintln!(
            "frame 1 in {:?}: {}x{} (scale {}), png {} bytes",
            t.elapsed(),
            frame.width,
            frame.height,
            c.scale(),
            png.len()
        );
        std::fs::write("/tmp/warren-vnc-live-1.png", &png).unwrap();
        // WARREN_VNC_SCRIPT="click:795,622;sleep:3;key:cmd+space;type:Terminal"
        // replaces the default Spotlight sequence.
        let script = std::env::var("WARREN_VNC_SCRIPT")
            .unwrap_or_else(|_| "key:cmd+space;sleep:1.5;type:Terminal;sleep:1.5".into());
        for step in script.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            let (kind, arg) = step.split_once(':').unwrap_or((step, ""));
            match kind {
                "sleep" => tokio::time::sleep(Duration::from_secs_f64(arg.parse().unwrap())).await,
                "click" => {
                    let (x, y) = arg.split_once(',').unwrap();
                    c.perform(&InputAction::Click {
                        x: x.parse().unwrap(),
                        y: y.parse().unwrap(),
                        button: MouseButton::Left,
                        count: 1,
                    })
                    .await
                    .unwrap();
                }
                "key" => c
                    .perform(&InputAction::Key { key: arg.into() })
                    .await
                    .unwrap(),
                // rawchord:0xffe7,0x20 presses the keysyms in order and releases
                // them in reverse, for probing modifier mappings.
                "rawchord" => {
                    // `rawchord:0xffeb,0x71@300`: hold each key 300 ms.
                    let (list, hold) = arg.split_once('@').unwrap_or((arg, "60"));
                    let hold = Duration::from_millis(hold.parse().unwrap());
                    let syms: Vec<u32> = list
                        .split(',')
                        .map(|k| {
                            u32::from_str_radix(k.trim().trim_start_matches("0x"), 16).unwrap()
                        })
                        .collect();
                    for k in &syms {
                        c.key(*k, true).await.unwrap();
                        tokio::time::sleep(hold).await;
                    }
                    for k in syms.iter().rev() {
                        c.key(*k, false).await.unwrap();
                        tokio::time::sleep(hold).await;
                    }
                }
                "type" => c
                    .perform(&InputAction::Type { text: arg.into() })
                    .await
                    .unwrap(),
                "shot" => {
                    // A fresh connection per frame, as the daemon does: Apple's
                    // server answers only the first full request promptly.
                    let mut c2 = VncClient::connect_with(&url, opts).await.unwrap();
                    let t = std::time::Instant::now();
                    let f = c2.capture().await.unwrap();
                    eprintln!("shot {arg} in {:?}", t.elapsed());
                    c2.close().await;
                    std::fs::write(
                        format!("/tmp/warren-vnc-live-{arg}.png"),
                        encode_png(&f).unwrap(),
                    )
                    .unwrap();
                }
                other => panic!("unknown step {other}"),
            }
            eprintln!("step {step} done");
        }
        let t = std::time::Instant::now();
        let mut c2 = VncClient::connect_with(&url, opts).await.unwrap();
        let frame2 = c2.capture().await.unwrap();
        c2.close().await;
        let png2 = encode_png(&frame2).unwrap();
        eprintln!(
            "frame 2 in {:?}, png {} bytes, differs: {}",
            t.elapsed(),
            png2.len(),
            frame2.rgb != frame.rgb
        );
        std::fs::write("/tmp/warren-vnc-live-2.png", &png2).unwrap();
        c.perform_all(&[
            InputAction::Key {
                key: "escape".into(),
            },
            InputAction::Key {
                key: "escape".into(),
            },
        ])
        .await
        .unwrap();
        c.close().await;
    }

    #[tokio::test]
    async fn a_wrong_password_is_reported() {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let port = fake_server("hunter2", seen.clone()).await;
        let Err(err) = VncClient::connect(&format!("vnc://:nope@127.0.0.1:{port}")).await else {
            panic!("a wrong password connected");
        };
        assert!(
            err.cause().unwrap_or_default().contains("wrong password"),
            "{}",
            err.describe()
        );
    }
}
