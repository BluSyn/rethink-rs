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

A working `rethink-cloud` binary exists and can enroll appliances, serve MQTT discovery to Home Assistant, and run the management UI. Appliance coverage is a subset of what upstream documents, plus some models ported from open upstream PRs. Treat the list below as “what this snapshot compiled,” not a support promise.

Supported in this tree (model IDs in `crates/rethink-devices`):

- **Air conditioners:** RAC / CST DualCool family, WIN window AC, POT portable, PAC floor-standing (`PAC_910604_WW`), DUCT ducted (`DUCT_626301_WW`)
- **Air & water:** DHUM dehumidifiers, HUM air-purifying humidifier, `AIR_910604_WW` air purifier, `VENT_725601_WW` ventilator, `WHT_056905_WW` heat-pump water heater, `1WPU4CIGCR__2` / `1WPD6CMIDR__3` water purifiers
- **Fridges:** several `2REF*` / `2RES*` / `2REB*` AABB models, including the GLUB bottom freezers
- **Washers:** F_ / Y_ / G2 UK / EU / AU front-loaders, VCDWL2QEUK, US front-loaders (F3L2 / F3L7 / F3M2 / F3P2 / F3P3, F24VDD, FAFXU25006), top-loaders (T1789 / T17A1, TLVE5BB), the MI2U7B SideKick pedestal washer, ThinQ1 washers (WTDN3, WTWN3, F3L2CNV4W_WIFI, F3L2CYV5W_WIFI)
- **Dryers & combos:** RV13 US dryers, BDH_D30007_US, heat-pump dryers (RH10, RH16, RC90U2 on ThinQ2 and ThinQ1 incl. RC90V9 / RH90V9, SDH_GVX5_6211), washer/dryers (F_V__F___W.B__ECON, Y_V8_F, V_V__F)
- **WashTower:** WTL_FXU_BDV_NA_01, WKEX200HBA (F3S1CWKB__ + RV13S1AMJW_US)
- **Stylers:** S5MPC, ST_B_E4H01Y_APL
- **Kitchen:** dishwashers (H11, N17, N02, D0211 / D2211, D2201, D30), STUDIO_HOOD range hood, ranges and ovens (WFV474PGV, WLSI_633_, WLREL6323S), microwaves (WMVEM1825, WMVEL2137)
- **Vacuums:** CordZero A9 stick vacuums (HWWA9K_F2, HWWA9S_F2U)

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
