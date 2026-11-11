//! Antigravity subscription routing uses the existing Google sign-in.
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::{ffi::OsString, path::PathBuf};

pub fn validate(args: &[OsString]) -> Result<()> {
    if std::env::var_os("AGY_LLM_GATEWAY_URL").is_some()
        || std::env::var_os("AGY_LLM_GATEWAY_API_KEY").is_some()
    {
        bail!(
            "agy enterprise gateway settings conflict with subscription routing; remove AGY_LLM_GATEWAY_URL/AGY_LLM_GATEWAY_API_KEY for this launch"
        );
    }
    let mut effective = serde_json::json!({});
    if let Some(home) = std::env::var_os("HOME") {
        let path = PathBuf::from(home).join(".gemini/antigravity-cli/settings.json");
        if path.exists() {
            effective = serde_json::from_slice(&std::fs::read(&path)?)?;
        }
    }
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        if arg == "--remote-agent"
            || arg
                .to_str()
                .is_some_and(|a| a.starts_with("--remote-agent="))
        {
            bail!(
                "remote agy agents execute outside this local relay; run agy locally to track subscription usage"
            );
        }
        let Some(arg) = arg.to_str() else {
            continue;
        };
        let settings = if arg == "--settings" {
            Some(
                args.next()
                    .and_then(|s| s.to_str())
                    .context("--settings requires JSON or a file path")?,
            )
        } else {
            arg.strip_prefix("--settings=")
        };
        if let Some(settings) = settings {
            let value: Value = if settings.trim_start().starts_with('{') {
                serde_json::from_str(settings)?
            } else {
                serde_json::from_slice(
                    &std::fs::read(settings).context("cannot read agy --settings file")?,
                )?
            };
            merge(&mut effective, value);
        }
    }
    check(&effective)
}

fn merge(target: &mut Value, patch: Value) {
    match (target, patch) {
        (Value::Object(target), Value::Object(patch)) => {
            for (key, value) in patch {
                merge(target.entry(key).or_insert(Value::Null), value);
            }
        }
        (target, patch) => *target = patch,
    }
}

fn check(settings: &Value) -> Result<()> {
    if settings
        .get("modelProvider")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty())
        || settings
            .pointer("/auth/type")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
    {
        bail!(
            "AGS agy requires Google subscription sign-in; remove modelProvider and explicit auth.type from agy settings for this launch"
        );
    }
    Ok(())
}
