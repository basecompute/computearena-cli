use crate::config::HTTP_CONNECT_TIMEOUT;
use anyhow::{Context, Result};
use reqwest::blocking::Client;
use serde_json::Value;
use std::time::Duration;

pub(crate) fn client(request_timeout: Duration) -> Result<Client> {
    Client::builder()
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .timeout(request_timeout)
        .user_agent(format!("computearena/{}", env!("CARGO_PKG_VERSION")))
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
