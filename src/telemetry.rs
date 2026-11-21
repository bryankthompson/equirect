//! Tells the Handy bridge (`tools/handy-sync`) where the video is, so the Handy can follow its funscript.
//!
//! One small UDP datagram, about four a second and at once on any change of video, play/pause or speed:
//! `{"v":1,"seq":N,"video":"<source>","t":<ms>,"playing":true,"rate":1.0,"haptics":{"offset_ms":0,"stroke_min":0,"stroke_max":100,"invert":false}}`.
//! Where it goes: `EQUIRECT_HANDY_ADDR` (`host:port`, default `127.0.0.1:17777`); `off` turns it off.
//! A bridge running in WSL cannot be reached on 127.0.0.1 over UDP, so the launcher sets the WSL address.

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use crate::haptics::Haptics;

const INTERVAL: Duration = Duration::from_millis(250);
/// A changed haptics setting must stay the same this long before it is sent, so dragging a slider does not
/// make the bridge re-sync the Handy on every frame.
const SETTLE: Duration = Duration::from_millis(300);

/// The haptics settings of the video now playing: its offset in ms, and the stroke range and invert for all videos.
pub type HapticsNow = (i32, Haptics);
const DEFAULT_ADDR: &str = "127.0.0.1:17777";

pub struct Telemetry {
    sock: UdpSocket,
    target: String,
    seq: u64,
    last_sent: Option<Instant>,
    last_state: Option<(String, bool, u32, HapticsNow)>,
    /// The haptics settings last sent, and a newer value waiting to hold still for `SETTLE`.
    settled: Option<HapticsNow>,
    pending: Option<(HapticsNow, Instant)>,
    /// True while sends are failing, so the error is logged once, not four times a second.
    failing: bool,
}

impl Telemetry {
    /// From the environment; `None` when switched off or the socket cannot be made (both logged).
    pub fn from_env() -> Option<Self> {
        let addr = std::env::var("EQUIRECT_HANDY_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());
        if addr.eq_ignore_ascii_case("off") {
            eprintln!("Handy telemetry: off (EQUIRECT_HANDY_ADDR=off)");
            return None;
        }
        match Self::to(&addr) {
            Ok(t) => {
                eprintln!("Handy telemetry: sending the play position to {addr}");
                Some(t)
            }
            Err(e) => {
                eprintln!("Handy telemetry: disabled, cannot open a socket for {addr}: {e}");
                None
            }
        }
    }

    pub fn to(addr: &str) -> std::io::Result<Self> {
        let sock = UdpSocket::bind("0.0.0.0:0")?;
        sock.set_nonblocking(true)?;
        Ok(Self { sock, target: addr.to_string(), seq: 0, last_sent: None, last_state: None, settled: None, pending: None, failing: false })
    }

    /// Call every frame. Sends when 250 ms have passed or video / playing / speed changed.
    pub fn tick(&mut self, video: &str, t_secs: f64, playing: bool, rate: f32, haptics: HapticsNow) {
        if self.last_state.as_ref().map_or(true, |s| s.0 != video) {
            self.settled = None; // a new video: its own offset goes out at once
        }
        let haptics = self.settle(haptics);
        let state = (video.to_string(), playing, rate.to_bits(), haptics);
        let changed = self.last_state.as_ref() != Some(&state);
        let due = self.last_sent.map_or(true, |t| t.elapsed() >= INTERVAL);
        if changed || due {
            self.send(video, t_secs, playing, rate, &state.3);
            self.last_state = Some(state);
        }
    }

    /// The haptics value to send: the first one at once, a changed one only after it held still for `SETTLE`.
    fn settle(&mut self, now: HapticsNow) -> HapticsNow {
        if self.pending.as_ref().map(|(h, _)| h) != Some(&now) {
            self.pending = Some((now, Instant::now()));
        }
        let (h, since) = self.pending.as_ref().expect("set just above");
        if self.settled.is_none() || since.elapsed() >= SETTLE {
            self.settled = Some(h.clone());
        }
        self.settled.clone().expect("set just above")
    }

    /// One last "not playing" so the Handy stops at once instead of waiting for the bridge's timeout.
    pub fn stop(&mut self, video: &str, t_secs: f64, rate: f32) {
        let haptics = self.settled.clone().unwrap_or_else(|| (0, Haptics::default()));
        self.send(video, t_secs, false, rate, &haptics);
    }

    fn send(&mut self, video: &str, t_secs: f64, playing: bool, rate: f32, haptics: &HapticsNow) {
        self.seq += 1;
        self.last_sent = Some(Instant::now());
        let body = serde_json::json!({
            "v": 1,
            "seq": self.seq,
            "video": video,
            "t": (t_secs.max(0.0) * 1000.0).round() as u64,
            "playing": playing,
            "rate": rate,
            "haptics": {
                "offset_ms": haptics.0,
                "stroke_min": haptics.1.stroke_min,
                "stroke_max": haptics.1.stroke_max,
                "invert": haptics.1.invert,
                "auto": haptics.1.auto,
            },
        })
        .to_string();
        match self.sock.send_to(body.as_bytes(), &self.target) {
            Ok(_) => {
                if self.failing {
                    eprintln!("Handy telemetry: sending again");
                    self.failing = false;
                }
            }
            Err(e) => {
                if !self.failing {
                    eprintln!("Handy telemetry: cannot send to {}: {e}", self.target);
                    self.failing = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_haptics() -> HapticsNow { (0, Haptics::default()) }

    fn pair() -> (UdpSocket, Telemetry) {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        let t = Telemetry::to(&rx.local_addr().unwrap().to_string()).unwrap();
        (rx, t)
    }

    fn recv(rx: &UdpSocket) -> Option<serde_json::Value> {
        let mut buf = [0u8; 1024];
        let n = rx.recv(&mut buf).ok()?;
        serde_json::from_slice(&buf[..n]).ok()
    }

    #[test]
    fn message_has_the_agreed_fields() {
        let (rx, mut t) = pair();
        t.tick(r"G:\vr\clip.mp4", 12.3456, true, 0.5, no_haptics());
        let m = recv(&rx).expect("a datagram");
        assert_eq!(m["v"], 1);
        assert_eq!(m["seq"], 1);
        assert_eq!(m["video"], r"G:\vr\clip.mp4");
        assert_eq!(m["t"], 12346);
        assert_eq!(m["playing"], true);
        assert_eq!(m["rate"], 0.5);
    }

    #[test]
    fn sends_on_change_but_not_twice_inside_the_interval() {
        let (rx, mut t) = pair();
        t.tick("a.mp4", 1.0, true, 1.0, no_haptics());
        assert!(recv(&rx).is_some());
        t.tick("a.mp4", 1.01, true, 1.0, no_haptics()); // same state, too soon: nothing
        assert!(recv(&rx).is_none());
        t.tick("a.mp4", 1.02, false, 1.0, no_haptics()); // paused: at once
        assert_eq!(recv(&rx).unwrap()["playing"], false);
        t.tick("b.mp4", 0.0, true, 1.0, no_haptics()); // new video: at once, higher seq
        let m = recv(&rx).unwrap();
        assert_eq!(m["video"], "b.mp4");
        assert_eq!(m["seq"], 3);
    }

    #[test]
    fn haptics_ride_in_every_message_and_a_change_waits_until_it_holds_still() {
        let (rx, mut t) = pair();
        let mut h = Haptics { stroke_min: 20, stroke_max: 80, invert: true, ..Haptics::default() };
        h.auto.mode = crate::haptics::AutoMode::NoScript;
        let first: HapticsNow = (-120, h);
        t.tick("a.mp4", 1.0, true, 1.0, first.clone());
        let m = recv(&rx).unwrap();
        assert_eq!(m["haptics"]["offset_ms"], -120);
        assert_eq!((m["haptics"]["stroke_min"].clone(), m["haptics"]["stroke_max"].clone(), m["haptics"]["invert"].clone()),
                   (20.into(), 80.into(), true.into()));
        assert_eq!(m["haptics"]["auto"]["mode"], "no_script", "the auto-mode settings go with it");
        assert!(m["haptics"]["auto"]["preset"].is_string() && m["haptics"]["auto"]["speed"].is_array());

        // The offset slider moves: the old value keeps going out (nothing is sent yet, state unchanged).
        let moved: HapticsNow = (-100, first.1.clone());
        t.tick("a.mp4", 1.01, true, 1.0, moved.clone());
        assert!(recv(&rx).is_none(), "a value that just changed is not sent yet");

        // It held still for SETTLE: now it goes out at once.
        std::thread::sleep(SETTLE + Duration::from_millis(50));
        t.tick("a.mp4", 1.4, true, 1.0, moved.clone());
        assert_eq!(recv(&rx).unwrap()["haptics"]["offset_ms"], -100);

        // A new video sends its own offset at once, without waiting.
        t.tick("b.mp4", 0.0, true, 1.0, (40, first.1.clone()));
        assert_eq!(recv(&rx).unwrap()["haptics"]["offset_ms"], 40);
    }

    #[test]
    fn stop_sends_not_playing() {
        let (rx, mut t) = pair();
        t.stop("a.mp4", 5.0, 1.0);
        assert_eq!(recv(&rx).unwrap()["playing"], false);
    }
}
