//! Serial control protocol for the Creative Sound Blaster X4 (USB 041e:3278).
//!
//! The X4's CDC-ACM port accepts `5a CMD LEN PAYLOAD` frames, but only after a
//! challenge/response unlock followed by `SW_MODE1`. The unlock scheme is the
//! one documented for the Katana V2X (https://blog.nns.ee/2026/02/20/katana-v2x-re);
//! the command set matches the G6's HID protocol (linuxblaster_control).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use nix::sys::termios::{self, FlushArg, SetArg, SpecialCharacterIndices};
use rand::RngCore;

const USB_VENDOR: &str = "041e";
const USB_PRODUCT: &str = "3278";

/// Static key material from Creative's CTCDC.dll, as published in the v2x crate.
/// The AES-256 key is `[challenge header (2)] KEY_DATA [USB PID (2)]`.
const KEY_DATA: [u8; 28] = [
    0xd3, 0x1a, 0x21, 0x27, 0x9b, 0xe3, 0x46, 0xf0, 0x99, 0x9d, 0x6e, 0xc4, 0xc3, 0xfe,
    0xbe, 0x98, 0x90, 0x18, 0x69, 0xc1, 0x18, 0xfb, 0xb1, 0x25, 0x6e, 0x0c, 0xe0, 0x7b,
];

pub const FAMILY_PLAYBACK: u8 = 0x96;
pub const EQ_ENABLE: u8 = 0x09;
pub const EQ_PREAMP: u8 = 0x0a;
/// Feature IDs of the ten EQ bands, 31 Hz to 16 kHz.
pub const EQ_BANDS: [u8; 10] = [0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14];

pub const OUTPUT_SPEAKERS: u8 = 0x01;
pub const OUTPUT_HEADPHONES: u8 = 0x04;

const MARKER: u8 = 0x5a;
const CMD_ACK: u8 = 0x02;
const CMD_FIRMWARE: u8 = 0x09;
const CMD_READ_FEATURE: u8 = 0x11;
const CMD_WRITE_FEATURE: u8 = 0x12;
const CMD_OUTPUT: u8 = 0x2c;

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Feature { family: u8, id: u8, value: f32 },
    Output(u8),
    Firmware(String),
    Rejected { cmd: u8, status: u8 },
    Disconnected,
}

#[derive(Debug)]
pub enum ConnectError {
    NotFound,
    Disconnected,
    Permission(PathBuf),
    Unlock(String),
    Io(io::Error),
}

impl From<io::Error> for ConnectError {
    fn from(e: io::Error) -> Self {
        ConnectError::Io(e)
    }
}

impl ConnectError {
    pub fn title(&self) -> String {
        match self {
            ConnectError::NotFound => "No Sound Blaster X4 found".into(),
            ConnectError::Disconnected => "Sound Blaster X4 disconnected".into(),
            ConnectError::Permission(_) => "No access to the X4's control port".into(),
            ConnectError::Unlock(_) => "Couldn't unlock the X4".into(),
            ConnectError::Io(_) => "Lost contact with the X4".into(),
        }
    }

    pub fn description(&self) -> String {
        match self {
            ConnectError::NotFound | ConnectError::Disconnected => {
                "Plug in the X4 over USB. This page connects as soon as it appears.".into()
            }
            ConnectError::Permission(path) => format!(
                "Can't open {}. Install the sbx4-control package, which adds a udev rule \
                 for it, then unplug and replug the X4.",
                path.display()
            ),
            ConnectError::Unlock(msg) => msg.clone(),
            ConnectError::Io(e) => e.to_string(),
        }
    }

    /// Whether to keep polling for the device instead of offering a retry button.
    pub fn retry_automatically(&self) -> bool {
        matches!(self, ConnectError::NotFound | ConnectError::Disconnected)
    }
}

/// Write side of an unlocked control port. Dropping it stops its reader thread.
pub struct Device {
    port: File,
    alive: Arc<AtomicBool>,
}

pub struct Connection {
    pub device: Device,
    pub reader: File,
    pub path: PathBuf,
    pub firmware: String,
}

impl Device {
    pub fn read_feature(&mut self, family: u8, id: u8) -> io::Result<()> {
        self.send(CMD_READ_FEATURE, &[0x01, family, id])
    }

    pub fn write_feature(&mut self, family: u8, id: u8, value: f32) -> io::Result<()> {
        let v = value.to_le_bytes();
        self.send(CMD_WRITE_FEATURE, &[0x01, family, id, v[0], v[1], v[2], v[3]])
    }

    pub fn set_output(&mut self, code: u8) -> io::Result<()> {
        self.send(CMD_OUTPUT, &[0x00, code, 0x00, 0x00, 0x00])
    }

    /// Ask for everything the UI shows; the answers arrive as events.
    pub fn request_state(&mut self) -> io::Result<()> {
        self.read_feature(FAMILY_PLAYBACK, EQ_ENABLE)?;
        self.read_feature(FAMILY_PLAYBACK, EQ_PREAMP)?;
        for id in EQ_BANDS {
            self.read_feature(FAMILY_PLAYBACK, id)?;
        }
        self.send(CMD_OUTPUT, &[0x01])
    }

    /// Decode frames from `reader` on a background thread and send them as events.
    pub fn spawn_reader(&self, mut reader: File, path: PathBuf, events: async_channel::Sender<Event>) {
        let alive = self.alive.clone();
        thread::spawn(move || {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 256];
            while alive.load(Ordering::Relaxed) {
                match reader.read(&mut chunk) {
                    // Reads time out after 100 ms of silence; that's when we check for unplugging.
                    Ok(0) => {
                        if !path.exists() {
                            break;
                        }
                    }
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        for frame in take_frames(&mut buf) {
                            for event in decode(&frame) {
                                if events.send_blocking(event).is_err() {
                                    return;
                                }
                            }
                        }
                        if buf.len() > 1024 {
                            buf.clear();
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            if alive.load(Ordering::Relaxed) {
                let _ = events.send_blocking(Event::Disconnected);
            }
        });
    }

    fn send(&mut self, cmd: u8, data: &[u8]) -> io::Result<()> {
        self.port.write_all(&frame(cmd, data))
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
    }
}

/// Find the X4's control port by walking sysfs.
pub fn find_port() -> Option<PathBuf> {
    for entry in fs::read_dir("/sys/class/tty").ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("ttyACM") {
            continue;
        }
        let Ok(interface) = fs::canonicalize(entry.path().join("device")) else {
            continue;
        };
        let Some(usb) = interface.parent() else {
            continue;
        };
        if read_attr(usb, "idVendor").as_deref() == Some(USB_VENDOR)
            && read_attr(usb, "idProduct").as_deref() == Some(USB_PRODUCT)
        {
            return Some(Path::new("/dev").join(&*name));
        }
    }
    None
}

/// Open, unlock and verify the control port. Blocks for up to a few seconds.
pub fn connect() -> Result<Connection, ConnectError> {
    let path = find_port().ok_or(ConnectError::NotFound)?;
    let mut port = open_raw(&path).map_err(|e| match e.kind() {
        io::ErrorKind::PermissionDenied => ConnectError::Permission(path.clone()),
        _ => ConnectError::Io(e),
    })?;
    unlock(&mut port)?;

    port.write_all(&frame(CMD_FIRMWARE, &[0x02]))?;
    let mut reply = read_reply(&mut port, Duration::from_millis(800))?;
    let firmware = take_frames(&mut reply)
        .iter()
        .flat_map(|f| decode(f))
        .find_map(|e| match e {
            Event::Firmware(version) => Some(version),
            _ => None,
        })
        .ok_or_else(|| ConnectError::Unlock("The X4 didn't answer after unlocking.".into()))?;

    let reader = port.try_clone()?;
    let device = Device { port, alive: Arc::new(AtomicBool::new(true)) };
    Ok(Connection { device, reader, path, firmware })
}

fn read_attr(dir: &Path, name: &str) -> Option<String> {
    fs::read_to_string(dir.join(name)).ok().map(|s| s.trim().to_owned())
}

fn open_raw(path: &Path) -> io::Result<File> {
    let port = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(nix::libc::O_NOCTTY)
        .open(path)?;
    let mut tio = termios::tcgetattr(&port)?;
    termios::cfmakeraw(&mut tio);
    tio.control_chars[SpecialCharacterIndices::VMIN as usize] = 0;
    tio.control_chars[SpecialCharacterIndices::VTIME as usize] = 1;
    termios::tcsetattr(&port, SetArg::TCSANOW, &tio)?;
    termios::tcflush(&port, FlushArg::TCIOFLUSH)?;
    Ok(port)
}

/// Collect bytes until the device goes quiet after answering, or `timeout` passes.
fn read_reply(port: &mut File, timeout: Duration) -> io::Result<Vec<u8>> {
    let start = Instant::now();
    let mut out = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        let n = port.read(&mut chunk)?;
        if n > 0 {
            out.extend_from_slice(&chunk[..n]);
        } else if !out.is_empty() || start.elapsed() >= timeout {
            return Ok(out);
        }
    }
}

fn unlock(port: &mut File) -> Result<(), ConnectError> {
    port.write_all(b"whoareyou.MyApp8\r\n")?;
    let reply = read_reply(port, Duration::from_millis(800))?;

    // No answer means the port is still unlocked from an earlier session.
    if let Some(pos) = reply.windows(9).position(|w| w == b"whoareyou") {
        let challenge = &reply[pos + 9..];
        if challenge.len() < 36 {
            return Err(ConnectError::Unlock("The X4 sent a truncated challenge.".into()));
        }
        let mut key = [0u8; 32];
        key[..2].copy_from_slice(&challenge[..2]);
        key[2..30].copy_from_slice(&KEY_DATA);
        key[30..].copy_from_slice(&challenge[2..4]);

        let mut iv = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut iv);
        let sealed = Aes256Gcm::new_from_slice(&key)
            .expect("key is 32 bytes")
            .encrypt(Nonce::from_slice(&iv[..12]), &challenge[4..36])
            .map_err(|_| ConnectError::Unlock("Encrypting the unlock response failed.".into()))?;

        let mut msg = b"unlock".to_vec();
        msg.extend_from_slice(&iv);
        msg.extend_from_slice(&sealed);
        msg.extend_from_slice(b"\r\n");
        port.write_all(&msg)?;

        let answer = read_reply(port, Duration::from_secs(1))?;
        if !answer.starts_with(b"unlock_OK") {
            return Err(ConnectError::Unlock(format!(
                "The X4 refused the unlock response ({:?}).",
                String::from_utf8_lossy(&answer).trim()
            )));
        }
    }

    port.write_all(b"SW_MODE1\r\n")?;
    read_reply(port, Duration::from_millis(300))?;
    Ok(())
}

fn frame(cmd: u8, data: &[u8]) -> Vec<u8> {
    let mut f = vec![MARKER, cmd, data.len() as u8];
    f.extend_from_slice(data);
    f
}

/// Remove complete frames from the front of `buf`. `5b` frames (seen as stray
/// acknowledgements around `SW_MODE1`) are dropped.
fn take_frames(buf: &mut Vec<u8>) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        if buf[i] != MARKER && buf[i] != 0x5b {
            i += 1;
            continue;
        }
        if i + 3 > buf.len() {
            break;
        }
        let end = i + 3 + buf[i + 2] as usize;
        if end > buf.len() {
            break;
        }
        if buf[i] == MARKER {
            frames.push(buf[i..end].to_vec());
        }
        i = end;
    }
    buf.drain(..i);
    frames
}

fn decode(frame: &[u8]) -> Vec<Event> {
    let (cmd, p) = (frame[1], &frame[3..]);
    match cmd {
        // 11 LEN COUNT 00 then COUNT x [family id f32le]
        CMD_READ_FEATURE if p.len() >= 2 => p[2..]
            .chunks_exact(6)
            .take(p[0] as usize)
            .map(|c| (c[0], c[1], f32::from_le_bytes([c[2], c[3], c[4], c[5]])))
            .filter(|&(_, _, value)| value.is_finite())
            .map(|(family, id, value)| Event::Feature { family, id, value })
            .collect(),
        CMD_OUTPUT if p.len() >= 2 && p[0] == 0x01 => vec![Event::Output(p[1])],
        CMD_FIRMWARE if p.len() >= 2 && p[0] == 0x02 => {
            let text = p[2..].split(|&b| b == 0).next().unwrap_or_default();
            vec![Event::Firmware(String::from_utf8_lossy(text).into_owned())]
        }
        CMD_ACK if p.len() >= 2 && p[1] != 0 => vec![Event::Rejected { cmd: p[0], status: p[1] }],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_captured_frames() {
        let mut buf = vec![
            0x5b, 0x02, 0x0a, 0, 0x6d, 0, 0, 0, 0, 0, 0, 0, 0, // stray 5b ack
            0x5a, 0x11, 0x08, 0x01, 0x00, 0x96, 0x09, 0x00, 0x00, 0x80, 0x3f,
            0x5a, 0x2c, 0x05, 0x01, 0x01, 0x00, 0x00, 0x00,
            0x5a, 0x02, 0x0a, 0x06, 0x81, 0, 0, 0, 0, 0, 0, 0, 0,
            0x5a, 0x11, // incomplete
        ];
        let events: Vec<Event> = take_frames(&mut buf).iter().flat_map(|f| decode(f)).collect();
        assert_eq!(
            events,
            vec![
                Event::Feature { family: 0x96, id: 0x09, value: 1.0 },
                Event::Output(0x01),
                Event::Rejected { cmd: 0x06, status: 0x81 },
            ]
        );
        assert_eq!(buf, vec![0x5a, 0x11]);
    }

    #[test]
    fn decodes_firmware() {
        let mut f = vec![0x5a, 0x09, 0x12, 0x02, 0x10];
        f.extend_from_slice(b"1.6.231011.0830\0");
        assert_eq!(decode(&f), vec![Event::Firmware("1.6.231011.0830".into())]);
    }
}
