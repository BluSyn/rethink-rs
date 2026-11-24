# ThinQ2 reverse-engineering notes

Protocol quick reference for reverse-engineering ThinQ2 appliances. For the current list of supported and unsupported models, see [`README.md`](../../README.md#status-of-this-fork).

## Wire protocols (ThinQ2)

### CLIP / MQTT ([`Thinq2CloudProtocol.md`](Thinq2CloudProtocol.md))
- After SoftAP Wi-Fi join, devices use HTTPS + MQTTS; no inbound device listeners.
- Provisioning: `https://common.lgthinq.com/route` → `apiServer` / `mqttServer` (any TLS cert accepted on route).
- Device cert: CSR to `apiServer/device/{uuid}/certificate`; MQTT client uses mutual TLS (device cert + AWS IoT CA).
- Topics: subscribe `lime/devices/{did}`; provision on `clip/provisioning/devices/{did}` (`preDeploy` / `deploy`); traffic on `clip/message/devices/{did}`.
- CLIP JSON fields: `mid`, `did`, `kind` (modelId), `cmd`, `type`, `data` (hex for UART payloads).
- **Cmds of interest:** `preDeploy`, `deploy`, `completeProvisioning` / `_ack`, `device_packet` (device→cloud UART), `packet` (cloud→device), `req_timesync`.

### UART TLV framing (`TLVProtocol` wiki + `rethink-util`)
- MQTT wraps UART frames: prefix `a`/`s` reliability bytes (toDevice) or `0000` (fromDevice).
- UART: `04 00 00 00` + kind (`65` toDevice, `87`/`a7` fromDevice) + byte5/6/7 + len + TLV + CRC16-XMODEM over UART body.
- Kind **`0xa7`**: CST cassette / some DHUM platforms (same TLV body; codec must accept it).
- TLV: 10-bit type, 2-bit length 0–3, nibble or multi-byte value (`rethink_util::tlv`).
- **Query tags:** `0x1f5=1` capabilities, `0x1f5=2` values (clip “values query”).

### Common TLV tags (AC / climate / humidity family)
| Tag | Meaning | Sources |
|-----|---------|---------|
| 0x1f5 | Query type (1=caps, 2=values) | wiki, RAC/DHUM/HUM |
| 0x1f7 | Power | RAC, DHUM, HUM, WIN, POT |
| 0x1f9 | Operating mode | RAC, DHUM, HUM |
| 0x1fa | Fan / wind strength | RAC, DHUM, HUM |
| 0x1fd | Current temp (½°C) | RAC, HUM |
| 0x1fe | Setpoint temp | RAC, WIN, POT |
| 0x253 | Target humidity % | DHUM, HUM |
| 0x336 | Current humidity | CST PR#129, DHUM, HUM |
| 0x2da | EEPROM checksum (caps marker) | tlv_device / RAC |
| 0x2cc–0x2d3 | Feature bits (jet, energy save, timers, swing) | RAC |
| 0x20e/0x20f/0x20d | Autodry / airclean / related | RAC CST unlock from state tags |
| 0x1e3 | Product status (HUM) | PR#114 modelJSON |
| 0x1e4 / 0x1e6 / 0x109 | Sleep / auto / night (HUM) | PR#114 |

### AABB framing (`AABBProtocol` wiki + laundry/fridge/hood)
- Envelope: `AA` + length + body + checksum `((sum)&0xff)^0x55` + `BB`.
- Laundry status often kind `0x20` EB/EC; dryers `0x30`; fridges `0x10` EB/EC; hood `0x43` EB/EC.
- Status query / monitor enable: `F0ED…` patterns (model-specific).
- Set commands: `F017…` (fridge), `F0xx…` (washer/dryer/hood).

### SoftAP / setup ([`SetupProtocol:JSON.md`](SetupProtocol:JSON.md))
- SoftAP IP typically `192.168.120.254`; JSON length-prefixed frames and XML variants.
- Newer modules: RTK_RTL8711am / RTL8720cm / BEKEN_BK7234 — need relaxed TLS ciphers (`SECLEVEL=0`, TLS 1.0+) and SoftAP `setApInfo` handling.

## External reverse-engineering sources

| Source | What it contributes |
|--------|---------------------|
| [rethink wiki](https://github.com/anszom/rethink/wiki) | TLV/AABB/CLIP SoftAP specs; per-appliance notes (RAC, WIN, washers, fridges, `WHT_056905_WW` water heater) |
| [rvanbaalen/lg-local](https://github.com/rvanbaalen/lg-local) | Independent local control; TLV parser + CRC16; points back to rethink wiki as protocol authority |
| LG modelJSON (`tlv_*` labels) | Authoritative tag→field map for models like `HUM_056905_WW`; used instead of packet guessing |
| Bridge dual-decode | Verifies fields against LG cloud while bridged |

## DeviceType quick reference

Management UI shows **name + code** (e.g. `A/C (401)`). Map lives in `html/panel.js` (`DEVICE_TYPE_NAMES`).

| deviceType | Class | Examples |
|------------|-------|----------|
| 101 | Refrigerator | `2REF*`, `2RES*`, `2REB*` |
| 102 | Kimchi Refrigerator | |
| 103 | Water purifier | `1WPU4CIGCR__2`, `1WPD6CMIDR__3` |
| 201 | Washer | `F_V*`, `Y_V*`, `VCDWL2QEUK`, `WTDN3` |
| 202 | Dryer | `RV13*`, `RH10V9_CH`, `RC90*` |
| 203 | Styler | `ST_B_E4H01Y_APL`, `S5MPC` |
| 204 | Dishwasher | `D0211`, `N17`, `N02`, `H11` |
| 221/222/223 | WashTower | `WTL_FXU_BDV_NA_01`, `FAKPK21021`, `BDH_D39301_KR` |
| 301 | Oven / Range | `WFV474PGV`, `WLSI_633_`, `WLREL6323S` |
| 302 | Microwave | `WMVEM1825`, `WMVEL2137`, `WMHES1738` |
| 303 | Cooktop | |
| 304 | Range Hood | `STUDIO_HOOD` |
| 401 | A/C | `RAC_*`, `WIN_*`, `CST_*`, `POT_*`, `CSL_*` |
| 402 | Air Purifier | `AIR_910604_WW` |
| 403 | Dehumidifier | `DHUM_*` |
| 404 | Humidifier | `HUM_056905_WW` |
| 406 | Water heater | `WHT_056905_WW` |
| 501 | Robot Vacuum | |
| 504 | Vacuum | `HWWA9K_F2`, `HWWA9S_F2U` |

## Adding support

1. Implement a handler under `crates/rethink-devices/src/devices/` (tests in the same file).
2. Add one line to the `register_devices!` table in `crates/rethink-devices/src/devices/mod.rs`.
3. Add fixture tests from captures.
4. See [`Adding-support-for-a-new-device.md`](Adding-support-for-a-new-device.md) and the management UI RE panel.

## Management RE tooling

- Device list + detail (meta, HA properties, platform).
- Server-side TLV/AABB decode (`/api/decode`).
- Unknown-tag classification vs known catalog (`/api/re/unknowns`).
- One-click text export for unknown traffic.
