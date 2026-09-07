use crate::api::{
    client as api_client, error_code as api_error_code, error_message as api_error_message,
};
use crate::config::{
    AUTH_HTTP_TIMEOUT, DEFAULT_API_URL, DEFAULT_DEVICE_AUTH_EXPIRES_SECS,
    DEFAULT_DEVICE_AUTH_POLL_INTERVAL_SECS, MIN_DEVICE_AUTH_POLL_INTERVAL_SECS,
};
use crate::ui::{finish_activity, start_activity, TerminalUi};
use crate::{read_report, set_private_permissions, Paths};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command as ProcessCommand, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub(crate) struct ApiSession {
    pub(crate) access_token: String,
    pub(crate) username: String,
    pub(crate) expires_at: String,
}
pub(crate) fn resolve_api_url(override_url: Option<String>) -> Result<String> {
    let value = override_url
        .or_else(|| std::env::var("BASERT_COMPUTEARENA_API_URL").ok())
        .unwrap_or_else(|| DEFAULT_API_URL.to_string());
    let value = value.trim().trim_end_matches('/');
    if value.is_empty() {
        bail!("ComputeArena API URL cannot be empty");
    }
    let parsed = reqwest::Url::parse(value).context("invalid ComputeArena API URL")?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("ComputeArena API URL must use http or https");
    }
    Ok(value.to_string())
}

pub(crate) fn login(paths: &Paths, api_url: &str) -> Result<()> {
    let ui = TerminalUi::detect();
    if let Some(session) = load_api_session(paths, api_url)? {
        let started = start_activity(ui, format!("Checking the saved login with {api_url}…"));
        match validate_api_session(api_url, &session)? {
            Some(username) => {
                finish_activity(ui, started, "Saved login is valid");
                println!(
                    "Logged in to {api_url} as {}.",
                    ui.success(format!("@{username}"))
                );
                println!("Run `basert computearena logout` before switching accounts.");
                return Ok(());
            }
            None => {
                println!(
                    "{} The saved login is expired or revoked; starting a new login.",
                    ui.warning("!")
                );
                remove_api_session(paths, api_url)?;
            }
        }
    }

    let client = api_client(AUTH_HTTP_TIMEOUT)?;
    let started = start_activity(ui, format!("Requesting a login code from {api_url}…"));
    let response = client
        .post(format!("{api_url}/auth/device"))
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .context("requesting a ComputeArena login code")?;
    let status = response.status();
    let body = response.text().unwrap_or_default();
    if !status.is_success() {
        bail!(
            "{}",
            api_error_message(&body)
                .unwrap_or_else(|| format!("server returned HTTP {}", status.as_u16()))
        );
    }
    let device: Value = serde_json::from_str(&body).context("parsing login response")?;
    let device_code = required_json_string(&device, "deviceCode")?;
    let user_code = required_json_string(&device, "userCode")?;
    let verification_url = required_json_string(&device, "verificationUriComplete")?;
    let interval = device
        .get("interval")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_DEVICE_AUTH_POLL_INTERVAL_SECS)
        .max(MIN_DEVICE_AUTH_POLL_INTERVAL_SECS);
    let expires_in = device
        .get("expiresIn")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_DEVICE_AUTH_EXPIRES_SECS);
    finish_activity(ui, started, "Login code ready");
    println!("\n  Open: {}", ui.brand(&verification_url));
    println!("  Confirm code: {}", ui.brand_bold(&user_code));
    if open_browser(&verification_url) {
        println!("\nYour browser was opened. Approve the device there.");
    } else {
        println!("\nOpen the URL in a browser, then approve the device.");
    }
    println!("Waiting for approval (Ctrl-C to cancel)…");
    let deadline = Instant::now() + Duration::from_secs(expires_in);
    let token_endpoint = format!("{api_url}/auth/device/token");

    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(interval));
        let request_body = serde_json::to_vec(&json!({ "deviceCode": device_code }))?;
        let response = client
            .post(&token_endpoint)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json")
            .body(request_body)
            .send()
            .context("checking ComputeArena login approval")?;
        let status = response.status();
        let body = response.text().unwrap_or_default();
        if status.is_success() {
            let token: Value = serde_json::from_str(&body).context("parsing login token")?;
            let session = ApiSession {
                access_token: required_json_string(&token, "accessToken")?,
                username: required_json_string(&token, "username")?,
                expires_at: required_json_string(&token, "expiresAt")?,
            };
            save_api_session(paths, api_url, &session)?;
            println!(
                "{} Logged in to {api_url} as @{}.",
                ui.success("✓"),
                session.username
            );
            return Ok(());
        }
        if api_error_code(&body).as_deref() == Some("authorization_pending") {
            continue;
        }
        bail!(
            "{}",
            api_error_message(&body)
                .unwrap_or_else(|| format!("server returned HTTP {}", status.as_u16()))
        );
    }
    bail!("login code expired; run `basert computearena login` again")
}

fn validate_api_session(api_url: &str, session: &ApiSession) -> Result<Option<String>> {
    let client = api_client(AUTH_HTTP_TIMEOUT)?;
    let response = client
        .get(format!("{api_url}/auth/session"))
        .header(reqwest::header::ACCEPT, "application/json")
        .bearer_auth(&session.access_token)
        .send()
        .context("validating the saved ComputeArena login")?;
    let status = response.status();
    let body = response.text().unwrap_or_default();
    if status.is_success() {
        let value: Value =
            serde_json::from_str(&body).context("parsing login validation response")?;
        return Ok(Some(required_json_string(&value, "username")?));
    }
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Ok(None);
    }
    bail!(
        "could not validate saved login: {}",
        api_error_message(&body)
            .unwrap_or_else(|| format!("server returned HTTP {}", status.as_u16()))
    )
}

pub(crate) fn logout(paths: &Paths, api_url: &str) -> Result<()> {
    let ui = TerminalUi::detect();
    let Some(session) = load_api_session(paths, api_url)? else {
        println!("Not logged in to {api_url}.");
        return Ok(());
    };
    let client = api_client(AUTH_HTTP_TIMEOUT)?;
    let revoke_result = client
        .post(format!("{api_url}/auth/logout"))
        .header(reqwest::header::ACCEPT, "application/json")
        .bearer_auth(&session.access_token)
        .send();
    match revoke_result {
        Ok(response) if response.status().is_success() => {}
        Ok(response) => eprintln!(
            "{} Server returned HTTP {}; removing the local session anyway.",
            ui.warning("!"),
            response.status().as_u16()
        ),
        Err(error) => eprintln!(
            "{} Could not reach the server ({error}); removing the local session anyway.",
            ui.warning("!")
        ),
    }
    remove_api_session(paths, api_url)?;
    println!(
        "{} Logged out @{} from {api_url}.",
        ui.success("✓"),
        session.username
    );
    Ok(())
}

fn required_json_string(value: &Value, field: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("server response is missing {field}"))
}

fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = ProcessCommand::new("open");
        command.arg(url);
        command
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = ProcessCommand::new("cmd");
        command.args(["/C", "start", "", url]);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = ProcessCommand::new("xdg-open");
        command.arg(url);
        command
    };
    #[cfg(not(any(unix, target_os = "windows")))]
    return false;

    command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

pub(crate) fn load_api_session(paths: &Paths, api_url: &str) -> Result<Option<ApiSession>> {
    if !paths.auth.is_file() {
        return Ok(None);
    }
    let document = read_report(&paths.auth).context("reading saved ComputeArena login")?;
    let Some(value) = document.pointer(&format!("/origins/{}", json_pointer_escape(api_url)))
    else {
        return Ok(None);
    };
    Ok(Some(ApiSession {
        access_token: required_json_string(value, "access_token")?,
        username: required_json_string(value, "username")?,
        expires_at: required_json_string(value, "expires_at")?,
    }))
}

pub(crate) fn save_api_session(paths: &Paths, api_url: &str, session: &ApiSession) -> Result<()> {
    let mut document = if paths.auth.is_file() {
        read_report(&paths.auth).context("reading saved ComputeArena login")?
    } else {
        json!({ "version": 1, "origins": {} })
    };
    let origins = document
        .get_mut("origins")
        .and_then(Value::as_object_mut)
        .context("saved ComputeArena login has an invalid origins object")?;
    origins.insert(
        api_url.to_string(),
        json!({
            "access_token": session.access_token,
            "username": session.username,
            "expires_at": session.expires_at
        }),
    );
    write_private_json(&paths.auth, &document)
}

pub(crate) fn remove_api_session(paths: &Paths, api_url: &str) -> Result<()> {
    if !paths.auth.is_file() {
        return Ok(());
    }
    let mut document = read_report(&paths.auth).context("reading saved ComputeArena login")?;
    let origins = document
        .get_mut("origins")
        .and_then(Value::as_object_mut)
        .context("saved ComputeArena login has an invalid origins object")?;
    origins.remove(api_url);
    write_private_json(&paths.auth, &document)
}

fn json_pointer_escape(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

fn write_private_json(path: &Path, value: &Value) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating private file under {}", parent.display()))?;
    set_private_permissions(temp.as_file())?;
    serde_json::to_writer_pretty(&mut temp, value)?;
    temp.write_all(b"\n")?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("saving {}", path.display()))?;
    Ok(())
}
