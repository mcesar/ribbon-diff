//! Ask the terminal for its background and palette colors (OSC 11 and OSC 4), so the ribbon
//! images match the terminal's color scheme, and whether it shows Kitty graphics. Call in raw
//! mode, before reading input events.

use crate::highlight::Rgb;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TermColors {
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    pub red: Option<Rgb>,
    pub green: Option<Rgb>,
    pub blue: Option<Rgb>,
    /// The terminal answered the Kitty graphics query with OK.
    pub kitty_graphics: bool,
}

const TIMEOUT: Duration = Duration::from_millis(400);

/// A Kitty graphics query: a 1x1 RGB image that is checked but not stored. Terminals that
/// support the protocol answer `ESC _ G i=31;OK ESC \`; others ignore it.
const KITTY_QUERY: &str = "\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\";

/// Query the terminal. Every terminal answers the primary device attributes request (`CSI c`),
/// so its reply marks the end of the color replies, and a terminal that ignores the color
/// queries costs no waiting.
pub fn query() -> TermColors {
    let Ok(mut tty) = OpenOptions::new().read(true).write(true).open("/dev/tty") else {
        return TermColors::default();
    };
    let request = format!("\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b]4;1;?\x1b\\\x1b]4;2;?\x1b\\\x1b]4;4;?\x1b\\{KITTY_QUERY}\x1b[c");
    if tty.write_all(request.as_bytes()).and_then(|()| tty.flush()).is_err() {
        return TermColors::default();
    }

    let mut reply = Vec::new();
    let deadline = Instant::now() + TIMEOUT;
    while !has_device_attributes(&reply) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || !readable(&tty, left) {
            break;
        }
        let mut chunk = [0u8; 512];
        match tty.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => reply.extend_from_slice(&chunk[..n]),
        }
    }
    parse(&String::from_utf8_lossy(&reply))
}

/// Wait until the tty has input. Uses select(): macOS poll() does not support /dev/tty and
/// reports it ready at once, which would leave the read below blocked forever.
fn readable(tty: &std::fs::File, wait: Duration) -> bool {
    let fd = tty.as_raw_fd();
    let mut timeout = libc::timeval {
        tv_sec: wait.as_secs() as libc::time_t,
        tv_usec: wait.subsec_micros() as libc::suseconds_t,
    };
    // SAFETY: the fd_set is initialized by FD_ZERO and holds one open descriptor below FD_SETSIZE.
    unsafe {
        let mut set: libc::fd_set = std::mem::zeroed();
        libc::FD_ZERO(&mut set);
        libc::FD_SET(fd, &mut set);
        libc::select(fd + 1, &mut set, std::ptr::null_mut(), std::ptr::null_mut(), &mut timeout) > 0
            && libc::FD_ISSET(fd, &set)
    }
}

/// `ESC [ ? ... c`
fn has_device_attributes(reply: &[u8]) -> bool {
    reply.windows(3).enumerate().any(|(start, w)| {
        w[0] == 0x1b && w[1] == b'[' && w[2] == b'?' && reply[start..].iter().any(|&b| b == b'c')
    })
}

fn parse(reply: &str) -> TermColors {
    let color_after = |prefix: &str| reply.find(prefix).and_then(|at| parse_rgb(&reply[at + prefix.len()..]));
    TermColors {
        foreground: color_after("]10;rgb:"),
        background: color_after("]11;rgb:"),
        red: color_after("]4;1;rgb:"),
        green: color_after("]4;2;rgb:"),
        blue: color_after("]4;4;rgb:"),
        kitty_graphics: reply.contains("_Gi=31;OK"),
    }
}

/// `rrrr/gggg/bbbb` with 1 to 4 hex digits per channel, scaled to 8 bits.
fn parse_rgb(text: &str) -> Option<Rgb> {
    let mut channels = text.splitn(3, '/');
    let mut channel = || {
        let digits: String = channels.next()?.chars().take_while(char::is_ascii_hexdigit).collect();
        let value = u32::from_str_radix(&digits, 16).ok()?;
        let max = (1u32 << (4 * digits.len() as u32)) - 1;
        Some((value * 255 / max) as u8)
    };
    Some([channel()?, channel()?, channel()?])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_replies_in_any_order() {
        let reply = "\x1b]10;rgb:cafe/d3d3/f5f5\x1b\\\x1b]4;2;rgb:a6a6/dada/9595\x1b\\\x1b]11;rgb:2424/2727/3a3a\x1b\\\x1b]4;1;rgb:ed/87/96\x07\x1b[?62;22c";
        let colors = parse(reply);
        assert_eq!(colors.foreground, Some([0xca, 0xd3, 0xf5]));
        assert_eq!(colors.background, Some([0x24, 0x27, 0x3a]));
        assert_eq!(colors.green, Some([0xa6, 0xda, 0x95]));
        assert_eq!(colors.red, Some([0xed, 0x87, 0x96]));
        assert_eq!(colors.blue, None);
        assert!(!colors.kitty_graphics);
        assert!(parse("\x1b_Gi=31;OK\x1b\\\x1b[?62c").kitty_graphics);
        assert!(has_device_attributes(reply.as_bytes()));
    }
}
