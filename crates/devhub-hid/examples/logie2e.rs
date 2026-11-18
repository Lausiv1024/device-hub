//! G502 HERO 実機での HID++ ドライバ往復テスト。
//!   cargo run -p devhub-hid --example logie2e

use devhub_core::backend::*;
use devhub_core::model::*;
use hidapi::HidApi;

fn main() {
    let api = HidApi::new().unwrap();
    let mut path = None;
    for d in api.device_list() {
        if d.vendor_id() == 0x046d
            && d.product_id() == 0xc08b
            && d.usage_page() == 0xff00
            && d.usage() == 0x01
        {
            path = Some(d.path().to_owned());
            break;
        }
    }
    let Some(path) = path else {
        println!("G502 HERO が見つかりません");
        return;
    };
    let dev = api.open_path(&path).unwrap();
    let info = DeviceInfo {
        id: DeviceId("hid:046d:c08b:test".into()),
        kind: DeviceKind::Mouse,
        name: "G502 HERO".into(),
        vendor: "Logitech".into(),
        model: "g502-hero".into(),
        vid: 0x046d,
        pid: 0xc08b,
        transport: Transport::Wired,
        firmware: None,
        dongle_firmware: None,
        serial: None,
        driver: DriverKind::Logitech,
        battery: None,
        supported: true,
        asleep: false,
        board_id: None,
    };
    let drv = match devhub_hid::logitech::open(dev, info) {
        Ok(d) => d,
        Err(e) => {
            println!("open failed: {e}");
            return;
        }
    };
    let info = drv.info();
    println!("info: {} / fw={:?}", info.name, info.firmware);
    let caps = drv.caps();
    println!(
        "caps: mouse={} profiles={:?} lighting={} settings={}",
        caps.mouse.is_some(),
        caps.profiles,
        caps.lighting.len(),
        caps.settings.len()
    );
    for a in &caps.lighting {
        let ids: Vec<(u8, &str)> = a.effects.iter().map(|e| (e.id, e.name.as_str())).collect();
        println!("  light {} {}: {:?}", a.id, a.name, ids);
    }

    let state = match drv.read() {
        Ok(s) => s,
        Err(e) => {
            println!("read failed: {e}");
            return;
        }
    };
    if let Some(m) = &state.mouse {
        println!(
            "dpi: {:?} active={}",
            m.stages.iter().map(|s| (s.x, s.enabled)).collect::<Vec<_>>(),
            m.active_stage
        );
        for (i, b) in m.buttons.iter().enumerate() {
            println!("  button {i}: {b:?}");
        }
    }
    println!("settings: {:?}", state.settings);
    println!("profile: {:?}", state.profile);
    for (id, l) in &state.lighting {
        println!("  light {id}: effect={} on={} color={:?} speed={} bright={}", l.effect, l.on, l.color, l.speed, l.brightness);
    }

    // --- 書き込み往復: stage 1 を 800 DPI / 125Hz にして読み戻す
    let mut stages = state.mouse.clone().unwrap().stages;
    stages[0].x = 800;
    stages[0].enabled = true;
    if let Err(e) = drv.apply(&Change::DpiStages { stages, active: 0 }) {
        println!("apply dpi failed: {e}");
    }
    if let Err(e) = drv.apply(&Change::Setting {
        id: "polling".into(),
        value: SettingValue::Choice(1000),
    }) {
        println!("apply polling failed: {e}");
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    match drv.read() {
        Ok(s) => {
            if let Some(m) = &s.mouse {
                println!(
                    "readback dpi: {:?} active={}",
                    m.stages.iter().map(|s| (s.x, s.enabled)).collect::<Vec<_>>(),
                    m.active_stage
                );
            }
            println!("readback settings: {:?}", s.settings);
            for (id, l) in &s.lighting {
                println!("  readback light {id}: effect={} on={}", l.effect, l.on);
            }
        }
        Err(e) => println!("readback failed: {e}"),
    }

    // --- ライティング往復: 両ゾーンを赤の固定色に
    for area in [0u8, 1u8] {
        let mut l = drv.read().ok().and_then(|s| s.lighting.get(&area).cloned()).unwrap();
        l.on = true;
        l.effect = 1;
        l.color = Rgb::hex(0xff0000);
        match drv.apply(&Change::Lighting { area, state: l }) {
            Ok(()) => println!("applied light {area}"),
            Err(e) => println!("apply light {area} failed: {e}"),
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    if let Ok(s) = drv.read() {
        for (id, l) in &s.lighting {
            println!("  light {id}: effect={} on={} color={:?}", l.effect, l.on, l.color);
        }
    }

    // --- ボタン往復: スナイパー (slot 5) を Ctrl+C にして戻す
    let _ = drv.apply(&Change::Button {
        slot: 5,
        action: ButtonAction::Key(KeyStroke {
            mods: 0x01,
            usage: 0x06,
        }),
    });
    if let Ok(s) = drv.read() {
        if let Some(m) = &s.mouse {
            println!("  button 5 (Ctrl+C 後): {:?}", m.buttons[5]);
        }
    }
    let _ = drv.apply(&Change::Button {
        slot: 5,
        action: ButtonAction::Dpi(DpiFn::Shift(800)),
    });
    if let Ok(s) = drv.read() {
        if let Some(m) = &s.mouse {
            println!("  button 5 (戻し): {:?}", m.buttons[5]);
        }
    }
}
