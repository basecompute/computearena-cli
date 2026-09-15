use crate::config::{COMPUTEARENA_INSTALL_SCRIPT, HTTP_CONNECT_TIMEOUT};
use anyhow::{Context, Result};
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::Value;
use std::time::Duration;

pub(crate) const CLIENT_VERSION_HEADER: &str = "x-computearena-client-version";

pub(crate) fn client(request_timeout: Duration) -> Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(
        CLIENT_VERSION_HEADER,
        HeaderValue::from_static(env!("CARGO_PKG_VERSION")),
    );
    Client::builder()
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .timeout(request_timeout)
        .user_agent(format!("computearena/{}", env!("CARGO_PKG_VERSION")))
        .default_headers(headers)
        .build()
        .context("building ComputeArena HTTP client")
}

pub(crate) fn error_message(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body)
        .ok()?
        .pointer("/error/message")?
        .as_str()
        .map(str::to_owned)
}

pub(crate) fn error_code(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body)
        .ok()?
        .pointer("/error/code")?
        .as_str()
        .map(str::to_owned)
}

/// Turn a structured API failure into an actionable message. The server can
/// retire an old client by returning HTTP 426 (or the matching error code)
/// without making older clients fail with an unexplained generic status.
pub(crate) fn server_error(status: reqwest::StatusCode, body: &str) -> String {
    if status == reqwest::StatusCode::CONFLICT
        && error_code(body).as_deref() == Some("submission_deleted")
    {
        return "This benchmark was previously deleted from ComputeArena. This saved report cannot be submitted again, including through Select all. Run a new benchmark to publish new results. Your local report is unchanged.".to_string();
    }
    let message =
        error_message(body).unwrap_or_else(|| format!("server returned HTTP {}", status.as_u16()));
    let upgrade_required = status == reqwest::StatusCode::UPGRADE_REQUIRED
        || matches!(
            error_code(body).as_deref(),
            Some("client_upgrade_required" | "unsupported_client_version")
        );
    if !upgrade_required {
        return message;
    }
    let value = serde_json::from_str::<Value>(body).ok();
    let minimum = value.as_ref().and_then(|value| {
        [
            "/error/details/minimumClientVersion",
            "/error/minimumClientVersion",
            "/minimumClientVersion",
        ]
        .into_iter()
        .find_map(|pointer| value.pointer(pointer).and_then(Value::as_str))
    });
    let requirement = minimum
        .map(|version| format!(" The server requires {version} or newer."))
        .unwrap_or_default();
    format!(
        "{message}\nInstalled ComputeArena CLI: {}.{requirement}\nUpdate with: {COMPUTEARENA_INSTALL_SCRIPT}",
        env!("CARGO_PKG_VERSION")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrade_errors_include_the_installed_and_required_versions() {
        let body = r#"{"error":{"code":"client_upgrade_required","message":"This client is no longer supported","details":{"minimumClientVersion":"0.2.0"}}}"#;
        let message = server_error(reqwest::StatusCode::UPGRADE_REQUIRED, body);
        assert!(message.contains("This client is no longer supported"));
        assert!(message.contains(env!("CARGO_PKG_VERSION")));
        assert!(message.contains("0.2.0 or newer"));
        assert!(message.contains("computearena.ai/install.sh"));
    }

    #[test]
    fn ordinary_api_errors_are_unchanged() {
        let body = r#"{"error":{"code":"bad_request","message":"Invalid report"}}"#;
        assert_eq!(
            server_error(reqwest::StatusCode::BAD_REQUEST, body),
            "Invalid report"
        );
    }

    #[test]
    fn deleted_reports_explain_why_select_all_cannot_restore_them() {
        for body in [
            r#"{"error":{"code":"submission_deleted"}}"#,
            r#"{"error":{"code":"submission_deleted","message":"Report deleted"}}"#,
        ] {
            let message = server_error(reqwest::StatusCode::CONFLICT, body);
            assert!(message.contains("previously deleted"));
            assert!(message.contains("Select all"));
            assert!(message.contains("Run a new benchmark"));
            assert!(message.contains("local report is unchanged"));
        }
        let body = r#"{"error":{"code":"submission_conflict","message":"Different report"}}"#;
        assert_eq!(
            server_error(reqwest::StatusCode::CONFLICT, body),
            "Different report"
        );
    }
}
