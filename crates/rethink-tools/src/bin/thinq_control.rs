//! Drive a ThinQ2 device through the official LG cloud, the way the LG app does, so that the
//! cloud's translation of named commands (e.g. `airState.*`) into device wire values can be
//! observed with the management monitor while the device is bridged. Port of upstream
//! `tools/thinq-control.ts`.
//!
//! Usage:
//!   thinq-control [--state DIR] status <deviceId>
//!   thinq-control [--state DIR] set <deviceId> <ctrlKey> <dataKey> <value> [<dataKey> <value> ...]
//!
//! Uses the oauth2.json login from bridge storage (management UI login), like lgcloud-monitor.
//! Diagnostics go to stderr; stdout carries the JSON result only.

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use rethink_bridge::state::{BridgeState, Environment, JsonStorage};
use rethink_bridge::thinq_api::Client;
use serde::Serialize;
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "thinq-control")]
struct Args {
    /// Directory containing oauth2.json (rethink-cloud bridge storage_path)
    #[arg(long, default_value = "./state")]
    state: PathBuf,
    /// `status <deviceId>` or `set <deviceId> <ctrlKey> <dataKey> <value> [<dataKey> <value> ...]`
    #[arg(required = true, num_args = 2.., allow_hyphen_values = true)]
    command: Vec<String>,
}

/// Numeric-looking values (`^-?\d+(\.\d+)?$`) go out as JSON numbers, like upstream's `Number()`.
fn data_value(raw: &str) -> Value {
    let digits = raw.strip_prefix('-').unwrap_or(raw);
    let (int, frac) = digits.split_once('.').unwrap_or((digits, "0"));
    let numeric = !int.is_empty()
        && !frac.is_empty()
        && int.bytes().all(|b| b.is_ascii_digit())
        && frac.bytes().all(|b| b.is_ascii_digit());
    match raw.parse::<f64>() {
        Ok(n) if numeric && n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 => {
            json!(n as i64)
        }
        Ok(n) if numeric => json!(n),
        _ => json!(raw),
    }
}

/// The control-sync body for `set`: one key uses dataKey/dataValue, several use the list forms.
fn control_body(ctrl_key: &str, pairs: &[String]) -> Result<Value> {
    if ctrl_key.is_empty() || pairs.len() < 2 || pairs.len() % 2 != 0 {
        bail!("set needs ctrlKey and key/value pairs");
    }
    let keys: Vec<&str> = pairs.iter().step_by(2).map(String::as_str).collect();
    let values: Vec<Value> = pairs.iter().skip(1).step_by(2).map(|v| data_value(v)).collect();
    Ok(if keys.len() == 1 {
        json!({"ctrlKey": ctrl_key, "command": "Set", "dataKey": keys[0], "dataValue": values[0]})
    } else {
        json!({"ctrlKey": ctrl_key, "command": "Set", "dataKeyList": keys, "dataValueList": values})
    })
}

/// `JSON.stringify(value, null, 1)`.
fn to_json_indent_1(value: &Value) -> Result<String> {
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
    value.serialize(&mut serde_json::Serializer::with_formatter(&mut out, formatter))?;
    Ok(String::from_utf8(out)?)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let (cmd, device_id, rest) = (&args.command[0], &args.command[1], &args.command[2..]);
    let storage = JsonStorage::new(&args.state);
    let creds = storage.get_credentials().ok_or_else(|| {
        anyhow!(
            "not logged in: no oauth2.json in {}. Log in via the rethink management UI \
             (Bridge → Log into LG account), then pass that storage path with --state.",
            args.state.display()
        )
    })?;
    let mut client = Client::new(Environment {
        country_code: creds.env.country_code.clone(),
        language_code: creds.env.language_code.clone(),
    });
    eprintln!("[thinq-control] country={} — authenticating…", creds.env.country_code);
    client
        .auth(&creds.refresh_token)
        .await
        .context("ThinQ auth (refresh token may be expired — re-login via management UI)")?;

    match cmd.as_str() {
        "status" => {
            let out = client.device_status(device_id).await?;
            println!("{}", to_json_indent_1(&out)?);
        }
        "set" => {
            let (ctrl_key, pairs) = rest
                .split_first()
                .ok_or_else(|| anyhow!("set needs ctrlKey and key/value pairs"))?;
            let body = control_body(ctrl_key, pairs)?;
            let result = client.control_sync(device_id, body.clone()).await?;
            println!("{}", json!({"sent": body, "result": result}));
        }
        other => bail!("unknown command {other}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn numeric_looking_values_become_json_numbers() {
        assert_eq!(data_value("1"), json!(1));
        assert_eq!(data_value("-3"), json!(-3));
        assert_eq!(data_value("5.0"), json!(5));
        assert_eq!(data_value("21.5"), json!(21.5));
        for text in ["@AC_MAIN_OPERATION_ALL_ON_W", "1e3", "0x10", "+1", ".5", "5.", ""] {
            assert_eq!(data_value(text), json!(text), "{text:?}");
        }
    }

    #[test]
    fn one_key_uses_data_key_and_several_use_the_lists() {
        assert_eq!(
            control_body("basicCtrl", &strings(&["airState.operation", "1"])).unwrap(),
            json!({"ctrlKey": "basicCtrl", "command": "Set", "dataKey": "airState.operation", "dataValue": 1})
        );
        assert_eq!(
            control_body(
                "basicCtrl",
                &strings(&["airState.opMode", "0", "airState.tempState.target", "22.5"])
            )
            .unwrap(),
            json!({
                "ctrlKey": "basicCtrl", "command": "Set",
                "dataKeyList": ["airState.opMode", "airState.tempState.target"],
                "dataValueList": [0, 22.5],
            })
        );
    }

    #[test]
    fn set_needs_a_ctrl_key_and_whole_pairs() {
        for pairs in [&[][..], &["airState.operation"][..], &["a", "1", "b"][..]] {
            assert!(control_body("basicCtrl", &strings(pairs)).is_err());
        }
        assert!(control_body("", &strings(&["a", "1"])).is_err());
    }

    #[test]
    fn status_output_is_indented_like_json_stringify_with_one_space() {
        assert_eq!(
            to_json_indent_1(&json!({"a": [1, 2]})).unwrap(),
            "{\n \"a\": [\n  1,\n  2\n ]\n}"
        );
    }
}
