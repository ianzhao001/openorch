use std::fs;
use std::path::Path;

use serde_json::Value;

fn numeric_round(name: &str) -> Option<u64> {
    name.strip_prefix('r')?.parse().ok()
}

fn validated_and_closed(events: &str, path: &Path) -> Result<(bool, bool), String> {
    let mut validated = false;
    let mut closed = false;

    for (index, line) in events
        .lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
    {
        let event: Value = serde_json::from_str(line).map_err(|error| {
            format!(
                "解析 materialized ledger {} 第 {} 行失败: {error}",
                path.display(),
                index + 1
            )
        })?;
        match event.get("type").and_then(Value::as_str) {
            Some("TaskValidated") => {
                let digest = event
                    .pointer("/payload/validationDigest")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                validated |= event.get("actor").and_then(Value::as_str) == Some("runtime:orch")
                    && digest.len() == 64
                    && digest.bytes().all(|byte| byte.is_ascii_hexdigit());
            }
            Some("RoundClosed") => closed = true,
            _ => {}
        }
    }

    Ok((validated, closed))
}

fn reopen_round(events_path: &Path, events: &str) -> Result<(), String> {
    let mut kept = Vec::new();
    for (index, line) in events
        .lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
    {
        let event: Value = serde_json::from_str(line).map_err(|error| {
            format!(
                "过滤 materialized ledger {} 第 {} 行失败: {error}",
                events_path.display(),
                index + 1
            )
        })?;
        if event.get("type").and_then(Value::as_str) != Some("RoundClosed") {
            kept.push(line);
        }
    }
    let reopened = if kept.is_empty() {
        String::new()
    } else {
        format!("{}\n", kept.join("\n"))
    };
    fs::write(events_path, reopened)
        .map_err(|error| format!("重开 scratch round {} 失败: {error}", events_path.display()))
}

pub fn prepare_local_valid_materialized_round(root: &Path) -> Result<String, String> {
    let rounds_root = root.join("coordination/rounds");
    let entries = fs::read_dir(&rounds_root).map_err(|error| {
        format!(
            "读取调用方 materialized rounds {} 失败: {error}",
            rounds_root.display()
        )
    })?;
    let mut candidates = Vec::new();

    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "遍历调用方 materialized rounds {} 失败: {error}",
                rounds_root.display()
            )
        })?;
        if !entry
            .file_type()
            .map_err(|error| format!("读取 round 类型失败: {error}"))?
            .is_dir()
        {
            continue;
        }
        let Some(round) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(number) = numeric_round(&round) else {
            continue;
        };
        let round_root = entry.path();
        if !round_root.join("ROUND-IR.yaml").is_file() {
            continue;
        }
        let events_path = round_root.join("events.jsonl");
        let Ok(events) = fs::read_to_string(&events_path) else {
            continue;
        };
        let (validated, closed) = validated_and_closed(&events, &events_path)?;
        if validated {
            candidates.push((number, round, events_path, events, closed));
        }
    }

    candidates.sort_by_key(|candidate| candidate.0);
    let (_, round, events_path, events, closed) = candidates.pop().ok_or_else(|| {
        format!(
            "调用方 materialized tree {} 中找不到带 ROUND-IR 与 runtime TaskValidated 的轮模板",
            root.display()
        )
    })?;

    if closed {
        reopen_round(&events_path, &events)?;
    }

    let runtime = root.join("coordination/runtime");
    fs::create_dir_all(&runtime)
        .map_err(|error| format!("创建 scratch runtime {} 失败: {error}", runtime.display()))?;
    let current_round = runtime.join("CURRENT-ROUND");
    fs::write(&current_round, format!("{round}\n")).map_err(|error| {
        format!(
            "写 scratch CURRENT-ROUND {} 失败: {error}",
            current_round.display()
        )
    })?;

    Ok(round)
}
