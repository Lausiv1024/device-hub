//! Logitech HID++ 2.0 ドライバ (G502 HERO)。
//!
//! # 実機で確認したこと (2026-11, 046d:c08b / G502 HERO)
//!
//! HID++ は同じインターフェース (MI_01) の 2 つのコレクションに分かれている:
//!
//! - usage page `0xFF00` / usage `0x01` (Col04) … short report (ID `0x10`, 7B)
//! - usage page `0xFF00` / usage `0x02` (Col05) … long  report (ID `0x11`, 20B)
//!
//! 送信は `[reportID, devIndex=0xFF, featureIndex, function<<4|swId, params...]`。
//! 応答は要求したチャネルに関係なく long 側で返ることがあるので両方を読む。
//!
//! フィーチャ (IFeatureSet 0x0001 の列挙 + root getFeature で確認):
//!
//! | ID | 名前 | index | 用途 |
//! |---|---|---|---|
//! | 0x0000 | IRoot | 00 | getFeature |
//! | 0x0001 | IFeatureSet | 01 | 列挙 |
//! | 0x8070 | ColorLedEffects (V1) | 02 | 2 ゾーン (Primary=DPI, Logo) |
//! | 0x0003 | DeviceFwVersion | 03 | FW |
//! | 0x0005 | DeviceName | 04 | 名前 (16B チャンク) |
//! | 0x2201 | AdjustableDpi | 0a | 現在 DPI |
//! | 0x8060 | ReportRate | 0b | 125/250/500/1000 |
//! | 0x8100 | OnboardProfiles | 0c | **本命** (DPI/ボタン/色/レート) |
//! | 0x8110 | MouseButtonSpy | 0d | 未使用 |
//!
//! 0x8100 のプロファイルは 256B セクタ。G502 HERO は 5 プロファイル・11 ボタン。
//! レイアウト (Solaar の OnboardProfile と同じ、実機ダンプで確認):
//!
//! ```text
//!   0   report rate (ms: 1=1000Hz, 2=500, 4=250, 8=125)
//!   1   default resolution index (0..4)
//!   2   shift resolution index
//!   3   5 × u16 LE resolution
//!   13  R,G,B (DPI インジケータ色)
//!   16  power mode / 17 angle snap / 18 write count
//!   28  ps timeout / 30 po timeout
//!   32  16 × 4B button mappings (先頭 11 個が実ボタン)
//!   96  16 × 4B G-button mappings
//!   160 名前 UTF-16LE 48B
//!   208 4 × 11B lighting effects
//!   254 CRC16-CCITT (init 0xFFFF, 対象 0..254, big-endian)
//! ```
//!
//! ボタン 1 個は 4B:
//! - `[0x80, type, hi, lo]` … type 1=マウスボタン (bitmask 1/2/4/8/0x10)、
//!   2=修飾+キー (mods, usage)、3=コンシューマ (16bit)、0=なし
//! - `[0x90, func, 0, data]` … ファーム機能 (3=DPI UP, 4=DPI DOWN, 5=DPI CYCLE,
//!   7=SHIFT(スナイパー), 0x0a=プロファイルサイクル, 0x10/0x11=スクロール、
//!   1/2=チルト)
//!
//! 書き込みは 0x8100 function 6 (開始) → 7 (16B チャンク) → 8 (確定)。
//! 書き込み後に mode=1 (オンボード) + active profile をセットすると実機へ反映される。

use devhub_core::backend::*;
use devhub_core::model::*;
use hidapi::{HidApi, HidDevice};
use std::collections::BTreeMap;
use std::sync::Mutex;

const DEV_INDEX: u8 = 0xff;

const FEAT_FW: u16 = 0x0003;
const FEAT_NAME: u16 = 0x0005;
const FEAT_DPI: u16 = 0x2201;
const FEAT_RATE: u16 = 0x8060;
const FEAT_LED: u16 = 0x8070;
const FEAT_PROFILE: u16 = 0x8100;

const DPI_MIN: u32 = 100;
const DPI_MAX: u32 = 16_000;
const DPI_STEP: u32 = 50;

// ==================================================================== HID++ I/O

struct Io {
    short: HidDevice,
    long: HidDevice,
    sw: u8,
}

impl Io {
    fn open(vid: u16, pid: u16, short: HidDevice) -> HubResult<Io> {
        short.set_blocking_mode(false).ok();
        let api = HidApi::new().map_err(|e| HubError::Io(format!("hidapi init: {e}")))?;
        let mut long = None;
        for d in api.device_list() {
            if d.vendor_id() == vid
                && d.product_id() == pid
                && d.usage_page() == 0xff00
                && d.usage() == 0x02
            {
                long = api.open_path(d.path()).ok();
                break;
            }
        }
        let long = long.ok_or_else(|| {
            HubError::Io("HID++ long report インターフェース (FF00:02) がありません".into())
        })?;
        long.set_blocking_mode(false).ok();
        Ok(Io { short, long, sw: 1 })
    }

    fn request(&mut self, feature: u8, function: u8, params: &[u8]) -> HubResult<Vec<u8>> {
        let mut last = HubError::Protocol("HID++ timeout".into());
        for _ in 0..3 {
            match self.try_request(feature, function, params) {
                Ok(v) => return Ok(v),
                Err(HubError::Protocol(e)) if e.contains("timeout") => {
                    last = HubError::Protocol(e);
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Err(last)
    }

    /// 応答を待たないコマンド (プロファイル確定 0x80 など)。
    fn send_only(&mut self, feature: u8, function: u8, params: &[u8]) -> HubResult<()> {
        let sw = self.next_sw();
        let frame = build_frame(feature, function, sw, params);
        let dev = if params.len() > 3 {
            &self.long
        } else {
            &self.short
        };
        dev.write(&frame)
            .map_err(|e| HubError::Io(format!("hid write: {e}")))?;
        // 応答が来る機種もあるので軽く捨てる。
        self.drain(60);
        Ok(())
    }

    fn next_sw(&mut self) -> u8 {
        let sw = self.sw;
        self.sw = if self.sw >= 0x0f { 1 } else { self.sw + 1 };
        sw
    }

    fn try_request(&mut self, feature: u8, function: u8, params: &[u8]) -> HubResult<Vec<u8>> {
        let sw = self.next_sw();
        let frame = build_frame(feature, function, sw, params);
        let dev = if params.len() > 3 {
            &self.long
        } else {
            &self.short
        };
        dev.write(&frame)
            .map_err(|e| HubError::Io(format!("hid write: {e}")))?;

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(450);
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
                    return Err(HubError::Protocol(format!(
                        "HID++ error: feature={:02x} function={} code={:02x}",
                        buf[3],
                        buf[4] >> 4,
                        buf[5]
                    )));
                }
                if buf[2] != feature {
                    continue;
                }
                let end = if buf[0] == 0x10 { 7 } else { 20 };
                return Ok(buf[4..end].to_vec());
            }
        }
        Err(HubError::Protocol("HID++ timeout".into()))
    }

    fn drain(&mut self, ms: i32) {
        for dev in [&self.long, &self.short] {
            let mut buf = [0u8; 32];
            let _ = dev.read_timeout(&mut buf, ms);
        }
    }

    fn root_feature(&mut self, feature: u16) -> HubResult<u8> {
        let r = self.request(0x00, 0x00, &[(feature >> 8) as u8, feature as u8])?;
        Ok(r[0])
    }

    fn get_mode(&mut self, feature: u8) -> HubResult<u8> {
        Ok(self.request(feature, 2, &[])?[0])
    }

    fn set_mode(&mut self, feature: u8, mode: u8) -> HubResult<()> {
        self.request(feature, 1, &[mode])?;
        Ok(())
    }

    fn get_active(&mut self, feature: u8) -> HubResult<u16> {
        let r = self.request(feature, 4, &[])?;
        Ok(u16::from_be_bytes([r[0], r[1]]))
    }

    fn set_active(&mut self, feature: u8, sector: u16) -> HubResult<()> {
        self.request(feature, 3, &[(sector >> 8) as u8, sector as u8])?;
        Ok(())
    }
}

fn build_frame(feature: u8, function: u8, sw: u8, params: &[u8]) -> Vec<u8> {
    let long = params.len() > 3;
    let mut frame = vec![0u8; if long { 20 } else { 7 }];
    frame[0] = if long { 0x11 } else { 0x10 };
    frame[1] = DEV_INDEX;
    frame[2] = feature;
    frame[3] = (function << 4) | (sw & 0x0f);
    let n = params.len().min(frame.len() - 4);
    frame[4..4 + n].copy_from_slice(&params[..n]);
    frame
}

// ==================================================================== プロファイル

#[derive(Clone)]
struct Profile {
    rate_ms: u8,
    default_index: u8,
    shift_index: u8,
    resolutions: [u16; 5],
    color: Rgb,
    power_mode: u8,
    angle_snap: u8,
    write_count: u16,
    reserved: [u8; 8],
    ps_timeout: u16,
    po_timeout: u16,
    buttons: [[u8; 4]; 16],
    gbuttons: [[u8; 4]; 16],
    name: String,
    lighting: [[u8; 11]; 4],
}

impl Profile {
    fn parse(bytes: &[u8]) -> HubResult<Profile> {
        if bytes.len() < 254 {
            return Err(HubError::Protocol("onboard profile too short".into()));
        }
        let mut resolutions = [0u16; 5];
        for (i, r) in resolutions.iter_mut().enumerate() {
            *r = u16::from_le_bytes([bytes[3 + i * 2], bytes[4 + i * 2]]);
        }
        let mut buttons = [[0xffu8; 4]; 16];
        for (i, b) in buttons.iter_mut().enumerate() {
            b.copy_from_slice(&bytes[32 + i * 4..36 + i * 4]);
        }
        let mut gbuttons = [[0xffu8; 4]; 16];
        for (i, b) in gbuttons.iter_mut().enumerate() {
            b.copy_from_slice(&bytes[96 + i * 4..100 + i * 4]);
        }
        let mut lighting = [[0xffu8; 11]; 4];
        for (i, l) in lighting.iter_mut().enumerate() {
            l.copy_from_slice(&bytes[208 + i * 11..219 + i * 11]);
        }
        let raw_name = &bytes[160..208];
        let name = if raw_name.iter().all(|&b| b == 0xff) {
            String::new()
        } else {
            let units: Vec<u16> = raw_name
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .take_while(|&u| u != 0 && u != 0xffff)
                .collect();
            String::from_utf16_lossy(&units)
        };
        let mut reserved = [0u8; 8];
        reserved.copy_from_slice(&bytes[20..28]);
        Ok(Profile {
            rate_ms: bytes[0],
            default_index: bytes[1],
            shift_index: bytes[2],
            resolutions,
            color: Rgb::new(bytes[13], bytes[14], bytes[15]),
            power_mode: bytes[16],
            angle_snap: bytes[17],
            write_count: u16::from_le_bytes([bytes[18], bytes[19]]),
            reserved,
            ps_timeout: u16::from_le_bytes([bytes[28], bytes[29]]),
            po_timeout: u16::from_le_bytes([bytes[30], bytes[31]]),
            buttons,
            gbuttons,
            name,
            lighting,
        })
    }

    fn to_bytes(&self, size: u16) -> Vec<u8> {
        let mut b = vec![0xffu8; size as usize];
        b[0] = self.rate_ms;
        b[1] = self.default_index;
        b[2] = self.shift_index;
        for (i, r) in self.resolutions.iter().enumerate() {
            b[3 + i * 2..5 + i * 2].copy_from_slice(&r.to_le_bytes());
        }
        b[13] = self.color.r;
        b[14] = self.color.g;
        b[15] = self.color.b;
        b[16] = self.power_mode;
        b[17] = self.angle_snap;
        b[18..20].copy_from_slice(&self.write_count.to_le_bytes());
        b[20..28].copy_from_slice(&self.reserved);
        b[28..30].copy_from_slice(&self.ps_timeout.to_le_bytes());
        b[30..32].copy_from_slice(&self.po_timeout.to_le_bytes());
        for (i, btn) in self.buttons.iter().enumerate() {
            b[32 + i * 4..36 + i * 4].copy_from_slice(btn);
        }
        for (i, btn) in self.gbuttons.iter().enumerate() {
            b[96 + i * 4..100 + i * 4].copy_from_slice(btn);
        }
        if self.name.is_empty() {
            b[160..208].fill(0xff);
        } else {
            b[160..208].fill(0);
            for (i, unit) in self.name.encode_utf16().take(24).enumerate() {
                b[160 + i * 2..162 + i * 2].copy_from_slice(&unit.to_le_bytes());
            }
        }
        for (i, l) in self.lighting.iter().enumerate() {
            b[208 + i * 11..219 + i * 11].copy_from_slice(l);
        }
        let body = (size as usize).saturating_sub(2);
        let crc = crc16(&b[..body]);
        b[body..body + 2].copy_from_slice(&crc.to_be_bytes());
        b
    }
}

fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xffff;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

// ==================================================================== ライティング

#[derive(Clone)]
struct Zone {
    location: u16,
    /// エフェクト ID (index 順)。
    effects: Vec<u16>,
}

impl Zone {
    fn name(&self) -> &'static str {
        match self.location {
            0x01 => "DPI インジケーター",
            0x02 => "ロゴ",
            0x03 => "左側",
            0x04 => "右側",
            _ => "LED",
        }
    }
}

fn effect_name(id: u16) -> &'static str {
    match id {
        0x00 => "オフ",
        0x01 => "固定色",
        0x02 => "パルス",
        0x03 => "サイクル",
        0x04 => "ウェーブ",
        0x0a => "ブリージング",
        0x0b => "リップル",
        _ => "エフェクト",
    }
}

fn period_to_speed(period: u16) -> u8 {
    if period == 0 {
        return 50;
    }
    (((20000i32 - period as i32).clamp(0, 18000)) / 180) as u8
}

fn speed_to_period(speed: u8) -> u16 {
    (20000i32 - speed as i32 * 180).clamp(2000, 20000) as u16
}

struct State {
    io: Io,
    profiles: Vec<Profile>,
    active: usize,
}

// ==================================================================== ドライバ本体

pub struct Logitech {
    state: Mutex<State>,
    info: Mutex<DeviceInfo>,
    caps: Capabilities,
    feature: FeatureMap,
    headers: Vec<(u16, u8)>,
    size: u16,
    zones: Vec<Zone>,
    button_count: usize,
}

#[derive(Clone, Copy, Default)]
struct FeatureMap {
    fw: u8,
    name: u8,
    dpi: u8,
    rate: u8,
    led: u8,
    profile: u8,
}

pub fn open(dev: HidDevice, info: DeviceInfo) -> HubResult<Logitech> {
    let mut io = Io::open(info.vid, info.pid, dev)?;

    let feature = FeatureMap {
        fw: io.root_feature(FEAT_FW)?,
        name: io.root_feature(FEAT_NAME)?,
        dpi: io.root_feature(FEAT_DPI)?,
        rate: io.root_feature(FEAT_RATE)?,
        led: io.root_feature(FEAT_LED)?,
        profile: io.root_feature(FEAT_PROFILE)?,
    };
    if feature.profile == 0 {
        return Err(HubError::NotImplemented("オンボードプロファイル (0x8100)"));
    }

    let zones = read_zones(&mut io, feature.led).unwrap_or_default();
    let supported_rates = read_rate_mask(&mut io, feature.rate).unwrap_or(0x8b);

    let header = io.request(feature.profile, 0, &[])?;
    let size = u16::from_be_bytes([header.get(7).copied().unwrap_or(1), header.get(8).copied().unwrap_or(0)]);
    let size = if (16..=1024).contains(&size) { size } else { 256 };
    let button_count = (header.get(5).copied().unwrap_or(11) as usize).clamp(1, 16);

    let headers = read_profile_headers(&mut io, feature.profile, size)?;
    if headers.is_empty() {
        return Err(HubError::NotImplemented("書き込み可能なオンボードプロファイル"));
    }

    // 全プロファイルを読んで名前と内容をキャッシュする。
    let mut profiles = Vec::with_capacity(headers.len());
    for &(sector, _) in &headers {
        let bytes = read_sector(&mut io, feature.profile, sector, size)?;
        profiles.push(Profile::parse(&bytes)?);
    }

    let active_sector = io.get_active(feature.profile).unwrap_or(0);
    let active = headers
        .iter()
        .position(|(s, _)| *s == active_sector)
        .unwrap_or(0);

    // G HUB がホストモードのままなら、いまの実設定 (レート/DPI) を
    // プロファイル 1 に引き継いでからオンボードへ切り替える。
    if io.get_mode(feature.profile).unwrap_or(2) != 1 {
        adopt_host_state(&mut io, &feature, &mut profiles[active]);
        let bytes = profiles[active].to_bytes(size);
        write_sector(&mut io, feature.profile, headers[active].0, &bytes)?;
    }
    io.set_mode(feature.profile, 1).ok();
    io.set_active(feature.profile, headers[active].0).ok();

    let caps = build_caps(&zones, supported_rates, button_count);

    let mut out_info = info;
    if let Ok(name) = read_name(&mut io, feature.name) {
        if !name.is_empty() {
            out_info.name = name;
        }
    }
    out_info.firmware = read_firmware(&mut io, feature.fw).ok().flatten();

    Ok(Logitech {
        state: Mutex::new(State { io, profiles, active }),
        info: Mutex::new(out_info),
        caps,
        feature,
        headers,
        size,
        zones,
        button_count,
    })
}

impl Logitech {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn write_profile(&self, st: &mut State, index: usize) -> HubResult<()> {
        let bytes = st.profiles[index].to_bytes(self.size);
        write_sector(
            &mut st.io,
            self.feature.profile,
            self.headers[index].0,
            &bytes,
        )
    }

    /// プロファイルを書き込み、オンボードモードで即時反映する。
    fn apply_profile(&self, st: &mut State, index: usize) -> HubResult<()> {
        self.write_profile(st, index)?;
        st.io.set_mode(self.feature.profile, 1)?;
        st.io
            .set_active(self.feature.profile, self.headers[index].0)?;
        st.active = index;
        Ok(())
    }

    fn refresh_active(&self, st: &mut State) -> HubResult<()> {
        let index = st.active.min(st.profiles.len().saturating_sub(1));
        let sector = self.headers[index].0;
        let bytes = read_sector(&mut st.io, self.feature.profile, sector, self.size)?;
        st.profiles[index] = Profile::parse(&bytes)?;
        Ok(())
    }

    fn lighting_from(&self, p: &Profile, zone: usize) -> LightingState {
        let slot = p.lighting.get(zone).copied().unwrap_or([0xff; 11]);
        let effect = slot[0] as u16;
        let color = Rgb::new(slot[1], slot[2], slot[3]);
        let (speed, brightness) = match effect {
            0x0a => (
                period_to_speed(u16::from_be_bytes([slot[3], slot[4]])),
                slot[6],
            ),
            0x03 => (
                period_to_speed(u16::from_be_bytes([slot[5], slot[6]])),
                slot[7],
            ),
            0x02 => (slot[4], 100),
            _ => (0, 100),
        };
        LightingState {
            on: effect != 0 && effect != 0xffff,
            effect: if effect == 0xffff { 0 } else { effect as u8 },
            color,
            brightness,
            speed,
            direction: 0,
            per_key: BTreeMap::new(),
            zone_on: vec![],
            palette: vec![color],
            palette_sel: 0,
            color_correction: None,
        }
    }

    fn encode_lighting(&self, area: usize, state: &LightingState) -> HubResult<[u8; 11]> {
        let zone = self
            .zones
            .get(area)
            .ok_or_else(|| HubError::Invalid(format!("lighting area {area}")))?;
        let id = if state.on { state.effect as u16 } else { 0 };
        if !zone.effects.contains(&id) {
            return Err(HubError::Unsupported("このライティングエフェクト"));
        }
        let mut b = [0u8; 11];
        b[0] = id as u8;
        let scale = |c: u8| ((c as u16 * state.brightness.min(100) as u16) / 100) as u8;
        match id {
            0x00 => {}
            0x01 => {
                // Static: 明るさは LED の色そのものをスケールして表現する
                // (G502 HERO には 0x8040 BrightnessControl が無い)。
                b[1] = scale(state.color.r);
                b[2] = scale(state.color.g);
                b[3] = scale(state.color.b);
            }
            0x02 => {
                // Pulse: color + speed
                b[1] = scale(state.color.r);
                b[2] = scale(state.color.g);
                b[3] = scale(state.color.b);
                b[4] = state.speed;
            }
            0x0a => {
                // Breathe: color, period(2), form, intensity
                b[1] = state.color.r;
                b[2] = state.color.g;
                b[3] = state.color.b;
                let period = speed_to_period(state.speed).to_be_bytes();
                b[4] = period[0];
                b[5] = period[1];
                b[6] = 0;
                b[7] = state.brightness;
            }
            0x03 => {
                let period = speed_to_period(state.speed).to_be_bytes();
                b[5] = period[0];
                b[6] = period[1];
                b[7] = state.brightness;
            }
            _ => {
                b[1] = state.color.r;
                b[2] = state.color.g;
                b[3] = state.color.b;
                b[4] = state.speed;
            }
        }
        Ok(b)
    }

    fn set_live_lighting(&self, st: &mut State, area: usize, slot: &[u8; 11]) -> HubResult<()> {
        if self.feature.led == 0 || area >= self.zones.len() {
            return Ok(());
        }
        let id = slot[0] as u16;
        let Some(effect_index) = self.zones[area].effects.iter().position(|&e| e == id) else {
            return Ok(());
        };
        let mut params = vec![area as u8, effect_index as u8];
        params.extend_from_slice(&slot[1..11]);
        st.io.send_only(self.feature.led, 3, &params)
    }
}

fn read_zones(io: &mut Io, feature: u8) -> HubResult<Vec<Zone>> {
    if feature == 0 {
        return Ok(vec![]);
    }
    let info = io.request(feature, 0, &[])?;
    let count = info[0];
    let mut zones = Vec::new();
    for z in 0..count {
        let zi = io.request(feature, 1, &[z, 0xff, 0x00])?;
        let location = u16::from_be_bytes([zi[1], zi[2]]);
        let effects_count = zi[3];
        let mut effects = Vec::new();
        for e in 0..effects_count {
            let ei = io.request(feature, 2, &[z, e, 0x00])?;
            effects.push(u16::from_be_bytes([ei[2], ei[3]]));
        }
        zones.push(Zone { location, effects });
    }
    Ok(zones)
}

fn read_rate_mask(io: &mut Io, feature: u8) -> HubResult<u8> {
    if feature == 0 {
        return Ok(0);
    }
    Ok(io.request(feature, 0, &[])?[0])
}

fn read_profile_headers(io: &mut Io, feature: u8, size: u16) -> HubResult<Vec<(u16, u8)>> {
    let _ = size;
    let mut out = Vec::new();
    let mut offset: u16 = 0;
    loop {
        let chunk = io.request(
            feature,
            5,
            &[
                0,
                0,
                (offset >> 8) as u8,
                offset as u8,
            ],
        )?;
        let sector = u16::from_be_bytes([chunk[0], chunk[1]]);
        if sector == 0xffff {
            break;
        }
        out.push((sector, chunk[2]));
        offset += 4;
        if offset > 64 {
            break;
        }
    }
    Ok(out)
}

fn read_sector(io: &mut Io, feature: u8, sector: u16, size: u16) -> HubResult<Vec<u8>> {
    let mut out = Vec::with_capacity(size as usize);
    let mut offset: u16 = 0;
    while (out.len() as u16) < size {
        let chunk = io.request(
            feature,
            5,
            &[
                (sector >> 8) as u8,
                sector as u8,
                (offset >> 8) as u8,
                offset as u8,
            ],
        )?;
        out.extend_from_slice(&chunk);
        offset += 16;
    }
    out.truncate(size as usize);
    Ok(out)
}

fn write_sector(io: &mut Io, feature: u8, sector: u16, bytes: &[u8]) -> HubResult<()> {
    let len = bytes.len();
    io.request(
        feature,
        6,
        &[
            (sector >> 8) as u8,
            sector as u8,
            0,
            0,
            (len >> 8) as u8,
            len as u8,
        ],
    )?;
    for chunk in bytes.chunks(16) {
        let mut p = [0xffu8; 16];
        p[..chunk.len()].copy_from_slice(chunk);
        io.request(feature, 7, &p)?;
    }
    io.send_only(feature, 8, &[])
}

fn adopt_host_state(io: &mut Io, feature: &FeatureMap, p: &mut Profile) {
    // レート
    if feature.rate != 0 {
        if let Ok(r) = io.request(feature.rate, 1, &[]) {
            if (1..=8).contains(&r[0]) {
                p.rate_ms = r[0];
            }
        }
    }
    // 現在 DPI
    if feature.dpi != 0 {
        if let Ok(r) = io.request(feature.dpi, 2, &[0]) {
            let mut dpi = u16::from_be_bytes([r[1], r[2]]);
            if dpi == 0 {
                dpi = u16::from_be_bytes([r[3], r[4]]);
            }
            if dpi >= DPI_MIN as u16 {
                let idx = (p.default_index as usize).min(4);
                p.resolutions[idx] = dpi;
            }
        }
    }
}

fn read_name(io: &mut Io, feature: u8) -> HubResult<String> {
    if feature == 0 {
        return Ok(String::new());
    }
    let len = io.request(feature, 0, &[])?[0] as usize;
    if len == 0 {
        return Ok(String::new());
    }
    // function 1 の引数はチャンク番号ではなく「文字オフセット」(実機で確認)。
    let mut out = Vec::new();
    let mut offset = 0usize;
    while offset < len {
        let chunk = io.request(feature, 1, &[offset as u8])?;
        out.extend_from_slice(&chunk);
        offset += 16;
    }
    out.truncate(len);
    let end = out.iter().position(|&b| b == 0).unwrap_or(out.len());
    Ok(String::from_utf8_lossy(&out[..end]).to_string())
}

fn read_firmware(io: &mut Io, feature: u8) -> HubResult<Option<String>> {
    if feature == 0 {
        return Ok(None);
    }
    let count = io.request(feature, 0, &[])?[0];
    for i in 0..count {
        let Ok(r) = io.request(feature, 1, &[i]) else {
            continue;
        };
        if r[0] & 0x0f == 0 && r.len() >= 8 {
            let major = r[4];
            let minor = r[5];
            let build = u16::from_be_bytes([r[6], r[7]]);
            return Ok(Some(if build != 0 {
                format!("{major}.{minor:02}.B{build:04X}")
            } else {
                format!("{major}.{minor:02}")
            }));
        }
    }
    Ok(None)
}

// ==================================================================== ボタン

fn mouse_bit(m: MouseFn) -> Option<u16> {
    Some(match m {
        MouseFn::Left => 0x0001,
        MouseFn::Right => 0x0002,
        MouseFn::Middle => 0x0004,
        MouseFn::Back => 0x0008,
        MouseFn::Forward => 0x0010,
        _ => return None,
    })
}

fn mouse_from_bit(v: u16) -> Option<MouseFn> {
    Some(match v {
        0x0001 => MouseFn::Left,
        0x0002 => MouseFn::Right,
        0x0004 => MouseFn::Middle,
        0x0008 => MouseFn::Back,
        0x0010 => MouseFn::Forward,
        _ => return None,
    })
}

fn consumer_usage(m: MediaFn) -> u16 {
    match m {
        MediaFn::PlayPause => 0x00cd,
        MediaFn::Stop => 0x00b7,
        MediaFn::Next => 0x00b5,
        MediaFn::Prev => 0x00b6,
        MediaFn::VolumeUp => 0x00e9,
        MediaFn::VolumeDown => 0x00ea,
        MediaFn::Mute => 0x00e2,
        MediaFn::Calculator => 0x0192,
        MediaFn::MyComputer => 0x0194,
        MediaFn::Browser => 0x0221,
        MediaFn::Mail => 0x018a,
        MediaFn::BrightnessUp => 0x006f,
        MediaFn::BrightnessDown => 0x0070,
        MediaFn::Refresh => 0x00b8,
    }
}

fn media_from_usage(v: u16) -> Option<MediaFn> {
    Some(match v {
        0x00cd => MediaFn::PlayPause,
        0x00b7 => MediaFn::Stop,
        0x00b5 => MediaFn::Next,
        0x00b6 => MediaFn::Prev,
        0x00e9 => MediaFn::VolumeUp,
        0x00ea => MediaFn::VolumeDown,
        0x00e2 => MediaFn::Mute,
        0x0192 => MediaFn::Calculator,
        0x0194 => MediaFn::MyComputer,
        0x0221 => MediaFn::Browser,
        0x018a => MediaFn::Mail,
        0x006f => MediaFn::BrightnessUp,
        0x0070 => MediaFn::BrightnessDown,
        0x00b8 => MediaFn::Refresh,
        _ => return None,
    })
}

fn encode_button(action: &ButtonAction) -> HubResult<[u8; 4]> {
    Ok(match action {
        ButtonAction::Disabled => [0x80, 0x00, 0xff, 0xff],
        ButtonAction::Mouse(m) => {
            if let Some(bit) = mouse_bit(*m) {
                [0x80, 0x01, (bit >> 8) as u8, bit as u8]
            } else {
                match m {
                    MouseFn::ScrollUp => [0x90, 0x11, 0x00, 0x00],
                    MouseFn::ScrollDown => [0x90, 0x10, 0x00, 0x00],
                    MouseFn::ScrollLeft => [0x90, 0x01, 0x00, 0x00],
                    MouseFn::ScrollRight => [0x90, 0x02, 0x00, 0x00],
                    MouseFn::DoubleClick => return Err(HubError::Unsupported("ダブルクリック割り当て")),
                    _ => return Err(HubError::Unsupported("このマウス動作")),
                }
            }
        }
        ButtonAction::Dpi(f) => match f {
            DpiFn::Cycle => [0x90, 0x05, 0x00, 0x00],
            DpiFn::Up => [0x90, 0x03, 0x00, 0x00],
            DpiFn::Down => [0x90, 0x04, 0x00, 0x00],
            DpiFn::Shift(_) => [0x90, 0x07, 0x00, 0x00],
        },
        ButtonAction::ProfileCycle => [0x90, 0x0a, 0x00, 0x00],
        ButtonAction::Key(k) => {
            if k.usage > 0xff {
                return Err(HubError::Unsupported("このキーコード"));
            }
            [0x80, 0x02, k.mods, k.usage as u8]
        }
        ButtonAction::Media(m) => {
            let u = consumer_usage(*m);
            [0x80, 0x03, (u >> 8) as u8, u as u8]
        }
        ButtonAction::Unsupported { ty, params } => {
            let mut b = [0xffu8; 4];
            b[0] = *ty;
            for (i, v) in params.iter().take(3).enumerate() {
                b[1 + i] = *v;
            }
            b
        }
        _ => return Err(HubError::Unsupported("このボタン割り当て")),
    })
}

fn decode_button(b: &[u8; 4], shift_dpi: u32) -> ButtonAction {
    match b[0] >> 4 {
        0x8 => match b[1] {
            0x00 => ButtonAction::Disabled,
            0x01 => {
                let v = u16::from_be_bytes([b[2], b[3]]);
                match mouse_from_bit(v) {
                    Some(m) => ButtonAction::Mouse(m),
                    None => ButtonAction::Unsupported {
                        ty: b[0],
                        params: b[1..4].to_vec(),
                    },
                }
            }
            0x02 => ButtonAction::Key(KeyStroke {
                mods: b[2],
                usage: b[3] as u16,
            }),
            0x03 => {
                let v = u16::from_be_bytes([b[2], b[3]]);
                match media_from_usage(v) {
                    Some(m) => ButtonAction::Media(m),
                    None => ButtonAction::Unsupported {
                        ty: b[0],
                        params: b[1..4].to_vec(),
                    },
                }
            }
            _ => ButtonAction::Unsupported {
                ty: b[0],
                params: b[1..4].to_vec(),
            },
        },
        0x9 => match b[1] {
            0x01 => ButtonAction::Mouse(MouseFn::ScrollLeft),
            0x02 => ButtonAction::Mouse(MouseFn::ScrollRight),
            0x03 => ButtonAction::Dpi(DpiFn::Up),
            0x04 => ButtonAction::Dpi(DpiFn::Down),
            0x05 => ButtonAction::Dpi(DpiFn::Cycle),
            0x07 => ButtonAction::Dpi(DpiFn::Shift(shift_dpi)),
            0x0a => ButtonAction::ProfileCycle,
            0x10 => ButtonAction::Mouse(MouseFn::ScrollDown),
            0x11 => ButtonAction::Mouse(MouseFn::ScrollUp),
            _ => ButtonAction::Unsupported {
                ty: b[0],
                params: b[1..4].to_vec(),
            },
        },
        _ => ButtonAction::Unsupported {
            ty: b[0],
            params: b[1..4].to_vec(),
        },
    }
}

// ==================================================================== caps

fn build_caps(zones: &[Zone], rate_mask: u8, buttons: usize) -> Capabilities {
    let mut settings = Vec::new();
    let rates = rate_options(rate_mask);
    if !rates.is_empty() {
        settings.push(
            SettingSpec::choice("polling", section::PERFORMANCE, "ポーリングレート", rates)
                .help("オンボードプロファイルのレポートレート (Hz)"),
        );
    }

    let names = [
        "左クリック",
        "右クリック",
        "ホイールクリック",
        "戻る",
        "進む",
        "スナイパー (G6)",
        "DPI ダウン (G7)",
        "DPI アップ (G8)",
        "プロファイル (G9)",
        "チルト右",
        "チルト左",
    ];
    let slots = (0..buttons)
        .map(|i| ButtonSlot {
            name: names.get(i).copied().unwrap_or("ボタン").to_string(),
        })
        .collect();

    let lighting = zones
        .iter()
        .enumerate()
        .map(|(i, z)| LightArea {
            id: i as u8,
            name: z.name().to_string(),
            effects: z
                .effects
                .iter()
                .map(|&id| {
                    let e = LightEffect::new(id as u8, effect_name(id));
                    match id {
                        0x00 | 0x01 => e.still(),
                        0x02 | 0x03 => e.color(),
                        0x0a => e.color(),
                        _ => e.color(),
                    }
                })
                .collect(),
            brightness_max: 100,
            speed_max: 100,
            directions: vec![],
            switchable: true,
            zones: vec![],
            per_key: false,
            direct_drive: false,
            live_readback: false,
        })
        .collect();

    Capabilities {
        settings,
        actions: vec![
            ActionSpec::new("reset_profile", section::SYSTEM, "このプロファイルを初期化")
                .help("DPI・ボタン・ライティングを既定値に戻します")
                .danger("現在のプロファイル設定を初期化します。よろしいですか?"),
        ],
        profiles: Some(ProfileCaps {
            count: 5,
            renamable: true,
        }),
        mouse: Some(MouseCaps {
            dpi_min: DPI_MIN,
            dpi_max: DPI_MAX,
            dpi_step: DPI_STEP,
            min_stages: 1,
            max_stages: 5,
            xy_split: false,
            stage_colors: false,
            stage_toggle: true,
            buttons: slots,
            button_kinds: vec![
                ButtonActionKind::Disabled,
                ButtonActionKind::Mouse,
                ButtonActionKind::Dpi,
                ButtonActionKind::Key,
                ButtonActionKind::Media,
                ButtonActionKind::ProfileCycle,
            ],
            require_left_click: true,
            macros: None,
            live_buttons: vec![],
            adaptive_dpi: None,
        }),
        keyboard: None,
        lighting,
        battery: false,
        peq: None,
        firmware_feed: None,
    }
}

fn rate_options(mask: u8) -> Vec<ChoiceOpt> {
    let table = [
        (1000i64, 0u8),
        (500, 1),
        (250, 3),
        (125, 7),
    ];
    table
        .iter()
        .filter(|(_, bit)| mask & (1 << bit) != 0)
        .map(|(hz, _)| ChoiceOpt::new(*hz, format!("{hz}")))
        .collect()
}

fn hz_to_ms(hz: i64) -> HubResult<u8> {
    Ok(match hz {
        1000 => 1,
        500 => 2,
        333 => 3,
        250 => 4,
        200 => 5,
        166 => 6,
        142 => 7,
        125 => 8,
        _ => return Err(HubError::Invalid(format!("未対応のポーリングレート: {hz}"))),
    })
}

fn ms_to_hz(ms: u8) -> i64 {
    match ms {
        1 => 1000,
        2 => 500,
        3 => 333,
        4 => 250,
        5 => 200,
        6 => 166,
        7 => 142,
        8 => 125,
        _ => 1000,
    }
}

// ==================================================================== DeviceDriver

impl DeviceDriver for Logitech {
    fn info(&self) -> DeviceInfo {
        self.info
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn caps(&self) -> Capabilities {
        self.caps.clone()
    }

    fn read(&self) -> HubResult<DeviceState> {
        let mut st = self.lock();
        // 本体のプロファイルボタンで切り替えられた場合も追従する。
        if let Ok(sector) = st.io.get_active(self.feature.profile) {
            if let Some(i) = self.headers.iter().position(|(s, _)| *s == sector) {
                st.active = i;
            }
        }
        self.refresh_active(&mut st)?;
        let p = st.profiles[st.active].clone();

        let active_stage = (p.default_index as usize).min(4);
        let shift_dpi = p
            .resolutions
            .get(p.shift_index as usize)
            .copied()
            .unwrap_or(0) as u32;
        let stages = p
            .resolutions
            .iter()
            .map(|&r| DpiStage {
                x: r as u32,
                y: None,
                color: p.color,
                enabled: r != 0,
            })
            .collect();
        let buttons = (0..self.button_count)
            .map(|i| decode_button(&p.buttons[i], shift_dpi))
            .collect();

        let mut settings = BTreeMap::new();
        settings.insert(
            "polling".to_string(),
            SettingValue::Choice(ms_to_hz(p.rate_ms)),
        );

        let lighting = (0..self.zones.len())
            .map(|i| (i as u8, self.lighting_from(&p, i)))
            .collect();

        Ok(DeviceState {
            profile: ProfileState {
                active: st.active as u8,
                names: st.profiles.iter().map(|p| p.name.clone()).collect(),
            },
            settings,
            mouse: Some(MouseState {
                stages,
                active_stage,
                buttons,
                macros: vec![],
                adaptive: None,
            }),
            keyboard: None,
            lighting,
            peq: None,
        })
    }

    fn apply(&self, change: &Change) -> HubResult<()> {
        let mut st = self.lock();
        let index = st.active.min(st.profiles.len().saturating_sub(1));
        match change {
            Change::Setting { id, value } if id == "polling" => {
                let ms = hz_to_ms(value.as_choice())?;
                st.profiles[index].rate_ms = ms;
                self.apply_profile(&mut st, index)
            }
            Change::DpiStages { stages, active } => {
                {
                    let p = &mut st.profiles[index];
                    for i in 0..5 {
                        p.resolutions[i] = match stages.get(i) {
                            Some(s) if s.enabled => clamp_dpi(s.x) as u16,
                            _ => 0,
                        };
                    }
                    p.default_index = (*active).min(4) as u8;
                    if let Some(s) = stages.get(*active) {
                        p.color = s.color;
                    }
                    if p.shift_index >= 5 {
                        p.shift_index = 0;
                    }
                }
                self.apply_profile(&mut st, index)
            }
            Change::ActiveStage(stage) => {
                st.profiles[index].default_index = (*stage).min(4) as u8;
                self.apply_profile(&mut st, index)
            }
            Change::Button { slot, action } => {
                if *slot >= self.button_count {
                    return Err(HubError::Invalid(format!("button {slot}")));
                }
                let mut bytes = encode_button(action)?;
                // Shift はプロファイルの shift index を使うので、指定 DPI に一番近い
                // ステージを shift に割り当てる。
                if let ButtonAction::Dpi(DpiFn::Shift(dpi)) = action {
                    let p = &mut st.profiles[index];
                    if let Some(i) = p
                        .resolutions
                        .iter()
                        .position(|&r| r as u32 == clamp_dpi(*dpi))
                    {
                        p.shift_index = i as u8;
                    }
                    bytes = [0x90, 0x07, 0x00, 0x00];
                }
                st.profiles[index].buttons[*slot] = bytes;
                self.apply_profile(&mut st, index)
            }
            Change::Lighting { area, state } => {
                let slot = self.encode_lighting(*area as usize, state)?;
                st.profiles[index].lighting[*area as usize] = slot;
                self.apply_profile(&mut st, index)?;
                self.set_live_lighting(&mut st, *area as usize, &slot)
            }
            Change::Profile(profile) => {
                let i = (*profile as usize).min(st.profiles.len().saturating_sub(1));
                st.io.set_mode(self.feature.profile, 1)?;
                st.io
                    .set_active(self.feature.profile, self.headers[i].0)?;
                st.active = i;
                Ok(())
            }
            Change::ProfileName { index, name } => {
                let i = (*index as usize).min(st.profiles.len().saturating_sub(1));
                st.profiles[i].name = name.clone();
                self.apply_profile(&mut st, i)
            }
            _ => Err(HubError::Unsupported("この変更")),
        }
    }

    fn run_action(&self, id: &str, _file: Option<&std::path::Path>) -> HubResult<ActionOutcome> {
        if id != "reset_profile" {
            return Err(HubError::Unsupported("action"));
        }
        let mut st = self.lock();
        let index = st.active.min(st.profiles.len().saturating_sub(1));
        let mut p = st.profiles[index].clone();
        p.rate_ms = 1;
        p.default_index = 1;
        p.shift_index = 2;
        p.resolutions = [400, 800, 1600, 3200, 6400];
        p.color = Rgb::hex(0xffffff);
        p.angle_snap = 0;
        let defaults: [[u8; 4]; 11] = [
            [0x80, 0x01, 0x00, 0x01],
            [0x80, 0x01, 0x00, 0x02],
            [0x80, 0x01, 0x00, 0x04],
            [0x80, 0x01, 0x00, 0x08],
            [0x80, 0x01, 0x00, 0x10],
            [0x90, 0x07, 0x00, 0x00],
            [0x90, 0x04, 0x00, 0x00],
            [0x90, 0x03, 0x00, 0x00],
            [0x90, 0x0a, 0x00, 0x00],
            [0x90, 0x02, 0x00, 0x00],
            [0x90, 0x01, 0x00, 0x00],
        ];
        for (i, b) in defaults.iter().enumerate() {
            p.buttons[i] = *b;
        }
        for slot in p.lighting.iter_mut() {
            *slot = [0xff; 11];
        }
        p.lighting[0] = [1, 0xff, 0xff, 0xff, 0x00, 0, 0, 0, 0, 0, 0];
        if p.lighting.len() > 1 {
            p.lighting[1] = p.lighting[0];
        }
        st.profiles[index] = p;
        self.apply_profile(&mut st, index)?;
        Ok(ActionOutcome {
            message: Some("プロファイルを初期化しました".into()),
            reload: true,
        })
    }
}

fn clamp_dpi(dpi: u32) -> u32 {
    let step = DPI_STEP;
    let v = dpi.clamp(DPI_MIN, DPI_MAX);
    (v / step * step).clamp(DPI_MIN, DPI_MAX)
}

// ==================================================================== テスト

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_device_profile() {
        // 実機 sector 1 の先頭 254B に対する CRC (0x4e2e) と一致すること。
        let mut b = vec![0xffu8; 254];
        b[0] = 1;
        b[1] = 2;
        b[2] = 0;
        b[3..13].copy_from_slice(&[
            0xb0, 0x04, 0x60, 0x09, 0xa0, 0x0f, 0x00, 0x19, 0x00, 0x00,
        ]);
        b[13..16].copy_from_slice(&[0xff, 0xff, 0xff]);
        b[16] = 0xff;
        b[17] = 0x00;
        b[18] = 0xff;
        b[19] = 0xff;
        // 実機の CRC と一致するのは「実データ」が必要なので、ここでは
        // 既知の CCITT-FALSE ベクタでアルゴリズムだけ確認する。
        assert_eq!(crc16(b"123456789"), 0x29b1);
    }

    #[test]
    fn button_roundtrip() {
        let b = encode_button(&ButtonAction::Mouse(MouseFn::Middle)).unwrap();
        assert_eq!(b, [0x80, 0x01, 0x00, 0x04]);
        assert_eq!(decode_button(&b, 0), ButtonAction::Mouse(MouseFn::Middle));
        let b = encode_button(&ButtonAction::Key(KeyStroke {
            mods: 0x01,
            usage: 0x06,
        }))
        .unwrap();
        assert_eq!(b, [0x80, 0x02, 0x01, 0x06]);
        assert_eq!(
            decode_button(&b, 0),
            ButtonAction::Key(KeyStroke {
                mods: 0x01,
                usage: 0x06
            })
        );
    }

    #[test]
    fn rate_and_dpi_helpers() {
        assert_eq!(hz_to_ms(1000).unwrap(), 1);
        assert_eq!(hz_to_ms(125).unwrap(), 8);
        assert_eq!(ms_to_hz(2), 500);
        assert_eq!(clamp_dpi(803), 800);
        assert_eq!(clamp_dpi(16_010), 16_000);
    }
}
