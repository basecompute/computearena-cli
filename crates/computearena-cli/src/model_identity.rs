//! Upstream identity is distinct from a converted package or its quantization.
use anyhow::{bail, Result};
use serde_json::{json, Value};

struct Entry {
    id: String,
    names: Vec<String>,
}

pub(crate) fn validate(id: &str) -> Result<()> {
    let parts: Vec<_> = id.split('/').collect();
    if parts.len() != 2
        || parts
            .iter()
            .any(|p| p.is_empty() || *p == "." || *p == "..")
        || id.len() > 256
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./".contains(&b))
    {
        bail!("model ID must be an upstream repository ID such as Qwen/Qwen3-0.6B");
    }
    Ok(())
}
pub(crate) fn canonical(value: &str) -> Option<String> {
    let values: Vec<Value> =
        serde_json::from_str(include_str!("model-identities.json")).expect("model catalogue");
    let entries = values.into_iter().map(|v| Entry {
        id: v["id"].as_str().expect("catalogue ID").to_string(),
        names: v["names"]
            .as_array()
            .expect("catalogue aliases")
            .iter()
            .map(|n| n.as_str().expect("catalogue alias").to_string())
            .collect(),
    });
    let value = value.trim();
    for e in entries {
        if value.eq_ignore_ascii_case(&e.id) {
            return Some(e.id);
        }
        let owner = e.id.split('/').next().unwrap();
        if e.names.iter().any(|name| {
            [
                name.clone(),
                format!("{owner}/{name}"),
                format!("basecompute/{name}"),
            ]
            .iter()
            .any(|alias| value.eq_ignore_ascii_case(alias))
        }) {
            return Some(e.id);
        }
    }
    None
}
pub(crate) fn record(model: &mut Value, declared: Option<&str>) -> Result<()> {
    let candidate = model["id"]
        .as_str()
        .or_else(|| model["name"].as_str())
        .unwrap_or("");
    let (id, source) = if let Some(id) = declared {
        validate(id)?;
        (Some(id.to_string()), "user_declared")
    } else {
        (canonical(candidate), "known_alias_catalogue_v1")
    };
    model["upstream_id"] = json!(id);
    model["upstream_id_source"] = json!(if id.is_some() { source } else { "unresolved" });
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn packaging_aliases_do_not_erase_model_variants() {
        assert_eq!(
            canonical("Qwen3-0.6B-localq8").as_deref(),
            Some("Qwen/Qwen3-0.6B")
        );
        assert_ne!(
            canonical("Llama-3.2-1B"),
            canonical("Llama-3.2-1B-Instruct")
        );
        assert_eq!(canonical("Qwen3-0.6B-Instruct"), None);
        assert_eq!(canonical("Qwen3-0.6B-MoE"), None);
        assert_eq!(canonical("someone/Qwen3-0.6B"), None);
        assert_eq!(canonical("Qwen3-0.6B-finetune"), None);
    }
    #[test]
    fn declared_identity_is_recorded_without_rewriting_package_fields() {
        let mut m = json!({"name":"local-file","quantization":"Q4_0"});
        record(&mut m, Some("someone/My-Instruct-MoE")).unwrap();
        assert_eq!(m["name"], "local-file");
        assert_eq!(m["upstream_id"], "someone/My-Instruct-MoE");
        assert_eq!(m["upstream_id_source"], "user_declared");
        assert!(validate("/tmp/model.base").is_err());
    }
}
