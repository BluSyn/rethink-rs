# Agents

This tree is a Rust rewrite of [anszom/rethink](https://github.com/anszom/rethink). Upstream is TypeScript and is the maintained project. Do not copy TypeScript tooling, CI, or docs-only changes. Port behavior into the Rust handlers, and do not reimplement something this tree already does.

Do not push, and do not open a pull request, unless the user asks.

## Layout

Workspace crates, from `Cargo.toml`:

| Crate | Role |
| --- | --- |
| `rethink-util` | Codecs: TLV, AABB, CRC16, hex, MTOSP |
| `rethink-core` | Config, Home Assistant types, `AabbDeviceCore` / `TlvDeviceCore`, ThinQ traits |
| `rethink-devices` | Per-model handlers and the `modelId` registry |
| `rethink-bridge` | Optional LG-cloud bridge |
| `rethink-cloud` | Server: enrollment, internal MQTT, management UI, ThinQ1/ThinQ2 acceptors |
| `rethink-setup` | SoftAP provisioning CLI |
| `rethink-tools` | `packet-parser`, `packet-sender`, `rethink-capture`, `rethink-mcp`, `lgcloud-monitor`, `thinq-control` |

Other paths: `html/` is the management UI, `config.jsonc` is the server config, `docs/research/` is local protocol notes, `Dockerfile` builds `rethink-cloud`.

A device handler lives in `crates/rethink-devices/src/devices/<module>.rs`. Register it with one arm of `register_devices!` in `devices/mod.rs`:

```text
module => { "MODEL_ID" | "ALIAS" },
```

`t2` is ThinQ2. `t1` is ThinQ1. The same `modelId` may appear in both when the protocols differ (`RAC_056905_WW`, `RC90U2_WW`). An alias is the same `create` function, not a second handler. Module names track the LG `modelId`.

Shared tables: `washer_common.rs`, `fridge_common.rs`, `ac_tables.rs`, `tl_us_washer_courses.rs`. Protocol notes: `docs/research/Adding-support-for-a-new-device.md`.

## Build and test

Rust 1.88 or newer. The image is `rust:1.99-bookworm`.

```bash
cargo test --workspace --offline -- --test-threads=8
cargo test --offline -p rethink-devices --lib -- <filter> -- --test-threads=8
```

`--test-threads` belongs after `--`. Do not pipe `cargo test` through `head`.

Device tests live in the handler file. Start from `Device::new` or the registry factory, feed a captured frame or command through the real handler, and assert the published property or the bytes sent. Do not reimplement the decoder inside the test.

`rustfmt` on `devices/mod.rs` or a crate `lib.rs` walks every sibling module. Format only the file you are editing, or skip formatting a pre-existing file when the diff would be unrelated.

## Supported-device list

`README.md`, under **Status of this fork**, is the published device list. It has two tables:

- **Supported** rows, grouped by appliance kind: ThinQ `modelId`, sold-as name, and one short phrase for what this Rust handler does.
- **Not yet supported**: model ids named by upstream master or by open upstream pull requests that this registry does not implement. The section names the master SHA and the fetch date.

Update that list in the same change whenever device support changes. That includes a new module, a new or removed alias, a model id moved between handlers, a control added or withdrawn, a handler becoming read-only or gaining a write, or one alias dropping an entity the others keep.

When you edit the list:

- Every string in `register_devices!` appears once in the supported tables. Every id in those tables is registered. No id is in both the supported tables and the not-yet-supported table.
- An alias is labeled as an alias of its handler. If that alias omits something (`F3P2CYUBW__` has no TurboWash; only `2RES1VE61NFWA` publishes the Pure N Fresh filter), say so on the row.
- When one `modelId` is registered for both ThinQ1 and ThinQ2, the row says they are different handlers.
- A sold-as name is used only when this handler or upstream states it. Otherwise write "Unknown".
- Say read-only when `set_property` ignores every write. Name a real exception (`RV13D5JSD_D_US` is read-only except power off).
- Do not copy upstream emoji ratings. Do not claim a sensor or command the module does not publish.
- Porting a not-yet-supported id removes it from that table. Refreshing the snapshot names the new master SHA, the fetch date, and the open-PR search.

After the edit, the set of backtick ids in the supported tables equals the set of `register_devices!` literals, and none of the not-yet-supported ids are in that set.

## Behavior changes

Match the existing handler: publish order, `None` for an unmapped code, tombstones for a withdrawn entity, and the real command bytes. A read-only handler stays read-only unless the change is the write itself.

Prefer a captured frame through the shipped handler over a unit test of a private table. Leave unrelated formatting and unused-import warnings that were already there.
