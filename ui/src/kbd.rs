//! Depth keyboard status over raw HID (hardware/keyboard/qmk/users/depth/depth.h): a thread finds a
//! QMK keyboard whose raw HID interface (usage page 0xFF60, usage 0x61) answers the Depth hello and
//! sends it the state its lights show, on change and every second. Keyboards that do not answer are
//! left alone; nothing is sent to them again until the device list changes.
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAGIC: u8 = 0xD7;

/// What the keyboard shows; see depth.h for the bits.
#[derive(Clone, Copy, PartialEq, Default, Debug)]
pub struct State { pub flags: u8, pub market: u8, pub tf: u8 }

pub const LIVE: u8 = 1;
pub const POS: u8 = 2;
pub const LOSS: u8 = 4;
pub const ORDERS: u8 = 8;
pub const ATTN: u8 = 16;
pub const CLOSE: u8 = 32;

pub struct Kbd { state: Arc<Mutex<State>> }

impl Kbd {
    pub fn start() -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        let s = state.clone();
        let _ = std::thread::Builder::new().name("depth-keyboard".into()).spawn(move || run(s));
        Kbd { state }
    }

    /// The latest state; the thread sends it when it changed (cheap, call every frame).
    pub fn set(&self, st: State) { *self.state.lock().unwrap() = st; }
}

/// One 32-byte report behind report id 0.
fn report(body: &[u8]) -> [u8; 33] {
    let mut r = [0u8; 33];
    r[1..1 + body.len()].copy_from_slice(body);
    r
}

fn run(state: Arc<Mutex<State>>) {
    let Ok(mut api) = hidapi::HidApi::new() else { return };
    let mut rejected: Vec<std::ffi::CString> = vec![];
    loop {
        // find one: the raw HID interfaces, each asked once
        let _ = api.refresh_devices();
        let found = api.device_list()
            .filter(|d| d.usage_page() == 0xFF60 && d.usage() == 0x61 && !rejected.iter().any(|p| p.as_c_str() == d.path()))
            .map(|d| d.path().to_owned()).collect::<Vec<_>>();
        let mut dev = None;
        for path in found {
            let Ok(d) = api.open_path(&path) else { continue };
            let ok = d.write(&report(&[MAGIC, 0x00])).is_ok() && {
                let mut buf = [0u8; 32];
                // the answer can trail other raw HID traffic briefly
                let until = Instant::now() + Duration::from_millis(500);
                let mut hit = false;
                while Instant::now() < until {
                    match d.read_timeout(&mut buf, 100) { Ok(n) if n >= 3 && buf[0] == MAGIC && buf[1] == 0x00 => { hit = true; break; } Ok(_) => {} Err(_) => break }
                }
                hit
            };
            if ok { dev = Some(d); break; } else { rejected.push(path); }
        }
        let Some(d) = dev else {
            std::thread::sleep(Duration::from_secs(3));
            continue;
        };
        // connected: send on change, and every second so the keyboard knows Depth is alive
        let (mut sent, mut at) = (None, Instant::now() - Duration::from_secs(10));
        loop {
            let st = *state.lock().unwrap();
            if Some(st) != sent || at.elapsed() >= Duration::from_secs(1) {
                if d.write(&report(&[MAGIC, 0x01, st.flags, st.market, st.tf])).is_err() { break; }
                (sent, at) = (Some(st), Instant::now());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // unplugged: look again (a replugged keyboard gets a new path, so it is asked again)
        rejected.clear();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn reports_carry_the_report_id_then_the_message() {
        let r = super::report(&[0xD7, 0x01, 5, 2, 3]);
        assert_eq!(r.len(), 33);
        assert_eq!(&r[..6], &[0, 0xD7, 0x01, 5, 2, 3]);
    }
}
