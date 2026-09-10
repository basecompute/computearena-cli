//! Finding and fetching GGUF models from the Hugging Face Hub.
//!
//! Only the public read API is used, and only for models that carry GGUF
//! files. Nothing is uploaded, and no token is sent unless one is already in
//! the environment (`HF_TOKEN`), which gated repositories need.
use crate::config::{HTTP_CONNECT_TIMEOUT, HUGGINGFACE_API, HUGGINGFACE_HOST};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const SEARCH_RESULTS: usize = 25;

pub(crate) struct HubModel {
    pub(crate) id: String,
    pub(crate) downloads: u64,
    pub(crate) likes: u64,
}

pub(crate) struct HubFile {
    pub(crate) path: String,
    pub(crate) size: u64,
}

fn client(timeout: Duration) -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .timeout(timeout)
        .user_agent(format!("computearena/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building the Hugging Face HTTP client")
}

fn request(url: &str, timeout: Duration) -> Result<reqwest::blocking::Response> {
    let mut builder = client(timeout)?.get(url);
    if let Some(token) = std::env::var_os("HF_TOKEN").filter(|token| !token.is_empty()) {
        builder = builder.bearer_auth(token.to_string_lossy());
    }
    let response = builder
        .send()
        .with_context(|| format!("contacting {url}"))?;
    if !response.status().is_success() {
        bail!(
            "Hugging Face answered {} for {url}{}",
            response.status(),
            if response.status().as_u16() == 401 || response.status().as_u16() == 403 {
                ". Gated or private repositories need HF_TOKEN set in the environment."
            } else {
                ""
            }
        );
    }
    Ok(response)
}

/// Models carrying GGUF files, most downloaded first.
pub(crate) fn search(query: &str) -> Result<Vec<HubModel>> {
    let query = query.trim();
    let url = format!(
        "{HUGGINGFACE_API}/models?filter=gguf&sort=downloads&direction=-1&limit={SEARCH_RESULTS}&search={}",
        urlencode(query)
    );
    let body: Value = serde_json::from_str(
        &request(&url, SEARCH_TIMEOUT)?
            .text()
            .context("reading the Hugging Face search results")?,
    )
    .context("parsing the Hugging Face search results")?;
    let entries = body
        .as_array()
        .context("Hugging Face returned an unexpected search result")?;
    Ok(entries
        .iter()
        .filter_map(|entry| {
            Some(HubModel {
                id: entry.get("id").and_then(Value::as_str)?.to_string(),
                downloads: entry
                    .get("downloads")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
                likes: entry
                    .get("likes")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
            })
        })
        .collect())
}

/// The GGUF files inside one repository, smallest first: quantizations are
/// usually chosen by the size a device can hold.
pub(crate) fn gguf_files(repository: &str) -> Result<Vec<HubFile>> {
    let url = format!("{HUGGINGFACE_API}/models/{repository}/tree/main?recursive=true");
    let body: Value = serde_json::from_str(
        &request(&url, SEARCH_TIMEOUT)?
            .text()
            .context("reading the repository listing")?,
    )
    .context("parsing the repository listing")?;
    let entries = body
        .as_array()
        .context("Hugging Face returned an unexpected repository listing")?;
    let mut files: Vec<HubFile> = entries
        .iter()
        .filter(|entry| entry.get("type").and_then(Value::as_str) == Some("file"))
        .filter_map(|entry| {
            let path = entry.get("path").and_then(Value::as_str)?;
            if !path.to_ascii_lowercase().ends_with(".gguf") {
                return None;
            }
            Some(HubFile {
                path: path.to_string(),
                size: entry
                    .get("size")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
            })
        })
        .collect();
    if files.is_empty() {
        bail!("{repository} has no .gguf files");
    }
    files.sort_by_key(|file| file.size);
    Ok(files)
}

pub(crate) fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// Where a downloaded model lives: under the ComputeArena data directory, laid
/// out like the repository it came from so two files never collide.
pub(crate) fn download_path(root: &Path, repository: &str, file: &str) -> PathBuf {
    let mut path = root.join("models");
    for segment in repository.split('/').chain(file.split('/')) {
        path.push(segment);
    }
    path
}

/// Fetch one file, printing progress. Downloads to a temporary name first so an
/// interrupted transfer never looks like a usable model.
pub(crate) fn download(root: &Path, repository: &str, file: &HubFile) -> Result<PathBuf> {
    let destination = download_path(root, repository, &file.path);
    if destination.is_file() {
        println!("Already downloaded: {}", destination.display());
        return Ok(destination);
    }
    let directory = destination
        .parent()
        .context("resolving the download directory")?;
    std::fs::create_dir_all(directory)
        .with_context(|| format!("creating {}", directory.display()))?;

    let url = format!(
        "{HUGGINGFACE_HOST}/{repository}/resolve/main/{}?download=true",
        file.path
    );
    println!("Downloading {} ({})", file.path, format_size(file.size));
    let mut response = request(&url, DOWNLOAD_TIMEOUT)?;
    let total = response.content_length().unwrap_or(file.size);
    let partial = destination.with_extension("part");
    let mut output =
        File::create(&partial).with_context(|| format!("creating {}", partial.display()))?;
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut received: u64 = 0;
    let mut reported = Instant::now();
    loop {
        let count = response.read(&mut buffer).context("reading the download")?;
        if count == 0 {
            break;
        }
        output
            .write_all(&buffer[..count])
            .context("writing the download")?;
        received += count as u64;
        // One line every couple of seconds: enough to show life, few enough to
        // stay readable in a log pane.
        if reported.elapsed() >= Duration::from_secs(2) {
            reported = Instant::now();
            match (received * 100).checked_div(total) {
                Some(percent) => println!(
                    "  {percent:>3}%  {} / {}",
                    format_size(received),
                    format_size(total)
                ),
                None => println!("  {}", format_size(received)),
            }
        }
    }
    output.sync_all().context("flushing the download")?;
    drop(output);
    if total > 0 && received != total {
        let _ = std::fs::remove_file(&partial);
        bail!(
            "the download ended early: {} of {}",
            format_size(received),
            format_size(total)
        );
    }
    std::fs::rename(&partial, &destination).with_context(|| {
        format!(
            "moving {} into place at {}",
            partial.display(),
            destination.display()
        )
    })?;
    println!("Saved {}", destination.display());
    Ok(destination)
}

/// Percent-encode a query for a URL's query string.
fn urlencode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(*byte as char)
            }
            b' ' => encoded.push('+'),
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_are_encoded_and_sizes_read_in_familiar_units() {
        assert_eq!(urlencode("qwen3 0.6b"), "qwen3+0.6b");
        assert_eq!(urlencode("a/b?c=d&e"), "a%2Fb%3Fc%3Dd%26e");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn downloads_mirror_the_repository_layout_under_the_data_directory() {
        let path = download_path(Path::new("/data"), "TheBloke/Qwen-GGUF", "q4/model.gguf");
        assert_eq!(
            path,
            Path::new("/data/models/TheBloke/Qwen-GGUF/q4/model.gguf")
        );
    }
}
