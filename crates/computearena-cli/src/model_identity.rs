//! Upstream identity is distinct from a converted package or its quantization.
use anyhow::{bail, Result};
use serde_json::{json, Value};

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
pub(crate) fn record(model: &mut Value, declared: Option<&str>) -> Result<()> {
    let (id, source) = if let Some(id) = declared {
        validate(id)?;
        (Some(id.to_string()), "user_declared")
    } else {
        (None, "unresolved")
    };
    model["identity_verification"] = json!("unverified");
    model["upstream_id"] = json!(id);
    model["upstream_id_source"] = json!(if id.is_some() { source } else { "unresolved" });
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filenames_never_establish_upstream_identity() {
        for name in [
            "Qwen/Qwen3-0.6B",
            "Qwen3-0.6B-localq8",
            "Llama-3.2-1B-Instruct",
        ] {
            let mut model = json!({"name": name, "id": name});
            record(&mut model, None).unwrap();
            assert!(model["upstream_id"].is_null());
            assert_eq!(model["upstream_id_source"], "unresolved");
            assert_eq!(model["identity_verification"], "unverified");
        }
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
