//! A small RTSP client for what FFmpeg cannot read: raw pictures in GStreamer's own X-GST RTP
//! payload, as the Kinova arm's depth camera sends them. It takes UDP, since GStreamer's server
//! drops what a TCP client cannot take in one burst.

use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};

use crate::av::{Cancel, Layout, PixelFormat};

/// How long a connection or an answer may take.
const ANSWER: Duration = Duration::from_secs(10);
/// How long the stream may go without a whole picture before it counts as hung.
const STALL: Duration = Duration::from_secs(5);
/// How long the goodbye may take, which frees the camera's session at once.
const GOODBYE: Duration = Duration::from_secs(1);
/// The most an answer's head or body may hold.
const MOST_TEXT: usize = 64 << 10;
/// The largest picture taken, 4096x4096 at 4 bytes a pixel.
const MOST_PICTURE: usize = 64 << 20;
/// A picture comes in one burst, so the system holds a few while the reader catches up.
const RECEIVE_BUFFER: usize = 8 << 20;

/// GStreamer's raw formats read here, with the bytes a pixel takes in the first plane.
const FORMATS: [(&str, PixelFormat, u32); 7] = [
    ("GRAY8", PixelFormat::GRAY8, 1),
    ("GRAY16_LE", PixelFormat::GRAY16LE, 2),
    ("RGB", PixelFormat::RGB24, 3),
    ("BGR", PixelFormat::BGR24, 3),
    ("RGBA", PixelFormat::RGBA, 4),
    ("YUY2", PixelFormat::YUYV422, 2),
    ("NV12", PixelFormat::NV12, 1),
];

/// What a DESCRIBE says of an X-GST stream of raw pictures.
#[derive(Clone, Debug, PartialEq)]
pub struct Description {
    setup: String,
    play: String,
    payload_type: u8,
    pub layout: Layout,
    /// One picture's bytes, with GStreamer's rows padded to 4 bytes.
    pub size: usize,
    /// 0 when unstated.
    pub fps: f64,
}

/// Asks the camera what it serves. None when it is not raw pictures in X-GST, which FFmpeg reads,
/// or when the URL has a login, which only FFmpeg's reader offers.
pub fn describe(url: &str, cancel: &Cancel) -> Result<Option<Description>, String> {
    let Some(mut c) = Control::connect(url)? else { return Ok(None) };
    let a = c.ask("DESCRIBE", url, "Accept: application/sdp\r\n", Some(cancel), ANSWER)?;
    if a.status != 200 {
        return Ok(None); // FFmpeg tells what is wrong
    }
    let base = a.header("content-base").or(a.header("content-location")).unwrap_or(url).to_owned();
    Ok(parse_sdp(&a.body, &base))
}

/// One playing session, which ends with a goodbye on drop.
pub struct Session {
    control: Control,
    rtp: UdpSocket,
    _rtcp: UdpSocket,
    camera: IpAddr,
    ssrc: Option<u32>,
    payload_type: u8,
    keepalive: Duration,
    last_said: Instant,
    last_drained: Instant,
    packet: Vec<u8>,
    picture: Assembler,
}

impl Session {
    pub fn open(url: &str, d: &Description, cancel: &Cancel) -> Result<Session, String> {
        let mut control = Control::connect(url)?.ok_or("the URL is not one this client takes")?;
        let (local, camera) = (control.tcp.local_addr().map_err(|e| e.to_string())?.ip(), control.tcp.peer_addr().map_err(|e| e.to_string())?.ip());
        let (rtp, rtcp) = udp_pair(local)?;
        let port = rtp.local_addr().map_err(|e| e.to_string())?.port();
        let transport = format!("Transport: RTP/AVP;unicast;client_port={port}-{}\r\n", port + 1);
        let a = control.ask("SETUP", &d.setup, &transport, Some(cancel), ANSWER)?;
        if a.status != 200 {
            // a camera that serves one viewer at a time says 503 while another holds it
            return Err(format!("the camera refused a session ({})", a.status));
        }
        let session = a.header("session").ok_or("the camera named no session")?;
        let mut parts = session.split(';').map(str::trim);
        control.session = parts.next().map(str::to_owned);
        control.teardown = Some(d.play.clone());
        let timeout = parts.find_map(|p| p.strip_prefix("timeout=")).and_then(|t| t.parse().ok()).unwrap_or(60u64);
        let ssrc = a.header("transport").and_then(|t| t.split(';').find_map(|p| p.trim().strip_prefix("ssrc="))).and_then(|s| u32::from_str_radix(s, 16).ok());
        let a = control.ask("PLAY", &d.play, "", Some(cancel), ANSWER)?;
        if a.status != 200 {
            return Err(format!("the camera would not play ({})", a.status));
        }
        rtp.set_read_timeout(Some(Duration::from_millis(100))).map_err(|e| e.to_string())?;
        Ok(Session {
            control,
            rtp,
            _rtcp: rtcp,
            camera,
            ssrc,
            payload_type: d.payload_type,
            keepalive: Duration::from_secs(timeout.clamp(2, 600) / 2),
            last_said: Instant::now(),
            last_drained: Instant::now(),
            packet: vec![0; 65536],
            picture: Assembler::new(d.size),
        })
    }

    /// The next whole picture. Err when the stream ended or stalled.
    pub fn read(&mut self, cancel: &Cancel) -> Result<&[u8], String> {
        let from = Instant::now();
        loop {
            if cancel() {
                return Err("stopped".into());
            }
            if from.elapsed() > STALL {
                return Err("no whole picture in time".into());
            }
            self.keep_alive()?;
            let (n, sender) = match self.rtp.recv_from(&mut self.packet) {
                Ok(r) => r,
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => continue,
                Err(e) => return Err(e.to_string()),
            };
            // only the camera's packets count, whoever else sends to the port
            if sender.ip() != self.camera {
                continue;
            }
            if let Some(payload) = rtp_payload(&self.packet[..n], self.payload_type, self.ssrc)
                && self.picture.add(payload)
            {
                return Ok(self.picture.whole());
            }
        }
    }

    /// Tells the camera the session is in use before it times out, and reads what it answers.
    fn keep_alive(&mut self) -> Result<(), String> {
        if self.last_said.elapsed() > self.keepalive {
            let uri = self.control.teardown.clone().unwrap_or_default();
            self.control.send("GET_PARAMETER", &uri, "")?;
            self.last_said = Instant::now();
        }
        if self.last_drained.elapsed() > Duration::from_secs(1) {
            self.control.drain()?;
            self.last_drained = Instant::now();
        }
        Ok(())
    }
}

/// The RTSP connection, which says goodbye on drop when it holds a session.
struct Control {
    tcp: TcpStream,
    cseq: u32,
    pending: Vec<u8>,
    session: Option<String>,
    /// Where the goodbye goes.
    teardown: Option<String>,
}

/// One answer from the camera.
struct Answer {
    status: u32,
    cseq: u32,
    headers: Vec<(String, String)>,
    body: String,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

impl Control {
    /// None for a URL this client does not take.
    fn connect(url: &str) -> Result<Option<Control>, String> {
        let Some((host, port)) = address(url) else { return Ok(None) };
        let addr = (host.as_str(), port).to_socket_addrs().map_err(|e| e.to_string())?.next().ok_or("the host has no address")?;
        let tcp = TcpStream::connect_timeout(&addr, ANSWER).map_err(|e| e.to_string())?;
        tcp.set_read_timeout(Some(Duration::from_millis(100))).map_err(|e| e.to_string())?;
        let _ = tcp.set_nodelay(true);
        Ok(Some(Control { tcp, cseq: 0, pending: Vec::new(), session: None, teardown: None }))
    }

    fn send(&mut self, method: &str, uri: &str, extra: &str) -> Result<u32, String> {
        self.cseq += 1;
        let session = self.session.as_ref().map_or(String::new(), |s| format!("Session: {s}\r\n"));
        let request = format!("{method} {uri} RTSP/1.0\r\nCSeq: {}\r\nUser-Agent: rant-transcoder\r\n{session}{extra}\r\n", self.cseq);
        self.tcp.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
        Ok(self.cseq)
    }

    fn ask(&mut self, method: &str, uri: &str, extra: &str, cancel: Option<&Cancel>, within: Duration) -> Result<Answer, String> {
        let cseq = self.send(method, uri, extra)?;
        let until = Instant::now() + within;
        let mut chunk = [0u8; 4096];
        loop {
            while let Some(a) = self.parse()? {
                if a.cseq == cseq {
                    return Ok(a);
                }
            }
            if cancel.is_some_and(|c| c()) {
                return Err("stopped".into());
            }
            if Instant::now() > until {
                return Err("no answer in time".into());
            }
            match self.tcp.read(&mut chunk) {
                Ok(0) => return Err("the camera closed the connection".into()),
                Ok(n) => self.pending.extend_from_slice(&chunk[..n]),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
    }

    /// Reads and drops what came meanwhile, the answers to keepalives.
    fn drain(&mut self) -> Result<(), String> {
        self.tcp.set_nonblocking(true).map_err(|e| e.to_string())?;
        let mut chunk = [0u8; 4096];
        let read = loop {
            match self.tcp.read(&mut chunk) {
                Ok(0) => break Err("the camera closed the connection".to_owned()),
                Ok(n) => self.pending.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break Ok(()),
                Err(e) => break Err(e.to_string()),
            }
        };
        self.tcp.set_nonblocking(false).map_err(|e| e.to_string())?;
        read?;
        while self.parse()?.is_some() {}
        Ok(())
    }

    /// Takes one whole answer off what was read, None until it is all there. A camera's own
    /// request reads as status 0, and interleaved data is skipped.
    fn parse(&mut self) -> Result<Option<Answer>, String> {
        while self.pending.first() == Some(&b'$') {
            let Some(len) = self.pending.get(2..4).map(|l| 4 + u16::from_be_bytes([l[0], l[1]]) as usize) else { return Ok(None) };
            if self.pending.len() < len {
                return Ok(None);
            }
            self.pending.drain(..len);
        }
        let Some(end) = self.pending.windows(4).position(|w| w == b"\r\n\r\n") else {
            return if self.pending.len() > MOST_TEXT { Err("the camera's answer is too long".into()) } else { Ok(None) };
        };
        let head = String::from_utf8_lossy(&self.pending[..end]).into_owned();
        let mut lines = head.split("\r\n");
        let first = lines.next().unwrap_or("");
        let status = if first.starts_with("RTSP/") { first.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0) } else { 0 };
        let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned())).collect();
        let field = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
        let length: usize = field("content-length").map_or(Some(0), |l| l.parse().ok()).ok_or("the camera sent a broken length")?;
        if length > MOST_TEXT {
            return Err("the camera's answer is too long".into());
        }
        let cseq = field("cseq").and_then(|c| c.parse().ok()).unwrap_or(0);
        let total = end + 4 + length;
        if self.pending.len() < total {
            return Ok(None);
        }
        let body = String::from_utf8_lossy(&self.pending[end + 4..total]).into_owned();
        self.pending.drain(..total);
        Ok(Some(Answer { status, cseq, headers, body }))
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        // a camera that serves one viewer at a time is free for the next at once
        if let Some(uri) = self.teardown.take() {
            let _ = self.ask("TEARDOWN", &uri, "", None, GOODBYE);
        }
    }
}

/// The host and port of a plain `rtsp://` URL with no login.
fn address(url: &str) -> Option<(String, u16)> {
    let rest = url.strip_prefix("rtsp://")?;
    let authority = rest.split(['/', '?']).next()?;
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => {
            let (host, after) = v6.split_once(']')?;
            (host, after.strip_prefix(':'))
        }
        None => match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        },
    };
    let port = port.map_or(Some(554), |p| p.parse().ok())?;
    Some((host.to_owned(), port))
}

/// Two UDP ports in a row from an even one, on the address that reaches the camera.
fn udp_pair(ip: IpAddr) -> Result<(UdpSocket, UdpSocket), String> {
    for _ in 0..20 {
        let rtp = bind(ip, 0).map_err(|e| e.to_string())?;
        let port = rtp.local_addr().map_err(|e| e.to_string())?.port();
        if port.is_multiple_of(2)
            && port < u16::MAX
            && let Ok(rtcp) = bind(ip, port + 1)
        {
            return Ok((rtp, rtcp));
        }
    }
    Err("no two UDP ports in a row are free".into())
}

fn bind(ip: IpAddr, port: u16) -> std::io::Result<UdpSocket> {
    let addr = SocketAddr::new(ip, port);
    let s = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    // the system may give less, which only costs pictures when the reader falls behind
    let _ = s.set_recv_buffer_size(RECEIVE_BUFFER);
    s.bind(&addr.into())?;
    Ok(s.into())
}

/// The X-GST stream of raw pictures an SDP offers, None when it offers none.
fn parse_sdp(sdp: &str, base: &str) -> Option<Description> {
    let mut sections = sdp.split("\nm=");
    let session = sections.next()?;
    let play = resolve(base, control_of(session).unwrap_or("*"));
    sections.find_map(|media| {
        let payload_type: u8 = media.lines().next()?.split_whitespace().nth(3)?.parse().ok()?;
        let attr = |name: &str| media.lines().map(str::trim).find_map(|l| l.strip_prefix(&format!("a={name}:{payload_type} ")));
        if !attr("rtpmap")?.to_ascii_uppercase().starts_with("X-GST/") {
            return None;
        }
        let caps = attr("fmtp")?.split(';').map(str::trim).find_map(|p| p.strip_prefix("caps="))?;
        let caps = base64(caps)?;
        let (layout, size, fps) = parse_caps(String::from_utf8_lossy(&caps).trim_end_matches('\0'))?;
        let setup = resolve(base, control_of(media).unwrap_or("*"));
        Some(Description { setup, play: play.clone(), payload_type, layout, size, fps })
    })
}

fn control_of(section: &str) -> Option<&str> {
    section.lines().map(str::trim).find_map(|l| l.strip_prefix("a=control:"))
}

/// A control URL from the SDP, made whole against the base the camera named.
fn resolve(base: &str, control: &str) -> String {
    if control.starts_with("rtsp://") {
        control.to_owned()
    } else if control.is_empty() || control == "*" {
        base.to_owned()
    } else if base.ends_with('/') {
        format!("{base}{control}")
    } else {
        format!("{base}/{control}")
    }
}

/// GStreamer's raw video caps as a layout, its size in bytes and its rate. GStreamer pads rows
/// to 4 bytes and starts NV12's chroma after the luma rows rounded up to even.
fn parse_caps(caps: &str) -> Option<(Layout, usize, f64)> {
    if caps.split(',').next()?.trim() != "video/x-raw" {
        return None;
    }
    let field = |name: &str| caps.split(',').map(str::trim).find_map(|f| f.strip_prefix(name)?.strip_prefix('=')).map(|v| v.rsplit_once(')').map_or(v, |(_, v)| v).trim());
    let (width, height): (u32, u32) = (field("width")?.parse().ok()?, field("height")?.parse().ok()?);
    let (_, format, bytes) = FORMATS.iter().find(|(name, _, _)| Some(*name) == field("format"))?;
    let odd = |v: u32| !v.is_multiple_of(2);
    if width == 0 || height == 0 || (*format == PixelFormat::NV12 && (odd(width) || odd(height))) || (*format == PixelFormat::YUYV422 && odd(width)) {
        return None;
    }
    let stride = (width as u64 * *bytes as u64).div_ceil(4) * 4;
    let rows = if *format == PixelFormat::NV12 { height as u64 * 3 / 2 } else { height as u64 };
    let size = stride.checked_mul(rows).filter(|&s| s <= MOST_PICTURE as u64)? as usize;
    let fps = field("framerate").and_then(|r| r.split_once('/')).and_then(|(n, d)| Some((n.parse::<f64>().ok()?, d.parse::<f64>().ok()?))).filter(|(_, d)| *d > 0.0).map_or(0.0, |(n, d)| n / d);
    Some((Layout { width, height, stride: stride as u32, format: *format }, size, fps))
}

fn base64(text: &str) -> Option<Vec<u8>> {
    let (mut out, mut acc, mut bits) = (Vec::new(), 0u32, 0);
    for c in text.bytes().take_while(|&c| c != b'=') {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6 | v as u32) & 0xffff;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// The payload of one RTP packet of the stream, None for any other packet.
fn rtp_payload(p: &[u8], payload_type: u8, ssrc: Option<u32>) -> Option<&[u8]> {
    if p.len() < 12 || p[0] >> 6 != 2 || p[1] & 0x7f != payload_type {
        return None;
    }
    if ssrc.is_some_and(|s| s.to_be_bytes() != p[8..12]) {
        return None;
    }
    let mut start = 12 + 4 * (p[0] & 0x0f) as usize;
    if p[0] & 0x10 != 0 {
        let words = u16::from_be_bytes([*p.get(start + 2)?, *p.get(start + 3)?]) as usize;
        start += 4 + 4 * words;
    }
    let padding = if p[0] & 0x20 != 0 { *p.last()? as usize } else { 0 };
    p.get(start..p.len().checked_sub(padding)?)
}

/// Puts pictures back together from X-GST packets: a picture is whole only when every packet
/// starts where the one before ended, from offset 0 to its size.
struct Assembler {
    picture: Vec<u8>,
    /// How far the current picture has come, None after a gap until the next starts.
    filled: Option<usize>,
}

impl Assembler {
    fn new(size: usize) -> Assembler {
        Assembler { picture: vec![0; size], filled: None }
    }

    /// Takes one packet's payload. True when it completed a picture.
    fn add(&mut self, payload: &[u8]) -> bool {
        // an event, or a format sent along, which pictures that keep the SDP's never carry
        if payload.len() < 8 || payload[0] & 0x80 != 0 || payload[1] != 0 {
            self.filled = None;
            return false;
        }
        let offset = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]) as usize;
        let data = &payload[8..];
        if offset == 0 {
            self.filled = Some(0);
        }
        let end = offset + data.len();
        if self.filled != Some(offset) || end > self.picture.len() {
            self.filled = None;
            return false;
        }
        self.picture[offset..end].copy_from_slice(data);
        self.filled = Some(end).filter(|&e| e < self.picture.len());
        self.filled.is_none()
    }

    fn whole(&self) -> &[u8] {
        &self.picture
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The arm's own SDP, as its depth camera sent it.
    const KINOVA: &str = "v=0\r\no=- 17228452073364664677 1 IN IP4 10.20.0.100\r\ns=Session streamed with GStreamer\r\ni=rtsp-server\r\nt=0 0\r\na=tool:GStreamer\r\na=type:broadcast\r\na=control:*\r\na=range:npt=now-\r\nm=application 0 RTP/AVP 96\r\nc=IN IP4 0.0.0.0\r\na=rtpmap:96 X-GST/90000\r\na=framerate:30\r\na=fmtp:96 caps=dmlkZW8veC1yYXcsIHdpZHRoPShpbnQpNDgwLCBoZWlnaHQ9KGludCkyNzAsIGZyYW1lcmF0ZT0oZnJhY3Rpb24pMzAvMSwgcGl4ZWwtYXNwZWN0LXJhdGlvPShmcmFjdGlvbikxLzEsIGludGVybGFjZS1tb2RlPShzdHJpbmcpcHJvZ3Jlc3NpdmUsIGZvcm1hdD0oc3RyaW5nKUdSQVkxNl9MRQ==;capsversion=0\r\na=control:stream=0\r\na=ts-refclk:local\r\na=mediaclk:sender\r\n";

    fn packet(offset: u32, data: &[u8]) -> Vec<u8> {
        let mut p = vec![0, 0, 0, 0];
        p.extend_from_slice(&offset.to_be_bytes());
        p.extend_from_slice(data);
        p
    }

    #[test]
    fn reads_the_arms_depth_stream() {
        let d = parse_sdp(KINOVA, "rtsp://10.0.0.30/depth/").expect("an X-GST stream");
        assert_eq!((d.layout.width, d.layout.height, d.layout.stride), (480, 270, 960));
        assert!(d.layout.format == PixelFormat::GRAY16LE);
        assert_eq!((d.size, d.fps, d.payload_type), (259_200, 30.0, 96));
        assert_eq!((d.setup.as_str(), d.play.as_str()), ("rtsp://10.0.0.30/depth/stream=0", "rtsp://10.0.0.30/depth/"));
    }

    #[test]
    fn leaves_other_streams_to_ffmpeg() {
        let h264 = "v=0\r\ns=cam\r\nt=0 0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=control:track0\r\n";
        assert!(parse_sdp(h264, "rtsp://cam/").is_none());
        // the caps' first six bytes, "video/", become "audio/"
        let audio = KINOVA.replace("dmlkZW8v", "YXVkaW8v");
        assert!(parse_sdp(&audio, "rtsp://cam/").is_none());
    }

    #[test]
    fn caps_give_gstreamers_padded_rows() {
        let caps = |format: &str, w: u32, h: u32| parse_caps(&format!("video/x-raw, width=(int){w}, height=(int){h}, framerate=(fraction)15/2, format=(string){format}"));
        let (l, size, fps) = caps("RGB", 481, 10).unwrap();
        assert_eq!((l.stride, size, fps), (1444, 14_440, 7.5));
        let (l, size, _) = caps("NV12", 6, 4).unwrap();
        assert_eq!((l.stride, size), (8, 48));
        assert_eq!(caps("GRAY8", 3, 3).unwrap().1, 12);
        assert!(caps("NV12", 6, 5).is_none() && caps("YUY2", 5, 4).is_none());
        assert!(caps("I420", 640, 480).is_none() && caps("GRAY8", 0, 480).is_none());
        assert!(caps("RGBA", 65535, 65535).is_none(), "far past the largest picture");
    }

    #[test]
    fn urls_split_and_resolve() {
        assert_eq!(address("rtsp://10.0.0.30/depth"), Some(("10.0.0.30".into(), 554)));
        assert_eq!(address("rtsp://cam:8554/a?b"), Some(("cam".into(), 8554)));
        assert_eq!(address("rtsp://[::1]:9/x"), Some(("::1".into(), 9)));
        assert_eq!(address("rtsp://user:pass@cam/x"), None);
        assert_eq!(address("http://cam/x"), None);
        assert_eq!(resolve("rtsp://cam/depth", "stream=0"), "rtsp://cam/depth/stream=0");
        assert_eq!(resolve("rtsp://cam/depth/", "*"), "rtsp://cam/depth/");
        assert_eq!(resolve("rtsp://cam/depth/", "rtsp://other/x"), "rtsp://other/x");
    }

    #[test]
    fn rtp_headers_are_read_and_checked() {
        let mut p = vec![0x80, 96, 0, 1, 0, 0, 0, 0, 0x8c, 0xa9, 0x0f, 0xec];
        p.extend_from_slice(b"pixels");
        assert_eq!(rtp_payload(&p, 96, Some(0x8ca90fec)), Some(&b"pixels"[..]));
        assert_eq!(rtp_payload(&p, 96, Some(1)), None, "another stream's packet");
        assert_eq!(rtp_payload(&p, 97, None), None);
        // a contributing source, an extension of one word and two bytes of padding
        let mut q = vec![0xb1, 96, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 9, 9, 9, 9, 0, 0, 0, 1, 7, 7, 7, 7];
        q.extend_from_slice(&[1, 2, 3, 0, 2]);
        assert_eq!(rtp_payload(&q, 96, None), Some(&[1, 2, 3][..]));
        assert_eq!(rtp_payload(&q[..14], 96, None), None, "cut short");
        assert_eq!(rtp_payload(&[0x80, 96, 0, 1], 96, None), None);
    }

    #[test]
    fn only_whole_pictures_come_out() {
        let mut a = Assembler::new(6);
        assert!(!a.add(&packet(0, &[1, 2])) && !a.add(&packet(2, &[3, 4])));
        assert!(a.add(&packet(4, &[5, 6])));
        assert_eq!(a.whole(), &[1, 2, 3, 4, 5, 6]);
        // a lost packet loses the picture, and the next one starts clean
        assert!(!a.add(&packet(0, &[9, 9])) && !a.add(&packet(4, &[9, 9])));
        assert!(!a.add(&packet(2, &[9, 9])), "the gap is not filled later");
        assert!(!a.add(&packet(0, &[7, 7])) && !a.add(&packet(2, &[7, 7])) && a.add(&packet(4, &[7, 7])));
        // a picture cut short by the next, and one that runs past its size
        assert!(!a.add(&packet(0, &[1, 1])) && !a.add(&packet(0, &[2, 2])) && !a.add(&packet(2, &[2, 2])) && a.add(&packet(4, &[2, 2])));
        assert_eq!(a.whole(), &[2, 2, 2, 2, 2, 2]);
        assert!(!a.add(&packet(0, &[1, 1, 1, 1])) && !a.add(&packet(4, &[1, 1, 1])));
        // events and formats sent along are not pixels
        let mut event = packet(0, &[1; 6]);
        event[1] = 1;
        let mut caps = packet(0, &[1; 6]);
        caps[0] = 0x80;
        assert!(!a.add(&event) && !a.add(&caps) && !a.add(&[0, 0, 0]));
    }

    #[test]
    fn base64_decodes() {
        assert_eq!(base64("dmlkZW8="), Some(b"video".to_vec()));
        assert_eq!(base64("TWFu"), Some(b"Man".to_vec()));
        assert_eq!(base64("a b"), None);
    }
}
