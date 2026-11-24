This page is a practical guide for contributors who want to add support for a new appliance to rethink. "Support" here means translating a device's binary protocol into Home Assistant entities, so that the appliance can be monitored and controlled without the LG cloud.

The general approach is reverse-engineering: get the device talking, watch its traffic, figure out which bytes mean what, and encode that knowledge in a device class. After the one-time [Setup](#setup), you can follow the [Manual workflow](#manual-workflow) or the [LLM-assisted workflow](#llm-assisted-workflow) - both reach the same place, and you can mix them. The steps are roughly ordered, but in practice you'll loop between them.

# Guidelines

- The primary goal is to map the device protocol to Home Assistant with little or no extra logic. Expose what the device reports and accepts, faithfully.
- Do not implement behaviour such as timers, schedules or safety locks. The user can configure these in Home Assistant, where they belong.
- If the device's internal safety features can be circumvented, do not do so intentionally.
- LLM-generated code is fine — as long as you understand it and can provide a rationale for the decisions taken. Unexplained or unreviewed generated code will not be accepted.

# Setup

## Prerequisites

- A working `rethink-cloud` installation — see the [installation guide](https://github.com/anszom/rethink/wiki/Installing-rethink%E2%80%90cloud).
- The device's **ThinQ Model ID** (e.g. `RAC_056905_WW`, `2REF11EIDA__4`). This is printed during provisioning and is the key used to register a device class in the code.
- Ideally, an LG ThinQ account and the official app, so you can use [bridge mode](https://github.com/anszom/rethink/wiki/Bridge) and observe how LG's own cloud handles the device. This is the single most valuable reverse-engineering aid.
- Find or create an [issue](https://github.com/anszom/rethink/issues) for the unsupported device. This will allow other users to see that you're working on it.

## Provision the device

Run the device through the normal provisioning ("setup") process so that it connects to `rethink-cloud`. See the [setup protocol](SetupProtocol.md) and the installation guide.

**Provisioning is largely protocol-agnostic** — it usually succeeds even for devices rethink does not yet translate, because it only configures Wi-Fi and cloud endpoints. So you can provision an unsupported device and still observe its traffic.

If provisioning fails for your device, see [issue #58](https://github.com/anszom/rethink/issues/58).

Once provisioning completes, the device should appear in the [web management panel](https://github.com/anszom/rethink/wiki/Bridge#device-setup) (default port `44401`).

## Bridge the device to the official cloud

Enable [bridge mode](https://github.com/anszom/rethink/wiki/Bridge) for the device. In bridge mode, rethink forwards the device's traffic to the real LG cloud while still letting you observe every packet and inject new ones. This works even if the device's protocol is not supported yet.

In the bridge mode, the official LG app keeps working, so you can **toggle real settings and issue real commands** and watch what travels over the wire. Each packet sent to/from the device can be observed in the management panel and rethink-cloud logs.

# Manual workflow

With the device provisioned and bridged, this is the classic loop: identify the protocol, watch traffic while you operate the device, figure out the fields, and encode them in a device class.

## Identify the protocol variant

ThinQ2 devices have been observed to use one of two binary formats. Determine which one your device uses by inspecting the packets in the management panel's device monitor, or in the logs (enable the `incoming` log topic in `config.jsonc`).

- **[TLV protocol](TLVProtocol.md)** — type/length/value-encoded fields, framed with a CRC16. Used e.g. by DualCool AC. Use `TlvDeviceCore` in Rust.
- **[AABB protocol](AABBProtocol.md)** — fixed-layout packets bracketed by `0xAA`…`0xBB` with a simple checksum. Used e.g. by fridges and newer washers. Use `AabbDeviceCore`.

ThinQ1 devices (like WTDN3) use the older XML-style protocol and implement `DeviceHandler` with ThinQ1 mocks/adapters.

For **TLV** devices, `packet-parser` and the management UI **TLV decode** panel are the main tools:

```
cargo run -p rethink-tools --bin packet-parser -- localhost:1884 <device-uuid>
cargo run -p rethink-tools --bin packet-parser -- -message <hex>
```

Also `POST /api/decode` on the management port for catalog-aware decode + LLM export.

## Find the closest existing device and use it as a skeleton

Look under [`crates/rethink-devices/src/devices/`](../../crates/rethink-devices/src/devices/). Pick the most similar handler and copy it. Shared helpers live in `fridge_common.rs`, `washer_common.rs`, `ac_tables.rs`.

Good reference points:

- TLV air conditioner: `rac_056905_ww.rs`
- AABB fridge: `dev_2res1ve600fwc.rs` / `dev_2ref12eii_p_2.rs`
- AABB washer/dryer: `f_v8_y___w_b_2qeuk.rs`
- ThinQ1 washer: `wtdn3.rs`

Register the modelId with **one line** in the `register_devices!` table in [`devices/mod.rs`](../../crates/rethink-devices/src/devices/mod.rs). Aliases can share one module (`module => { "ID" | "ALIAS" }`). Put tests in a `#[cfg(test)]` module in the same device file.

### TLV devices

`TlvDeviceCore` + `FieldDefinition` map TLV type IDs to Home Assistant properties with read/write transforms; the core handles capability queries and framing.

### AABB devices

`AabbDeviceCore` unwraps the envelope; your handler implements status parse + `set_property` command bodies. Many AABB devices use a status block with 0xFF = unchanged on writes and prev/cur pairs on notifications.
## Map state updates (device → cloud)

With the device bridged and the parser running, **toggle settings directly on the appliance** (buttons, dials, doors) and watch which fields change. This tells you how to *read* the device's state.

- For TLV devices, note which `t=` IDs change and how their values map to the physical state.
- For AABB devices, diff the raw packets before and after each change to locate the relevant byte(s).

Encode each finding as a property your device class publishes to Home Assistant.

## Map commands (cloud → device)

Now do the reverse: **change settings in the official LG app** and capture the commands the cloud sends down to the device. This tells you how to *write* state.

Match each app action to the resulting packet, then implement `set_property()` (or the equivalent TLV write transform) so that Home Assistant commands produce the same bytes. Most devices also need an initial "query" packet before they report anything — e.g. the fridges send `F0ED…` on startup (see the device classes' `start()` method).

## Cross-check against the LG cloud's own parsing

For the trickiest fields, it helps to see how LG's cloud *interprets* a value, not just how the app displays it. Two complementary techniques:

- **Observe** — enable **bridge mode** on the device in the management UI, or validate cloud credentials with `cargo run -p rethink-tools --bin lgcloud-monitor -- --state ./state`. Live LG-cloud MQTT interpretation is available while bridged.
- **Inject** — use the management panel's packet monitor (or `packet-sender` / MCP `inject`) to send *modified* packets while bridged, then watch how the cloud reacts.

```
cargo run -p rethink-tools --bin packet-sender -- localhost:1884 <device-uuid> 1 1 2 2 1 501 1
```

## Add a test and submit a PR

- Add a unit test next to the Rust handler under `crates/rethink-devices/src/devices/`, using `MockHaConnection` + `MockThinq2Device`. Run `cargo test --workspace`.
- Open a pull request — contributions are welcome!

# LLM-assisted workflow

Much of the manual workflow - watching traffic, correlating device packets with the cloud's interpretation, sweeping field values, drafting the device class - is mostly pattern-matching over packet captures, which an LLM coding agent can do well. The repository ships an [MCP](https://modelcontextprotocol.io) server binary **`rethink-mcp`** (`crates/rethink-tools`) that exposes the reverse-engineering toolkit over management HTTP/WS + pure codecs. The [Guidelines](#guidelines) above apply just as much here - in particular, you must understand and be able to justify any generated code.

## Setup

The same [Setup](#setup) prerequisites apply: the device must be provisioned and bridged. Log into LG via the management UI (Bridge → Log into LG account); credentials land in the bridge `storage_path` (`oauth2.json`).

Register the server with your agent. A checked-in [`.mcp.json`](../../.mcp.json) declares:

```json
{
  "mcpServers": {
    "rethink-agent": {
      "command": "cargo",
      "args": ["run", "-q", "-p", "rethink-tools", "--bin", "rethink-mcp"],
      "env": { "RETHINK_MGMT": "localhost:44401" }
    }
  }
}
```

Set `RETHINK_MGMT` to your rethink-cloud management host[:port] if it isn't `localhost:44401` (or call `set_mgmt_host` at runtime).

## The toolkit

- **`list_devices` / `health`** — management HTTP inventory.
- **`set_mgmt_host`** — point tools at your management host for the session.
- **`decode_packet` / `encode_packet`** — pure `rethink-util` framing (optional `via_mgmt` uses `POST /api/decode` for catalog + unknowns).
- **`inject`** — management `/device` WebSocket injection (gated with `inject_ok`).
- **`read_capture`** — page a JSONL file from `rethink-capture`.

Record a session:

```
cargo run -p rethink-tools --bin rethink-capture -- <mgmt-host[:port]> <device-uuid> capture.jsonl
```

Type notes on stdin while operating the appliance. Offline analysis: MCP `read_capture` or any JSONL viewer.

## A typical session

1. `list_devices` to find the device UUID (and confirm it's bridged).
2. Capture with `rethink-capture` or the management monitor UI.
3. Operate the appliance / LG app; decode with management RE tools or `decode_packet`.
4. Draft a Rust handler under `crates/rethink-devices`, add fixture tests, run `cargo test --workspace`.

## Caveats

- The cloud feed is the key enabler: it provides labelled ground truth, which turns field inference into a supervised problem — and it's the only generic decoder available for [AABB](AABBProtocol.md) devices.
- Correlation is a coarse time-window match, so a quiet account helps; concurrent activity on other devices can leak into the window.
- The agent *proposes*; you review. Entity modelling — sensible names, device classes, value ranges, which fields are even worth exposing — is a judgement call best left to a human, and is bound by the [Guidelines](#guidelines).
- Safety: only `toDevice` injection actuates hardware and it's gated; `probe` only injects `fromDevice`. Still, you are talking to a real appliance through the real cloud — supervise it.
