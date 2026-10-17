//! Minimal MCP (Model Context Protocol) server over stdio.
//!
//! Every tool shells out to this same executable so that interactive prompts
//! fail fast (stdin is null) and a panic in signing can't take the server down.
//! Logging in stays a manual step (`plumesign account login`) since it needs a
//! password and a 2FA code; the saved session is then reused for every install.

use std::ffi::OsString;
use std::io::{BufRead, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

const PROTOCOL_VERSION: &str = "2025-06-18";
// apple_codesign warns once per nested bundle and plume_utils logs every bundle it signs,
// which floods a tool result with hundreds of lines; failures still surface as the error.
const CHILD_LOG_FILTER: &str = "warn,apple_codesign=error,plumesign=info,plume_core=info";

const INSTRUCTIONS: &str = "Sideloads .ipa files onto the user's iPhone/iPad with the Apple ID \
saved by `plumesign account login` (or the Impactor app), so no login is needed per install. \
Call list_devices first when the target device is unknown. With a free Apple ID, apps expire \
after 7 days, at most 3 sideloaded apps can be installed at once, and only 10 new app IDs can \
be registered per 7 days. Never ask the user for their Apple ID password; if no account is \
saved, tell them to run `plumesign account login` in a terminal.";

pub fn serve() -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(e) => {
                let error = json!({"code": -32700, "message": e.to_string()});
                write_message(&mut stdout, &json!({"jsonrpc": "2.0", "id": null, "error": error}))?;
                continue;
            }
        };

        // Notifications carry no id and responses carry no method; neither needs a reply.
        let (Some(id), Some(method)) = (message.get("id"), message["method"].as_str()) else {
            continue;
        };

        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let response = match handle(method, &params) {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": code, "message": message},
            }),
        };

        write_message(&mut stdout, &response)?;
    }

    Ok(())
}

fn write_message(stdout: &mut std::io::Stdout, message: &Value) -> Result<()> {
    let mut lock = stdout.lock();
    serde_json::to_writer(&mut lock, message)?;
    lock.write_all(b"\n")?;
    lock.flush()?;
    Ok(())
}

fn handle(method: &str, params: &Value) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or(PROTOCOL_VERSION),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "plumesign", "version": env!("CARGO_PKG_VERSION")},
            "instructions": INSTRUCTIONS,
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or_default();
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
            let (text, is_error) = match call_tool(name, &arguments) {
                Ok(text) => (text, false),
                Err(e) => (format!("{e:#}"), true),
            };
            Ok(json!({"content": [{"type": "text", "text": text}], "isError": is_error}))
        }
        _ => Err((-32601, format!("Method not found: {method}"))),
    }
}

fn tools() -> Value {
    json!([
        {
            "name": "list_devices",
            "description": "List iOS devices visible over USB or Wi-Fi (through Apple Mobile Device Service from iTunes). Returns name, UDID and connection type.",
            "inputSchema": {"type": "object", "properties": {}},
        },
        {
            "name": "account_status",
            "description": "Show the Apple IDs saved by `plumesign account login` or the Impactor app, and which one is selected for signing.",
            "inputSchema": {"type": "object", "properties": {}},
        },
        {
            "name": "install_ipa",
            "description": "Sign an .ipa with the saved Apple ID (registering the device and app ID as needed) and install it on a device. Can take a few minutes for large apps.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Absolute path to the .ipa file (or an .app folder) on this computer."},
                    "udid": {"type": "string", "description": "Target device UDID from list_devices. Optional when exactly one device is connected."},
                    "sign": {"type": "boolean", "description": "Re-sign with the saved Apple ID before installing (default true). Set false only for an .ipa that is already signed for this device.", "default": true},
                    "bundle_id": {"type": "string", "description": "Override the bundle identifier."},
                    "name": {"type": "string", "description": "Override the app's display name."},
                    "team_id": {"type": "string", "description": "Developer team ID, only needed when the account belongs to several teams."},
                },
                "required": ["path"],
            },
        },
    ])
}

fn call_tool(name: &str, arguments: &Value) -> Result<String> {
    match name {
        "list_devices" => {
            let devices = connected_devices()?;
            Ok(serde_json::to_string_pretty(&devices)?)
        }
        "account_status" => run(["account", "list"]),
        "install_ipa" => install_ipa(arguments),
        _ => bail!("Unknown tool: {name}"),
    }
}

fn install_ipa(arguments: &Value) -> Result<String> {
    let path = arguments["path"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("`path` is required")?;
    let path = Path::new(path);
    ensure!(path.exists(), "File not found: {}", path.display());

    let udid = match string_arg(arguments, "udid") {
        Some(udid) => udid.to_string(),
        None => only_device_udid()?,
    };

    let mut args: Vec<OsString> = Vec::new();

    if arguments["sign"].as_bool().unwrap_or(true) {
        args.extend(["sign", "--apple-id", "--register-and-install", "--udid"].map(OsString::from));
        args.push(udid.into());
        args.push("--package".into());
        args.push(path.into());

        for (key, flag) in [
            ("team_id", "--team"),
            ("bundle_id", "--custom-identifier"),
            ("name", "--custom-name"),
        ] {
            if let Some(value) = string_arg(arguments, key) {
                args.push(flag.into());
                args.push(value.into());
            }
        }
    } else {
        args.extend(["device", "--udid"].map(OsString::from));
        args.push(udid.into());
        args.push("--install".into());
        args.push(path.into());
    }

    let output = run(args)?;

    // Drop the per-percent progress lines; the outcome lines are what matter.
    Ok(output
        .lines()
        .filter(|line| !line.contains("Installation progress"))
        .collect::<Vec<_>>()
        .join("\n"))
}

fn string_arg<'a>(arguments: &'a Value, key: &str) -> Option<&'a str> {
    arguments[key].as_str().filter(|s| !s.is_empty())
}

fn connected_devices() -> Result<Vec<Value>> {
    let output = spawn(["device", "--list"])?;
    if !output.status.success() {
        bail!("Failed to list devices:\n{}", combined_output(&output));
    }

    serde_json::from_slice(&output.stdout).context("Unexpected `device --list` output")
}

fn only_device_udid() -> Result<String> {
    let mut udids: Vec<String> = connected_devices()?
        .iter()
        .filter_map(|d| d["udid"].as_str().map(str::to_string))
        .collect();
    udids.sort();
    udids.dedup();

    match udids.as_slice() {
        [udid] => Ok(udid.clone()),
        [] => bail!(
            "No iOS device connected. Plug the device in over USB (and trust this computer), \
             or enable Wi-Fi sync in iTunes."
        ),
        _ => bail!(
            "Several devices are connected ({}); pass `udid`.",
            udids.join(", ")
        ),
    }
}

fn run<I, S>(args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let output = spawn(args)?;
    let text = combined_output(&output);

    if !output.status.success() {
        bail!("plumesign failed ({}):\n{}", output.status, text);
    }

    Ok(text)
}

fn spawn<I, S>(args: I) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let log_filter = std::env::var("PLUMESIGN_MCP_LOG").unwrap_or_else(|_| CHILD_LOG_FILTER.into());

    Command::new(std::env::current_exe()?)
        .args(args.into_iter().map(Into::into))
        .env("RUST_LOG", log_filter)
        .env("RUST_LOG_STYLE", "never")
        .stdin(Stdio::null())
        .output()
        .context("Failed to run plumesign")
}

fn combined_output(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    [stdout.trim(), stderr.trim()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}
