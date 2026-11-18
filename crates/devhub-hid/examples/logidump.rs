//! 0x8100 (onboard profiles) を読み出して表示する調査用ツール。
//!
//! HID++ の "function" は 0〜15 の番号。Solaar の表記 (0x50 など) は
//! 4bit シフト済みの値なので、ここでは function 番号そのものを使う。

use hidapi::{HidApi, HidDevice};

const DEV_INDEX: u8 = 0xff;

struct Logi {
    short: HidDevice,
    long: HidDevice,
    sw: u8,
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

impl Logi {
    fn open(api: &HidApi) -> Option<Logi> {
        let mut short = None;
        let mut long = None;
        for d in api.device_list() {
            if d.vendor_id() == 0x046d && d.product_id() == 0xc08b && d.usage_page() == 0xff00 {
                if d.usage() == 0x01 {
                    short = api.open_path(d.path()).ok();
                } else if d.usage() == 0x02 {
                    long = api.open_path(d.path()).ok();
                }
            }
        }
        let short = short?;
        let long = long?;
        short.set_blocking_mode(false).ok();
        long.set_blocking_mode(false).ok();
        Some(Logi {
            short,
            long,
            sw: 1,
        })
    }

    fn req(&mut self, feature: u8, function: u8, params: &[u8]) -> Option<Vec<u8>> {
        let sw = self.sw;
        self.sw = if self.sw == 0x0f { 1 } else { self.sw + 1 };

        let long_report = params.len() > 3;
        let len = if long_report { 20 } else { 7 };
        let mut frame = vec![0u8; len];
        frame[0] = if long_report { 0x11 } else { 0x10 };
        frame[1] = DEV_INDEX;
        frame[2] = feature;
        frame[3] = (function << 4) | sw;
        frame[4..4 + params.len()].copy_from_slice(params);

        let dev = if long_report { &self.long } else { &self.short };
        if let Err(e) = dev.write(&frame) {
            println!("write error: {e}");
            return None;
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while std::time::Instant::now() < deadline {
            for dev in [&self.long, &self.short] {
                let mut buf = [0u8; 32];
                let Ok(n) = dev.read_timeout(&mut buf, 40) else {
                    continue;
                };
                if n < 7 || buf[1] != DEV_INDEX || (buf[3] & 0x0f) != sw {
                    continue;
                }
                if buf[2] == 0xff {
                    println!("hidpp error: feature={:02x} code={:02x}", buf[3], buf[5]);
                    return None;
                }
                if buf[2] != feature {
                    continue;
                }
                let end = if buf[0] == 0x10 { 7 } else { 20 };
                return Some(buf[4..end].to_vec());
            }
        }
        println!("timeout: feature={feature:02x} function={function}");
        None
    }

    fn root(&mut self, feature_id: u16) -> Option<u8> {
        self.req(0x00, 0x00, &[(feature_id >> 8) as u8, feature_id as u8])
            .map(|r| r[0])
    }
}

fn read_sector(dev: &mut Logi, feature: u8, sector: u16, size: u16) -> Vec<u8> {
    let mut out = Vec::new();
    let mut offset: u16 = 0;
    while offset < size {
        let params = [
            (sector >> 8) as u8,
            sector as u8,
            (offset >> 8) as u8,
            offset as u8,
        ];
        match dev.req(feature, 5, &params) {
            Some(chunk) => out.extend_from_slice(&chunk),
            None => break,
        }
        offset += 16;
    }
    out.truncate(size as usize);
    out
}

fn main() {
    let api = HidApi::new().unwrap();
    let Some(mut dev) = Logi::open(&api) else {
        println!("G502 HERO の HID++ インターフェースが見つかりません");
        return;
    };

    let Some(feature) = dev.root(0x8100) else {
        println!("0x8100 がありません");
        return;
    };
    println!("0x8100 feature index = {feature:02x}");

    let header = dev.req(feature, 0, &[]).unwrap_or_default();
    println!("header fn0 : {}", hex(&header));
    let count = header.get(3).copied().unwrap_or(0);
    let buttons = header.get(5).copied().unwrap_or(0);
    let sectors = header.get(6).copied().unwrap_or(0);
    let size = u16::from_be_bytes([header.get(7).copied().unwrap_or(0), header.get(8).copied().unwrap_or(0)]);
    println!("profiles={count} buttons={buttons} sectors={sectors} size={size}");

    if let Some(mode) = dev.req(feature, 2, &[]) {
        println!("mode fn2   : {}", hex(&mode));
    }
    if let Some(active) = dev.req(feature, 4, &[]) {
        println!("active fn4 : {}", hex(&active));
    }

    // プロファイルヘッダ (sector, enabled) の一覧。
    let mut headers = Vec::new();
    let mut index = 0u16;
    loop {
        let offset = index * 4;
        let params = [
            0,
            0,
            (offset >> 8) as u8,
            offset as u8,
        ];
        let Some(chunk) = dev.req(feature, 5, &params) else {
            break;
        };
        let sector = u16::from_be_bytes([chunk[0], chunk[1]]);
        if sector == 0xffff {
            break;
        }
        let enabled = chunk[2];
        println!("profile[{index}] sector={sector} enabled={enabled}");
        headers.push((sector, enabled));
        index += 1;
        if index > 32 {
            break;
        }
    }

    if size == 0 {
        return;
    }
    let control = read_sector(&mut dev, feature, 0, size);
    std::fs::create_dir_all("_probe").ok();
    std::fs::write("_probe/sector_0.bin", &control).ok();
    println!("control sector ({}B):", control.len());
    for (i, chunk) in control.chunks(16).enumerate() {
        println!("  {:04x}: {}", i * 16, hex(chunk));
    }

    for (sector, enabled) in headers {
        let bytes = read_sector(&mut dev, feature, sector, size);
        std::fs::write(format!("_probe/sector_{sector}.bin"), &bytes).ok();
        println!("--- profile sector {sector} enabled={enabled} ({}B) ---", bytes.len());
        for (i, chunk) in bytes.chunks(16).enumerate() {
            println!("  {:04x}: {}", i * 16, hex(chunk));
        }
    }
}
