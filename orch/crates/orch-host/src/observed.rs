//! Provider-qualified model identity evidence.
//!
//! A requested model, a REPORT self-declaration, and a provider observation
//! are three different facts.  This module owns the third fact and the strict
//! comparison between it and an explicit declaration.  In particular, the
//! provider component is never discarded during strict reconciliation.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;

/// A model identity as supplied by one provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelIdentity {
    pub provider: Option<String>,
    pub id: String,
}

impl ModelIdentity {
    /// Parse either `provider/model-id` or an unqualified `model-id`.
    ///
    /// Only outer whitespace is removed.  Case and every provider/id byte are
    /// otherwise retained because strict reconciliation must not inherit the
    /// lossy, presentation-only normalization used for REPORT prose.
    pub fn parse(raw: &str) -> Self {
        let raw = raw.trim();
        match raw.split_once('/') {
            Some((provider, id)) if !provider.is_empty() && !id.is_empty() => Self {
                provider: Some(provider.to_string()),
                id: id.to_string(),
            },
            _ => Self {
                provider: None,
                id: raw.to_string(),
            },
        }
    }

    pub fn qualified(&self) -> String {
        match self.provider.as_deref() {
            Some(provider) => format!("{provider}/{}", self.id),
            None => self.id.clone(),
        }
    }
}

/// Observation is intentionally three-state.  An absent or broken evidence
/// source is not evidence that the requested model matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservedEvidence {
    Found(ModelIdentity),
    Missing,
    Unreadable(String),
}

/// Result of comparing a declaration with provider evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservedOutcome {
    Match,
    Mismatch {
        declared: ModelIdentity,
        observed: ModelIdentity,
    },
    Missing,
    Unreadable {
        reason: String,
    },
}

/// Strictly reconcile provider-qualified identities.
///
/// `accepts` is the caller's explicit, per-agent alias list.  No implicit
/// provider stripping or case folding is performed here.
pub fn reconcile_identity(
    declared: &ModelIdentity,
    accepts: &[ModelIdentity],
    evidence: &ObservedEvidence,
) -> ObservedOutcome {
    match evidence {
        ObservedEvidence::Found(observed)
            if observed == declared || accepts.iter().any(|alias| alias == observed) =>
        {
            ObservedOutcome::Match
        }
        ObservedEvidence::Found(observed) => ObservedOutcome::Mismatch {
            declared: declared.clone(),
            observed: observed.clone(),
        },
        ObservedEvidence::Missing => ObservedOutcome::Missing,
        ObservedEvidence::Unreadable(reason) => ObservedOutcome::Unreadable {
            reason: reason.clone(),
        },
    }
}

/// Extract the provider's actual model from pi `--mode json` NDJSON.
///
/// pi also emits `message.model`, but that field merely echoes the request.
/// Only `message.responseModel` is accepted as observed evidence.  Non-JSON
/// wrapper diagnostics are ignored; malformed/conflicting responseModel facts
/// fail closed as unreadable evidence.
pub fn extract_pi_response_model(ndjson: &str) -> ObservedEvidence {
    let mut found: Option<ModelIdentity> = None;
    for line in ndjson
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            // wake-pi also projects exact plain-text lifecycle markers.
            continue;
        };
        if !matches!(
            value.get("type").and_then(serde_json::Value::as_str),
            Some("message_end" | "turn_end")
        ) {
            continue;
        }
        let Some(message) = value.get("message").and_then(serde_json::Value::as_object) else {
            continue;
        };
        if message.get("role").and_then(serde_json::Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(raw_response) = message.get("responseModel") else {
            continue;
        };
        let Some(response) = raw_response
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return ObservedEvidence::Unreadable(
                "pi responseModel must be a non-empty string".to_string(),
            );
        };
        let mut identity = ModelIdentity::parse(response);
        if identity.provider.is_none() {
            identity.provider = message
                .get("provider")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|provider| !provider.is_empty())
                .map(str::to_string);
        }
        if identity.provider.is_none() {
            return ObservedEvidence::Unreadable(
                "pi responseModel lacks a provider identity".to_string(),
            );
        }
        if let Some(previous) = found.as_ref() {
            if previous != &identity {
                return ObservedEvidence::Unreadable(format!(
                    "pi responseModel changed within one evidence stream: {} -> {}",
                    previous.qualified(),
                    identity.qualified()
                ));
            }
        } else {
            found = Some(identity);
        }
    }

    found
        .map(ObservedEvidence::Found)
        .unwrap_or(ObservedEvidence::Missing)
}

#[derive(Debug, Deserialize)]
struct OpenCodeIdentityRow {
    provider: Option<String>,
    model_id: String,
}

fn sqlite_read_only_uri(path: &Path) -> Result<String, String> {
    let raw = path
        .to_str()
        .ok_or_else(|| format!("opencode db path is not UTF-8: {}", path.display()))?;
    let mut encoded = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    Ok(format!("file:{encoded}?mode=ro"))
}

fn sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Read the newest assistant model identity for one OpenCode session and one
/// bounded millisecond window via the `sqlite3` CLI.
///
/// Both `-readonly` and a `file:...?mode=ro` URI are used.  The fixed query is
/// constrained by `session_id` and `time_created` before ordering, so a shared
/// multi-gigabyte database is neither treated as a global latest-message log
/// nor opened for mutation.
pub fn read_opencode_identity(
    db: &Path,
    session_id: &str,
    from_ms: u64,
    to_ms: u64,
) -> ObservedEvidence {
    if session_id.trim().is_empty() {
        return ObservedEvidence::Unreadable("opencode session id is empty".to_string());
    }
    if from_ms > to_ms {
        return ObservedEvidence::Unreadable(format!(
            "opencode observation window is reversed: {from_ms} > {to_ms}"
        ));
    }
    let uri = match sqlite_read_only_uri(db) {
        Ok(uri) => uri,
        Err(reason) => return ObservedEvidence::Unreadable(reason),
    };
    let query = format!(
        "SELECT json_extract(data, '$.providerID') AS provider, \
                json_extract(data, '$.modelID') AS model_id \
         FROM message \
         WHERE session_id = {} \
           AND time_created >= {} AND time_created <= {} \
           AND json_valid(data) \
           AND json_extract(data, '$.role') = 'assistant' \
           AND typeof(json_extract(data, '$.modelID')) = 'text' \
           AND trim(json_extract(data, '$.modelID')) <> '' \
         ORDER BY time_created DESC, message.id DESC LIMIT 1;",
        sql_string(session_id),
        from_ms,
        to_ms,
    );
    let output = match Command::new("sqlite3")
        .arg("-batch")
        .arg("-bail")
        .arg("-readonly")
        .arg("-json")
        .arg(uri)
        .arg(query)
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            return ObservedEvidence::Unreadable(format!("cannot launch sqlite3: {error}"));
        }
    };
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return ObservedEvidence::Unreadable(if detail.is_empty() {
            format!("sqlite3 exited with {}", output.status)
        } else {
            format!("sqlite3 read-only lookup failed: {detail}")
        });
    }
    let stdout = match std::str::from_utf8(&output.stdout) {
        Ok(value) => value.trim(),
        Err(error) => {
            return ObservedEvidence::Unreadable(format!(
                "sqlite3 returned non-UTF-8 JSON: {error}"
            ));
        }
    };
    if stdout.is_empty() || stdout == "[]" {
        return ObservedEvidence::Missing;
    }
    let rows: Vec<OpenCodeIdentityRow> = match serde_json::from_str(stdout) {
        Ok(rows) => rows,
        Err(error) => {
            return ObservedEvidence::Unreadable(format!(
                "sqlite3 returned invalid identity JSON: {error}"
            ));
        }
    };
    let Some(row) = rows.into_iter().next() else {
        return ObservedEvidence::Missing;
    };
    let id = row.model_id.trim();
    if id.is_empty() {
        return ObservedEvidence::Unreadable("opencode modelID is empty after lookup".to_string());
    }
    let Some(provider) = row
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return ObservedEvidence::Unreadable(
            "opencode providerID is missing after lookup".to_string(),
        );
    };
    let identity = ModelIdentity::parse(&format!("{provider}/{id}"));
    ObservedEvidence::Found(identity)
}

/// The production OpenCode database location, with an explicit environment
/// override for hermetic runtime/test deployments.
pub fn opencode_db_path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("ORCH_OPENCODE_DB") {
        if path.is_empty() {
            return Err("ORCH_OPENCODE_DB is empty".to_string());
        }
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| "HOME is unavailable".to_string())?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("share")
        .join("opencode")
        .join("opencode.db"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflicting_pi_response_models_are_unreadable() {
        let frames = r#"{"type":"message_end","message":{"role":"assistant","provider":"p","responseModel":"one"}}
{"type":"turn_end","message":{"role":"assistant","provider":"p","responseModel":"two"}}"#;
        assert!(matches!(
            extract_pi_response_model(frames),
            ObservedEvidence::Unreadable(_)
        ));
    }

    #[test]
    fn sqlite_uri_is_explicitly_read_only_and_percent_encoded() {
        let uri = sqlite_read_only_uri(Path::new("/tmp/a b#c.db")).unwrap();
        assert_eq!(uri, "file:/tmp/a%20b%23c.db?mode=ro");
    }

}
