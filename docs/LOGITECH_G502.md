# Logitech HID++ 2.0 — G502 HERO (046d:c08b) 実機解析

G HUB を置き換えるために、G HUB を止めた状態で実機を直接叩いて確認した。
実装は `crates/devhub-hid/src/logitech.rs`。設定は全てオンボードプロファイル
(0x8100) に書くので、アプリが落ちていてもマウス側に残る。

## 1. トランスポート (Windows)

同じ MI_01 インターフェースが HID++ を 2 つのコレクションに分けて公開する。

| HID コレクション | usage | フレーム | 用途 |
|---|---|---|---|
| interface 1 Col04 | FF00:01 | report ID `0x10` / 7B | short (params <= 3B) |
| interface 1 Col05 | FF00:02 | report ID `0x11` / 20B | long (params <= 16B) |

- 送信: `[reportID, devIndex=0xFF, featureIndex, function<<4|swId, params...]`
- short で送っても応答は long (20B) で返ることがある → 両方のハンドルを読む。
- `hidapi` の `write` は先頭の report ID を含めたバイト列を渡す。
- 応答の `swId` が一致するまで読み飛ばす。HID++ エラーは
  `featureIndex=0xFF` で返り、`params[1]` がエラーコード。
- 同じ VID/PID でも Col04 と Col05 は別 path。両方開くこと。
- **Col04 に 20B を書くと `ERROR_INVALID_FUNCTION`**、Col05 に 7B を
  送ると同じエラーになる。用途ごとにハンドルを分ける必要がある。

実機のフィーチャ表 (IFeatureSet 0x0001 = index 01):

| ID | index | 名前 | 備考 |
|---|---|---|---|
| 0x0000 | 00 | IRoot | getFeature |
| 0x0001 | 01 | IFeatureSet | 全 18 個を列挙 |
| 0x8070 | 02 | ColorLedEffects V1 | **V2 (0x8071) ではない** |
| 0x0003 | 03 | DeviceFwVersion | 39.03.B0010 |
| 0x0005 | 04 | DeviceName | 22B。fn1 の引数はチャンク番号ではなく文字オフセット |
| 0x1300 | 05 | LED Control | 未使用 |
| 0x2201 | 0a | AdjustableDpi | 1 sensor / 100–16000 / 50 刻み |
| 0x8060 | 0b | ReportRate | bitmask `0x8b` = 1/2/4/8ms |
| 0x8100 | 0c | OnboardProfiles | 5 profile / 11 button / 256B |
| 0x8110 | 0d | MouseButtonSpy | 未使用 |
| 0x00c2 | 0e | DFU Control V3 | 未使用 |
| 0x1802 | 10 | DeviceReset | 未使用 |
| 0x1806 | 11 | ConfigurableDeviceProperties | 未使用 |

**0x1B04 (ReprogControls) は無い。** ボタン割り当ては 0x8100 の
プロファイルにしか存在しない (G HUB も同じ経路)。

## 2. 0x8100 (OnboardProfiles) — 読み書き

| fn | 名前 | params | 応答 |
|---|---|---|---|
| 0 | getInfo | — | `[memory=1, current, macro, count=5, oob, buttons=11, sectors=16, size(2B BE)=256, shift]` |
| 1 | setMode | `[1]`=オンボード / `[2]`=ホスト | ACK |
| 2 | getMode | — | `[1]` / `[2]` |
| 3 | setActiveProfile | 2B (セクタ番号。実機は profile1=`00 01`) | ACK |
| 4 | getActiveProfile | — | 2B |
| 5 | readSector | `[sectorHi, sectorLo, offsetHi, offsetLo]` | 16B |
| 6 | writeStart | `[sectorHi, sectorLo, 0, 0, lenHi, lenLo]` | ACK |
| 7 | writeChunk | 16B データ | ACK |
| 8 | writeEnd | — | ACK (確定) |

fn 5 でセクタ 0 の先頭 64B を読むとプロファイル目次:
`[sector(2B BE), enabled, 0]` × 5、終端 `ff ff 00 00`。
実機は sector 1..5 が enabled=1。セクタ 0 が目次 (CRC あり)。

### プロファイル 256B レイアウト (実機ダンプ)

```text
  0   report rate (ms: 1=1000, 2=500, 4=250, 8=125)
  1   default resolution index (0-4)
  2   shift resolution index (スナイパー)
  3   5 × u16 LE resolution (0 = 無効ステージ)
  13  R,G,B (DPI インジケータの色)
  16  power mode / 17 angle snap / 18 write count (LE)
  20  reserved 8B / 28 ps timeout / 30 po timeout
  32  16 × 4B button map (先頭 11 個が実ボタン)
  96  16 × 4B G-button map
  160 name UTF-16LE 48B (空 = ff × 48)
  208 4 × 11B lighting (zone ごと)
  254 CRC16-CCITT (poly 0x1021, init 0xFFFF, 対象 0..254, BE)
```

CRC は `crc16(data[0..254])` を big-endian で 254..256 に置く。
5 プロファイル全てで一致を確認済み (`crc16(b"123456789") == 0x29b1`)。

### ボタン 1 個 = 4B

| bytes | 意味 |
|---|---|
| `80 00 ff ff` | なし |
| `80 01 hi lo` | マウスボタン bitmask (1=左, 2=右, 4=中, 8=戻る, 0x10=進む) |
| `80 02 mods key` | 修飾 + HID キーコード |
| `80 03 hi lo` | コンシューマ usage (16bit BE) |
| `90 03 00 00` | DPI アップ |
| `90 04 00 00` | DPI ダウン |
| `90 05 00 00` | DPI サイクル |
| `90 07 00 00` | スナイパー (プロファイルの shift index を使用) |
| `90 0a 00 00` | プロファイルサイクル |
| `90 10/11 00 00` | スクロール下 / 上 |
| `90 01/02 00 00` | チルト左 / 右 |

### 物理ボタンの並び (プロファイルのスロット順)

| # | ボタン | 既定 |
|---|---|---|
| 1 | 左クリック | Mouse Left |
| 2 | 右クリック | Mouse Right |
| 3 | ホイールクリック | Mouse Middle |
| 4 | 戻る | Mouse Back |
| 5 | 進む | Mouse Forward |
| 6 | スナイパー (G6) | DPI シフト (shift index の DPI) |
| 7 | DPI ダウン (G7) | DPI ダウン |
| 8 | DPI アップ (G8) | DPI アップ |
| 9 | プロファイル (G9) | プロファイルサイクル |
| 10 | チルト右 | スクロール右 |
| 11 | チルト左 | スクロール左 |

G7/G8 は「DPI アップ/ダウン」固定ではなく、UI のボタン画面から
なし / マウスボタン / DPI (アップ・ダウン・サイクル・シフト) /
キー・ショートカット / メディア / プロファイル切替 に変更できる
(実機で G7→Ctrl+C、G8→プロファイル切替 の書き込み・読み戻しを確認)。
G6 は既定で「DPI シフト」— プロファイルの shift index の DPI になる。

### ライティング (0x8070 V1)

| fn | 名前 | params | 応答 |
|---|---|---|---|
| 0 | getInfo | — | `[zoneCount=2, 0, caps(2B)]` |
| 1 | getZoneInfo | `[zone, 0xFF, 0]` | `[zone, location(2B BE), effectCount]` |
| 2 | getEffectInfo | `[zone, effect, 0]` | `[zone, index, id(2B BE), caps(2B), period(2B)]` |
| 3 | setEffect | `[zone, effectIndex, R,G,B, params 7B]` | ACK |
| 14 | getEffect | `[zone]` | `[zone, effectIndex, R,G,B, ...]` |

- location `0x01` = Primary (DPI インジケータ)、`0x02` = Logo。
- 実機の ID: `0x00` オフ / `0x01` 固定 / `0x0a` ブリージング / `0x03` サイクル。
- プロファイルの lighting スロット 0/1 が zone 0/1 に対応 (slot 2/3 は未使用)。
- パラメータ位置は HID++ の LEDEffects 定義と同じ:
  固定 = color、ブリージング = color + period(2B BE) + form + intensity、
  サイクル = period + intensity。

## 3. 実機で確認した往復

`cargo run -p devhub-hid --example logie2e` で以下を確認済み (2026-11):

- DPI ステージ書き換え → 読み戻し (800 / 2400 / 3200 / 6400)
- ポーリング 1000Hz 書き込み → 読み戻し
- ライティング 2 ゾーンを赤固定 → 読み戻し
- スナイパーボタンを Ctrl+C に変更 → 読み戻し → スナイパーに戻す
- オンボードモード切替 (`mode=1`) と active profile (`00 01`)
- プロファイル名 (UTF-16LE) の読み出し

## 4. G HUB との関係 / 移行

- G HUB はホストモード (`mode=2`) で動き、設定を 0x2201 / 0x8070 で
  RAM に流し込む。プロファイルは使わない。
- 本ドライバは起動時にホストモードなら、現在の DPI とレートを
  プロファイルへ引き継いでから `mode=1` にする。以後はプロファイルが真実。
- G HUB を消してもマウスは通常の HID マウスとしてそのまま動く。
  設定はオンボードプロファイルに残る。
- G HUB と同時起動すると、G HUB がホストモードへ戻して設定を
  上書きすることがある。置き換えるなら G HUB は止めること。

## 5. 未対応 / 今後

- マクロ (プロファイルのマクロセクタ) は未実装。UI の `MacroCaps` は出していない。
- ファームウェア更新 (0x00c2 DFU) は未実装。
- アングルスナップ (profile byte 17) は書き込み経路だけ用意 (UI 未露出)。
- G502 X など別 PID は registry に足すだけで動く見込み (要実機確認)。
