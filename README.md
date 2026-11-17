# rethink (Rust rewrite)

**This repository is a full Rust rewrite of [anszom/rethink](https://github.com/anszom/rethink).**

The original project — design, reverse engineering, protocol documentation, device maps, and the TypeScript/`rethink-cloud` implementation — is the work of [Andrzej Szombierski](https://github.com/anszom) and [the upstream contributors](https://github.com/anszom/rethink/graphs/contributors). This tree reimplements that system in Rust. Credit for the project belongs upstream.

## Use upstream instead

**If you want to run rethink, report a bug, request a device, or send a patch, go to the original repo:**

**→ [https://github.com/anszom/rethink](https://github.com/anszom/rethink)**

That is the maintained project. Installation, wiki, issues, and PRs live there.

This Rust fork is **not actively maintained**. It is a snapshot / experiment:

- Do **not** open issues or PRs here expecting review or merge.
- Device support, bugfixes, and new work should target **upstream**.
- The wiki and protocol notes you actually want are on the [upstream wiki](https://github.com/anszom/rethink/wiki).

If you already have this tree checked out, the rest of this file is only enough to build it.

---

## What rethink does

Rethink de-clouds LG ThinQ appliances: it talks to them without the official LG app/cloud and exposes them as Home Assistant MQTT. An optional **bridge** mode can still forward traffic to LG’s cloud (useful for reverse engineering, or to keep the official app working).

This fork is a **Rust-only** runtime. The former TypeScript/Node application was removed; `html/` remains as static assets for the management UI.

## Status of this fork

A working `rethink-cloud` binary exists and can enroll appliances, serve MQTT discovery to Home Assistant, and run the management UI. The tables below are what this snapshot's `register_devices!` table actually registers. They are not a support promise, and they do not copy upstream's maturity ratings.

An alias is the same handler, not a second implementation. Where one alias drops an entity, the row says so. The same `modelId` can be registered twice when ThinQ1 and ThinQ2 need different handlers.

### Air conditioners

| ThinQ modelId | Sold as | This handler |
| --- | --- | --- |
| `RAC_056905_WW`, `RAC_0B0001_WW` | DualCool wall IDU. `RAC_0B0001_WW` is an alias of the ThinQ2 handler. The same `RAC_056905_WW` id on ThinQ1 (WH09SKN-18) is `rac_056905_ww_t1`. | ThinQ2: climate, fan, and swing, plus Jet, energy save, AI Air, and Viraat when the unit reports those caps. Display light also needs a known firmware checksum. ThinQ1: JSON status on the monitoring socket. |
| `WIN_056905_WW` | Window AC, for example LW1823HRSM | Climate: power, mode, setpoint, fan, and swing. |
| `POT_056905_WW` | LP1022FVSM portable | Climate plus a Jet switch. |
| `PAC_910604_WW` | FQ18PADNQN floor-standing | Climate, two-vane swing, wind mode, human sense, and air quality. No filter reset. |
| `DUCT_626301_WW` | Ducted IDU (PWFMDD200) | Climate with this unit's fan speeds: 2/4/6 are low/medium/high. |
| `CST_570004_WW` | Ceiling cassette | DualCool TLV with its own mode, fan, and swing tables. Not a RAC alias. |

### Air, water, and ventilation

| ThinQ modelId | Sold as | This handler |
| --- | --- | --- |
| `DHUM_056905_WW` | MD19GQGE0 dehumidifier | Humidifier, fan speed, current humidity, ionizer, UV nano, and bucket. |
| `DHUM_231006_WW` | Dehumidifier, protocolVer 7 | The DHUM_056905 map with this model's own mode and fan codes. |
| `HUM_056905_WW` | PuriCare humidifier | Power, mode, fan, target humidity, night, sleep, and auto. |
| `AIR_910604_WW` | AS309DWA air purifier | Power, mode, and fan. Room temperature and humidity are not published. |
| `VENT_725601_WW`, `VENT_725601`, `Z-H0151B2SR` | Heat-recovery ventilator. The last two ids are aliases. | One fan entity for power, speed, and preset, plus CO2 and particulate readings. |
| `WHT_056905_WW` | WH27S / APHWC801M heat-pump water heater | Water-heater climate (heat pump, eco, performance, off), power draw, and hot water remaining. |
| `1WPU4CIGCR__2` | PuriCare water purifier (ATOM-U) | Read-only state and dispense totals. |
| `1WPD6CMIDR__3` | WD722RH water purifier | State plus the settings whose ThinQ-app write was captured. Ice, dispensing, and sterilisation stay read-only. |

### Refrigerators

| ThinQ modelId | Sold as | This handler |
| --- | --- | --- |
| `2REF11EIDA__4` | LF28H8330S | Fridge and freezer setpoints, and the flex drawer. |
| `2REF11EIDG__4` | LRFXC2606S / LRFXS3106S | Setpoints, Ice Plus, Sabbath, Smart Learner, filter life, and water use. |
| `2REF11EICT__4` | LRYXC2606S | The EIDG controls, plus Craft Ice and the ice-maker states. Sabbath schedule is left out. |
| `2REF11EBIVPC4` | Unknown | Celsius setpoints in this model's raw units, door, express freeze, and Shabbat. |
| `2REF11EII__M2` | LMXS28626S | Setpoints from the model-JSON maps. This handler does not ack the appliance. |
| `2REF12EII_P_2` | GML844 slim multi-door | Fridge and freezer setpoints with this model's own formulas. |
| `2REFT1DIC4P_U` | GF-V700BSLC | Setpoints, express freeze, and Craft Ice. |
| `2REFTBDII4P_U` | GF-L500MBL | Setpoints. A temperature write also sets the unit byte. |
| `2RES1VE61NFA2` | GSJV70PZTE | Fridge and freezer setpoints, express cool, and express freeze. |
| `2REB1GLVB1__2` | GSB470BASZ | Setpoints, express cool, express freeze, and eco friendly. |
| `2REBGLUB_2P__` | GBBSJ2CCEP | Setpoints, Super Cool, Vacation, Fresh Converter, door counts, energy, and night view. |
| `2REBGLUBD1P_A` | GBBSJ11ESW | Setpoints, Super Freeze, Vacation, and Super Cool. Only the fridge door is reported. |
| `2RES1VE600FWC`, `2RES1VE61NFWA` | GA-B509CMUM, and GSX961MCCZ for the alias | Setpoints and express freeze. Only `2RES1VE61NFWA` publishes Pure N Fresh filter health. |

### Washing machines

| ThinQ modelId | Sold as | This handler |
| --- | --- | --- |
| `Y_V8_Y___W.B32QEUK` | FSR7A04PG | Course and options. Run programme sends one `F0 26` frame. |
| `Y_V8_Y___W.B_2QEUK` | F2WR709S2W | Telemetry of `Y_V8_Y___W.B32QEUK`. Start, pause, Delay End, and the wash-option switches are withdrawn. |
| `F_V7_Y___W.B_2QEUK`, `F_V8_Y___W.B_2QEUK`, `F_V__Y___W.B_2QEUK`, `F_V7_Y___W.B__QEUK`, `F_V__Y___W.B__QEUK`, `F_V3_Y___W.B_2QEUK` | F4WV508S2E, F4WV709P1E (32-byte F4WV909P2TE), TW4V9RW9W, F2V5PS0W, WV9-1609B, F82AV35WHS. All aliases of one handler. | Course, spin, temperature, power, start, and pause. A 32-byte record also gets remote start and Delay End. |
| `F_VB_Y___W.B_2QEUK` | F4WV710ATR | The F_V8 record plus detergent and softener doses. Child lock is not exposed. |
| `F_C__Y___W.A__QEUK`, `F_V7_Y___W.A__QEUK` | F4WV709P1, and WV5-1275W for the alias | 30-byte status, door lock, power, start, and pause. |
| `F_V__Y___W.A__QEUK` | WV9-1409W | Course at its own offset. Start sends one `F0 26` with the settings held in HA. |
| `G2V7_Y___W.B_2QEUK` | F2NX50S9TKC | Course, temperature, spin, and power, start, and pause. |
| `VCDWL2QEUK` | F4X7511TWS | Read-only course and status. Power is off only when the status byte is 0. |
| `F_V__F___W.B_1QEUK`, `F_VA_F___W.B__QEUK` | FV1413H2B / FV1413H2BA. The second id is an alias. | Course, spin, temperature, dry mode, energy, and power/start/pause. |
| `F_VB_F___W.B_2QEUK` | CV74J7S2QA washer/dryer | Course plus eco hybrid, prewash, and steam. Power, start, and pause. |
| `F_V__F___W.B__ECON` | CV9014WC2 washer/dryer | Staged course (spin, temperature, rinse, dry, delay) then start. |
| `Y_V8_F___W.B_2QEUK` | W4WR70E61 washer/dryer | Wash and dry fields, plus the wash-cycle counter. |
| `Y_VB_Y___W.B32QEUK` | F14R76BSTA and F4W1175YW | The Y_V8 record plus ezDispense doses, the drawer, and both tank-low alerts. |
| `V_V__F___W.B_2QEUK` | W4X1085NWH washer/dryer | Power, pause, and run programme. It does not resume a pause. |
| `F3L2CYU__` | WM3900HBA | Read-only power, status, and course. |
| `F3L7CYK5W_US_WIFI` | Unknown | Read-only status, course, child lock, rinse, initial time, and tub-clean count. |
| `F3M2CYK__` | WM4500HBA | Read-only. Power comes from the heartbeat, not the short status record. |
| `F3P2CYUBE__`, `F3P2CYUBW__` | WM4000HWA, and WM3850HVA for the alias | Course, options, and delay, written only while Remote Start is armed. The alias publishes no TurboWash. |
| `F3P2CYVAT__` | WM3600HWA | Read-only course, time, and tub-clean count. |
| `F3P3CYK2_` | WM4200HBA | Status and course. Only power-off is writable. |
| `F24VDD` | Unknown | Course, spin, temperature, rinse, and reserve. Start and resume send the captured template. |
| `FAFXU25006` | WM5800HVA | Read-only 72-byte status. |
| `T1789EFH_F` | WT7300CW top-loader | Course and options from the marker-led record. Stale `0xE2` snapshots are ignored. |
| `T17A1EFHU_F` | WT7305CV / WT7305CW | Course, delay, extra rinses, pause, and power off. Smart-course defaults come from this model's modelJSON. |
| `TLVE5BB` | WT21BWV6 | Read-only 50-byte status. |
| `MI2U7B` | SideKick pedestal washer | Course, state, and drawer. Writes wait for a fresh powered-on report. |
| `WTDN3` | F2J7HG1W (ThinQ1) | Course, rinse, child lock, delay, and tub-clean count. Remaining time is 0 while off. Power, start, and pause. |
| `WTWN3`, `WTWN3H` | F4J7TN1W, and F4J7VY2WD for the alias | ThinQ1 course, wash, rinse, and options. |
| `F3L2CNV4W_WIFI` | WM3500CW (ThinQ1) | Course and remote start. There is no power-on command. |
| `F3L2CYV5W_WIFI` | WM3700HVA (ThinQ1) | Read-only status, course, soil, spin, and temperature. |

### Dryers and combos

| ThinQ modelId | Sold as | This handler |
| --- | --- | --- |
| `RV13U6AM8W_D_US_WIFI` | DLE7300WE / DLG7301WE | Course (the old cycle topic is withdrawn), dry level, temperature, and error. Start is rebuilt from the last record. |
| `RV13B6BSD_D_US_WIFI` | DLEX3900B | Read-only course, dry level, temperature, and error. |
| `RV13B6ES_D_US_WIFI` | Unknown | Read-only, plus signal, more/less time, and load-item count. |
| `RV13D4ASJW_D_US` | DLEX4000W | Course, Time Dry, and AI. Writes need Remote Start. |
| `RV13D5JSD_D_US` | DLEX4200B | Read-only status except a power-off button. Completion stays latched through power-off. |
| `BDH_D30007_US` | DLHC5502V | Read-only course, dry level, eco hybrid, and the newer error table. |
| `RH10V9_CH` | Unknown heat-pump dryer | Read-only phase, live remaining minutes, and course. |
| `RH16_T_KR` | RH16VT | Course start with reserve hours, and the ten captured download courses. |
| `RC90U2_WW`, `RC90V9_WW`, `RC90V9_AUS`, `RH90V9_WW` | RC9DN9029, RC91V9EV2Q, DVH9-09B, and RC10V9AV2W. The last three are aliases. The same `RC90U2_WW` id on ThinQ1 is `rc90u2_ww_t1`. | Course, dry level, eco hybrid, power, and pause, on both the AABB handler and the ThinQ1 adapter. `RH90V9_WW` adds Cotton+ and Tub Clean and drops its old extra entities. |
| `SDH_GVX5_6211` | RHX5009NHB | Course and options. Power on also wakes the dryer. |

### WashTowers

| ThinQ modelId | Sold as | This handler |
| --- | --- | --- |
| `WTL_FXU_BDV_NA_01` | WKEX200HBA, one module | Washer and dryer state, course, and time on one device. |
| `F3S1CWKB__` | WKEX200HBA washer half | Course and power. The run phase comes from the status dump, not the short record. |
| `RV13S1AMJW_US` | WKEX200HBA dryer half | Course, dry level, and a power switch. Not an RV13B6 alias. |

### Dishwashers

| ThinQ modelId | Sold as | This handler |
| --- | --- | --- |
| `H11` | DUE2BG.AKOR | Course, state, and energy. Controls are `F0 26`. |
| `N17` | LDNPQ445S | Course and settings. Power off is not cancel. |
| `N02` | LDPH7972S | The N17 settings plus Time Indicator, Dry Boost, and Night Dry. Loading a download cycle is not implemented. |
| `D0211`, `D2211` | DB365TXS / DBC435TSL, and XD3A25MB for the alias | Read-only. Course and options publish only while running. |
| `D2201` | XD3A15MB | The D0211 record plus rinse, softening, and flags. A smart course is stored, not started. |
| `D30` | LDT54788D | Read-only course and options, including in Ready and Pause. |

### Kitchen

| ThinQ modelId | Sold as | This handler |
| --- | --- | --- |
| `STUDIO_HOOD` | HCED3015D, likely other hoods | Hood fan and light from the `0x43` status. |
| `WFV474PGV` | Unknown double oven | Both cavities' state, a gated remote start, timer, and cancel. Preference rows are write-only. |
| `WLSI_633_` | LSIS6338FE | Read-only oven and cooktop on the 115-byte record. |
| `WLREL6323S` | LREL6323S | Read-only oven and cooktop, plus clock and beeper writes. |
| `WMVEM1825` | MVEM1825D/F | Oven state, hood fan and light, clock, and beeper. |
| `WMVEL2137` | MVEL2033F | The same, with four vent speeds, sound, and clock sync. |

### Stylers and vacuums

| ThinQ modelId | Sold as | This handler |
| --- | --- | --- |
| `ST_B_E4H01Y_APL` | S5BBP | Course, state, and the captured `ControlWifi` writes. Energy and the internal light are not published. |
| `S5MPC` | S5MPC | Its own 27-byte record: state, course, remaining time, and cycle energy. |
| `HWWA9K_F2` | CordZero A9 | State and the five captured one-byte settings. |
| `HWWA9S_F2U` | CordZero A9K (A9K-CORE3S) | Read-only 12-byte status. The content-sync envelopes are acked so the handle stays on Wi-Fi. |

### Not yet supported

These ThinQ `modelId`s are named by open pull requests on [anszom/rethink](https://github.com/anszom/rethink) and are not in this registry. Upstream master `4e6028125cda6bdd8b18cb233841e1a5d0381bce` (fetched 2026-11-17) does not add any further device module beyond the ids above. The open-PR search `repo:anszom/rethink is:pr is:open` returned 100 that day. A product name already covered by a row above is not repeated here.

| ThinQ modelId | Sold as | Named by |
| --- | --- | --- |
| `2REF11EICDX_2` | LHFS28XBS | Open PR #304 |
| `2REF11EID___2` | LFXC22526S (ThinQ1) | Open PR #320 |
| `2REF12EICD__2` | Unknown fridge | Open PR #273 |
| `2REF12EII_N_2` | GC-L228FTL1 | Open PR #308 |
| `2REF12EJDD__2` | LRFDS3006D | Open PR #337 |
| `2RES1VE41PFA2` | Unknown side-by-side | Open PR #298 |
| `2RES1VE61PFA2` | GC-L257SLP | Open PR #324 |
| `BDH_D39301_KR` | Korean WashTower dryer half | Open PR #117 |
| `BDH_HI2103_EU` | RH18U8AVCW | Open PR #315 |
| `BDVG_FX0003_US` | DLEX6700B | Open PR #204 |
| `CSL_306406_WW` | Console air conditioner | Open PR #343 |
| `D3210` | LDP6810SS | Open PR #329 |
| `F3S1CWK__` | WKGX201HWA washer half | Open PR #300 |
| `FAFXU22007` | WM6700HBA | Open PR #204 |
| `FAKPK21021` | Korean WashTower washer half | Open PR #117 |
| `F_V7_F___W.B__QEUK` | CV5012PC4A | Open PR #308 |
| `F_V__F___W.B__QEUK` | CV9011EC4 | Open PR #341 |
| `RV13B6AM_D_US_WIFI` | DLE3500W | Open PRs #326 and #336 |
| `RV13U7JS9W_D_US_WIFI` | DLEX7800VE | Open PR #329 |
| `T1872EFHU_F` | WT7800CV | Open PR #329 |
| `TLXA5CFPH` | Unknown | Open PR #342 |
| `VIC2_799_US_FACTORY` | A WM3500CW variant. Not the ThinQ1 WM3500CW already registered. | Open PR #336 |
| `W3S1CVK2EVS_D_US` | WKGX201HWA dryer half | Open PR #300 |
| `WLSEL6333F` | LSEL6333F | Open PRs #304 and #329 |
| `WLRGL5825F` | Unknown range | Open PR #273 |
| `WMHES1738` | MHES1738F | Open PR #323 |
| `WMVEF1337F` | MVEF1337F | Open PR #339 |
| `Y_V8_F___W.B32QEUK` | FWY916WBTN1 | Open PR #316 |

Unlisted appliances can still be used in **bridge mode** (no HA translation). For new devices or gaps, contribute **upstream**, not here.

Research notes in this repo: [`docs/research/`](docs/research/) (start with [`THINQ2_RESEARCH.md`](docs/research/THINQ2_RESEARCH.md)). Canonical docs: [upstream wiki](https://github.com/anszom/rethink/wiki).

## Build & run (this snapshot)

Requirements: Rust **1.88+**, OpenSSL CLI (CA / device CSR signing).

```bash
cargo build -p rethink-cloud -p rethink-setup -p rethink-bridge -p rethink-tools
cargo test --workspace
cargo run -p rethink-cloud -- ./config.jsonc
cargo run -p rethink-setup -- 192.168.120.254 'MySSID' 'MyPassword!'
```

Docker:

```bash
docker build -t rethink .
docker run --rm -p 443:443 -p 8883:8883 -p 44401:44401 -v rethink-data:/app/data rethink
```

Network / DNS (`rethink.lgthinq.com`, etc.): follow **[upstream installation](https://github.com/anszom/rethink/wiki/Installing-rethink‐cloud)**.

### Workspace

| Crate | Role |
|-------|------|
| `rethink-util` | Codecs (TLV, CRC16, framing, MTOSP) |
| `rethink-core` | Config, HA types, device bases, ThinQ traits |
| `rethink-devices` | Per-model HA handlers + `modelId` registry |
| `rethink-bridge` | Optional LG-cloud bridge helpers |
| `rethink-cloud` | Main server |
| `rethink-setup` | SoftAP provisioning CLI |
| `rethink-tools` | packet-parser / packet-sender / capture / MCP / lgcloud-monitor / thinq-control |

Adding a device in *this* tree (for your own fork only): implement `crates/rethink-devices/src/devices/<module>.rs` with tests in the same file, then one line in the `register_devices!` table in `devices/mod.rs`. Prefer sending that work **upstream**.

Management UI defaults to port **44401**.

## Credits

- **Original project:** [anszom/rethink](https://github.com/anszom/rethink) by Andrzej Szombierski and contributors.
- **This rewrite:** a Rust reimplementation of that work. It would not exist without the upstream reverse engineering and Node/TypeScript codebase.

## Notice

LG ThinQ is used here for identification only. This project is not affiliated with LG.

## Warning

    This program is distributed in the hope that it will be useful,
    but WITHOUT ANY WARRANTY; without even the implied warranty of
    MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
    GNU General Public License for more details.

If your device breaks, you get to fix it yourself or keep both pieces.
