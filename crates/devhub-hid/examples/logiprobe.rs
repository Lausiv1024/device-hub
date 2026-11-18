//! Logitech HID++ 2.0 プロトコル解析用プローブ。
//!
//! G502 HERO は HID++ を 2 つのコレクションに分けて公開する:
//!   - usage 0xFF00/0x01 (Col04) … short report (ID 0x10, 7B)
//!   - usage 0xFF00/0x02 (Col05) … long  report (ID 0x11, 20B)
//!
//!   cargo run -p devhub-hid --example logiprobe

use hidapi::{HidApi, HidDevice};
use std::time::Duration;

const VID: u16 = 0x046d;
const PID: u16 = 0xc08b;
const DEV_INDEX: u8 = 0xff;

struct Hidpp {
    short: HidDevice,
    long: HidDevice,
    sw: u8,
}

fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

impl Hidpp {
    fn open(api: &HidApi) -> Option<Hidpp> {
        let mut short = None;
        let mut long = None;
        for d in api.device_list() {
            if d.vendor_id() != VID || d.product_id() != PID || d.usage_page() != 0xff00 {
                continue;
            }
            match d.usage() {
                0x01 => short = api.open_path(d.path()).ok(),
                0x02 => long = api.open_path(d.path()).ok(),
                _ => {}
            }
        }
        for d in [&short, &long].into_iter().flatten() {
            d.set_blocking_mode(false).ok();
        }
        Some(Hidpp {
            short: short?,
            long: long?,
            sw: 1,
        })
    }

    fn read_any(&self) -> Option<[u8; 32]> {
        for dev in [&self.long, &self.short] {
            let mut r = [0u8; 32];
            if let Ok(n) = dev.read_timeout(&mut r, 60) {
                if n >= 7 {
                    return Some(r);
                }
            }
        }
        None
    }

    fn request(&mut self, feature_idx: u8, function: u8, params: &[u8]) -> Option<Vec<u8>> {
        let sw = self.sw;
        self.sw = if self.sw >= 0x0f { 1 } else { self.sw + 1 };
        let (dev, buf) = if params.len() <= 3 {
            let mut b = [0u8; 7];
            b[0] = 0x10;
            b[1] = DEV_INDEX;
            b[2] = feature_idx;
            b[3] = (function << 4) | sw;
            b[4..4 + params.len()].copy_from_slice(params);
            (&self.short, b.to_vec())
        } else {
            let mut b = [0u8; 20];
            b[0] = 0x11;
            b[1] = DEV_INDEX;
            b[2] = feature_idx;
            b[3] = (function << 4) | sw;
            b[4..4 + params.len()].copy_from_slice(params);
            (&self.long, b.to_vec())
        };
        if let Err(e) = dev.write(&buf) {
            println!("  write(fi={feature_idx:02x} fn={function:02x}) error: {e}");
            return None;
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(400);
        while std::time::Instant::now() < deadline {
            let r = match self.read_any() {
                Some(r) => r,
                None => continue,
            };
            if r[1] != 0xff {
                continue;
            }
            if r[2] == 0xff {
                return Some(vec![0xff, r[3], r[4], r[5]]);
            }
            if r[2] != feature_idx {
                continue;
            }
            if (r[3] & 0x0f) != sw {
                continue;
            }
            let len = if r[0] == 0x10 { 7 } else { 20 };
            return Some(r[4..len].to_vec());
        }
        None
    }

    fn root_feature(&mut self, feature_id: u16) -> Option<u8> {
        let p = [(feature_id >> 8) as u8, (feature_id & 0xff) as u8];
        let r = self.request(0x00, 0x00, &p)?;
        if r[0] == 0xff && r.len() >= 4 {
            println!("  root({feature_id:04x}) ERROR code={:02x}", r[3]);
            return None;
        }
        Some(r[0])
    }
}

fn main() {
    let api = HidApi::new().unwrap();
    let mut h = match Hidpp::open(&api) {
        Some(h) => h,
        None => {
            println!("G502 HERO の HID++ インターフェースが見つかりません");
            return;
        }
    };

    println!("--- root getFeature 応答 ---");
    for id in [0x0000u16, 0x0001, 0x0003, 0x0005, 0x1b04, 0x2201, 0x8060, 0x8071, 0x8100] {
        let p = [(id >> 8) as u8, (id & 0xff) as u8];
        match h.request(0x00, 0x00, &p) {
            Some(r) => println!("  0x{id:04x} -> {}", hex(&r)),
            None => println!("  0x{id:04x} -> (no reply)"),
        }
    }

    let fs_idx = h.root_feature(0x0001).unwrap_or(0);
    println!("IFeatureSet index = {fs_idx:02x}");
    if fs_idx != 0 {
        let cnt = h.request(fs_idx, 0x00, &[]).map(|r| r[0]).unwrap_or(0);
        println!("feature count = {cnt}");
        let mut features = Vec::new();
        for i in 0..cnt {
            if let Some(r) = h.request(fs_idx, 0x01, &[i]) {
                if r[0] == 0xff {
                    break;
                }
                let id = ((r[0] as u16) << 8) | r[1] as u16;
                let idx = h.root_feature(id).unwrap_or(0);
                println!("  [{i:02}] id={id:04x} idx={idx:02x} flags={:02x} type={}", r[2], r.get(3).copied().unwrap_or(0));
                features.push((id, idx));
            }
        }
    }

    if let Some(idx) = h.root_feature(0x2201) {
        if idx != 0 {
            println!("--- AdjustableDpi idx={idx:02x} ---");
            for (f, p) in [(0x00u8, vec![]), (0x01, vec![0]), (0x02, vec![0])] {
                if let Some(r) = h.request(idx, f, &p) {
                    println!("  fn {f:02x}: {}", hex(&r));
                }
            }
        }
    }
    if let Some(idx) = h.root_feature(0x8060) {
        if idx != 0 {
            println!("--- ReportRate idx={idx:02x} ---");
            for (f, p) in [(0x00u8, vec![]), (0x01, vec![]), (0x10, vec![])] {
                if let Some(r) = h.request(idx, f, &p) {
                    println!("  fn {f:02x}: {}", hex(&r));
                }
            }
        }
    }
    if let Some(idx) = h.root_feature(0x8071) {
        if idx != 0 {
            println!("--- RgbEffectsV2 idx={idx:02x} ---");
            for (f, p) in [(0x00u8, vec![0xff, 0xff, 0x00]), (0x00, vec![0, 0, 0])] {
                if let Some(r) = h.request(idx, f, &p) {
                    println!("  fn {f:02x} p={}: {}", hex(&p), hex(&r));
                }
            }
        }
    }
    if let Some(idx) = h.root_feature(0x1b04) {
        if idx != 0 {
            println!("--- ReprogControlsV4 idx={idx:02x} ---");
            if let Some(r) = h.request(idx, 0x00, &[]) {
                let n = r[0];
                println!("  count={n}");
                for i in 0..n {
                    if let Some(c) = h.request(idx, 0x10, &[i]) {
                        let cid = ((c[0] as u16) << 8) | c[1] as u16;
                        let task = ((c[2] as u16) << 8) | c[3] as u16;
                        println!("   [{i:02}] cid={cid:04x} task={task:04x} flags={:02x} pos={} group={:02x} mask={:02x} add={:02x}", c[4], c[5], c[6], c[7], c[8]);
                    }
                }
            }
        }
    }
    if let Some(idx) = h.root_feature(0x8100) {
        if idx != 0 {
            println!("--- 0x8100 idx={idx:02x} ---");
            for (f, p) in [(0x00u8, vec![]), (0x10, vec![]), (0x20, vec![]), (0x30, vec![0])] {
                if let Some(r) = h.request(idx, f, &p) {
                    println!("  fn {f:02x} p={}: {}", hex(&p), hex(&r));
                }
            }
        }
    }
    if let Some(idx) = h.root_feature(0x0003) {
        if idx != 0 {
            println!("--- DeviceFwVersion idx={idx:02x} ---");
            for (f, p) in [(0x00u8, vec![]), (0x10, vec![0]), (0x20, vec![0]), (0x30, vec![0]), (0x40, vec![0]), (0x50, vec![0])] {
                if let Some(r) = h.request(idx, f, &p) {
                    println!("  fn {f:02x}: {}", hex(&r));
                }
            }
        }
    }
    let _ = Duration::from_millis(1);
}
