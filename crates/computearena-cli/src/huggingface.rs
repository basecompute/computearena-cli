//! Finding and fetching GGUF models from the Hugging Face Hub.
//!
//! Only the public read API is used, and only for models that carry GGUF
//! files. Nothing is uploaded, and no token is sent unless one is already in
//! the environment (`HF_TOKEN`), which gated repositories need.
use crate::config::{HTTP_CONNECT_TIMEOUT, HUGGINGFACE_API, HUGGINGFACE_HOST};
use anyhow::{bail, Context, Result};
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);

/// Characters that stay literal inside one URL path segment.
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// Percent-encode one path segment, so a branch name such as `refs/pr/1`
/// stays a single segment and no value can steer a request to another path.
fn encode_segment(value: &str) -> String {
    utf8_percent_encode(value, PATH_SEGMENT).to_string()
}

fn encode_path(value: &str) -> String {
    value
        .split('/')
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/")
}
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const SEARCH_RESULTS: usize = 25;

pub(crate) struct HubModel {
    pub(crate) id: String,
    pub(crate) downloads: u64,
    pub(crate) likes: u64,
}

#[derive(Clone)]
pub(crate) struct HubFile {
    pub(crate) path: String,
    pub(crate) size: u64,
    pub(crate) revision: String,
    pub(crate) sha256: Option<String>,
    pub(crate) canonical_repository: Option<String>,
}

pub(crate) struct RepositoryIdentity {
    pub(crate) revision: String,
    pub(crate) canonical_repository: Option<String>,
}

pub(crate) struct HubFileIdentity {
    pub(crate) repository: String,
    pub(crate) file: HubFile,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BaseModelRelation {
    Quantized,
    Finetune,
    Adapter,
    Merge,
    Unknown,
}

fn relation_of(value: &str) -> Option<BaseModelRelation> {
    match value {
        "quantized" => Some(BaseModelRelation::Quantized),
        "finetune" => Some(BaseModelRelation::Finetune),
        "adapter" => Some(BaseModelRelation::Adapter),
        "merge" => Some(BaseModelRelation::Merge),
        _ => None,
    }
}

fn is_repository_id(value: &str) -> bool {
    let mut parts = value.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(owner), Some(model), None) => [owner, model].iter().all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        }),
        _ => false,
    }
}

/// The single upstream model a repository declares itself a quantization of.
///
/// Only a `quantized` relation is followed. A finetune, adapter, or merge is a
/// different model, and an instruct model's card links to its pretraining
/// base exactly that way. The server applies the same rule together with its
/// own publisher trust list, so this is the client's best claim, not the
/// verdict.
fn canonical_repository(info: &Value) -> Option<String> {
    fn add(
        links: &mut BTreeMap<String, (String, BaseModelRelation)>,
        declared: Option<BaseModelRelation>,
        raw: &str,
    ) {
        let (relation, repository) = match raw.split_once(':') {
            Some((prefix, rest)) if relation_of(prefix).is_some() => (relation_of(prefix), rest),
            _ => (None, raw),
        };
        if !is_repository_id(repository) {
            return;
        }
        let relation = relation.or(declared).unwrap_or(BaseModelRelation::Unknown);
        let entry = links
            .entry(repository.to_ascii_lowercase())
            .or_insert((repository.to_string(), relation));
        if entry.1 == BaseModelRelation::Unknown {
            entry.1 = relation;
        }
    }
    let declared = info
        .pointer("/cardData/base_model_relation")
        .and_then(Value::as_str)
        .and_then(relation_of);
    let mut links = BTreeMap::new();
    match info.pointer("/cardData/base_model") {
        Some(Value::String(value)) => add(&mut links, declared, value),
        Some(Value::Array(values)) => {
            for value in values.iter().filter_map(Value::as_str) {
                add(&mut links, declared, value);
            }
        }
        _ => {}
    }
    for tag in info
        .get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|tag| tag.strip_prefix("base_model:"))
    {
        add(&mut links, declared, tag);
    }
    if links.len() != 1 {
        return None;
    }
    let (repository, relation) = links.into_values().next()?;
    (relation == BaseModelRelation::Quantized).then_some(repository)
}

fn repository_info(repository: &str) -> Result<Value> {
    let url = format!("{HUGGINGFACE_API}/models/{repository}");
    serde_json::from_str(
        &request(&url, SEARCH_TIMEOUT)?
            .text()
            .context("reading Hugging Face model metadata")?,
    )
    .context("parsing Hugging Face model metadata")
}

fn repository_info_at(repository: &str, revision: &str) -> Result<Value> {
    let url = format!(
        "{HUGGINGFACE_API}/models/{repository}/revision/{}",
        encode_segment(revision)
    );
    serde_json::from_str(
        &request(&url, SEARCH_TIMEOUT)?
            .text()
            .context("reading Hugging Face model metadata")?,
    )
    .context("parsing Hugging Face model metadata")
}

pub(crate) fn repository_identity(repository: &str) -> Result<RepositoryIdentity> {
    let info = repository_info(repository)?;
    Ok(RepositoryIdentity {
        revision: info
            .get("sha")
            .and_then(Value::as_str)
            .context("Hugging Face model metadata omitted its immutable revision")?
            .to_string(),
        canonical_repository: canonical_repository(&info),
    })
}

fn repository_identity_at(repository: &str, revision: &str) -> Result<RepositoryIdentity> {
    let info = repository_info_at(repository, revision)?;
    Ok(RepositoryIdentity {
        revision: info
            .get("sha")
            .and_then(Value::as_str)
            .context("Hugging Face model metadata omitted its immutable revision")?
            .to_string(),
        canonical_repository: canonical_repository(&info),
    })
}

fn decode_path_segment(value: &str) -> Result<String> {
    percent_decode_str(value)
        .decode_utf8()
        .map(|value| value.into_owned())
        .context("Hugging Face URL contains invalid UTF-8")
}

fn parse_file_url(value: &str) -> Result<(String, String, String)> {
    let url = reqwest::Url::parse(value).context("parsing the Hugging Face file URL")?;
    if url.scheme() != "https" || url.host_str() != Some("huggingface.co") {
        bail!("expected an https://huggingface.co/... file URL");
    }
    let parts: Vec<_> = url
        .path_segments()
        .context("Hugging Face file URL has no path")?
        .map(decode_path_segment)
        .collect::<Result<_>>()?;
    if parts.len() < 5 || !matches!(parts[2].as_str(), "blob" | "resolve") {
        bail!(
            "expected a Hugging Face file URL such as https://huggingface.co/owner/model/blob/revision/path/to/model.gguf"
        );
    }
    Ok((
        format!("{}/{}", parts[0], parts[1]),
        parts[3].clone(),
        parts[4..].join("/"),
    ))
}

fn next_page(response: &reqwest::blocking::Response) -> Option<String> {
    response
        .headers()
        .get(reqwest::header::LINK)?
        .to_str()
        .ok()?
        .split(',')
        .find_map(|link| {
            let (url, attributes) = link.trim().split_once(';')?;
            if !attributes
                .split(';')
                .any(|attribute| attribute.trim() == "rel=\"next\"")
            {
                return None;
            }
            url.trim()
                .strip_prefix('<')?
                .strip_suffix('>')
                .map(str::to_string)
        })
}

fn repository_tree(repository: &str, revision: &str) -> Result<Vec<Value>> {
    let mut url = Some(format!(
        "{HUGGINGFACE_API}/models/{repository}/tree/{}?recursive=true&expand=true",
        encode_segment(revision)
    ));
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();
    while let Some(page) = url.take() {
        if !seen.insert(page.clone()) {
            bail!("Hugging Face returned a cyclic pagination link");
        }
        let response = request(&page, SEARCH_TIMEOUT)?;
        url = next_page(&response);
        if let Some(next) = &url {
            let next = reqwest::Url::parse(next).context("parsing Hugging Face pagination URL")?;
            if next.scheme() != "https" || next.host_str() != Some("huggingface.co") {
                bail!("Hugging Face returned an unsafe pagination URL");
            }
        }
        let body: Value =
            serde_json::from_str(&response.text().context("reading the repository listing")?)
                .context("parsing the repository listing")?;
        entries.extend(
            body.as_array()
                .context("Hugging Face returned an unexpected repository listing")?
                .iter()
                .cloned(),
        );
    }
    Ok(entries)
}

/// Resolve a human-facing Hugging Face file URL to immutable repository and
/// file identity. `paths-info` exposes the content SHA-256 for LFS/Xet model
/// objects without downloading the model again.
pub(crate) fn file_identity(url: &str) -> Result<HubFileIdentity> {
    let (repository, requested_revision, path) = parse_file_url(url)?;
    artifact_identity(&repository, &requested_revision, &path)
}

/// Resolve an already identified repository/revision/path tuple without
/// searching the whole Hub. Submission preflight uses this bounded lookup to
/// compare the report's local SHA-256 with the publisher's LFS object ID.
pub(crate) fn artifact_identity(
    repository: &str,
    requested_revision: &str,
    path: &str,
) -> Result<HubFileIdentity> {
    if repository.split('/').count() != 2
        || repository
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        bail!("invalid Hugging Face repository ID in model identity");
    }
    if path.starts_with('/')
        || path
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        bail!("invalid Hugging Face artifact path in model identity");
    }
    let identity = repository_identity_at(repository, requested_revision)?;
    let info_url = format!(
        "{HUGGINGFACE_API}/models/{repository}/paths-info/{}",
        encode_segment(&identity.revision)
    );
    let mut builder = client(SEARCH_TIMEOUT)?
        .post(&info_url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(serde_json::to_vec(&serde_json::json!({"paths": [&path]}))?);
    if let Some(token) = std::env::var_os("HF_TOKEN").filter(|token| !token.is_empty()) {
        builder = builder.bearer_auth(token.to_string_lossy());
    }
    let response = builder
        .send()
        .with_context(|| format!("contacting {info_url}"))?;
    if !response.status().is_success() {
        bail!("Hugging Face answered {} for {info_url}", response.status());
    }
    let body: Value = serde_json::from_str(
        &response
            .text()
            .context("reading Hugging Face file metadata")?,
    )
    .context("parsing Hugging Face file metadata")?;
    let entry = body
        .as_array()
        .and_then(|entries| entries.first())
        .filter(|entry| entry.get("type").and_then(Value::as_str) == Some("file"))
        .with_context(|| format!("{repository} has no file named {path} at that revision"))?;
    let sha256 = entry
        .pointer("/lfs/oid")
        .and_then(Value::as_str)
        .filter(|sha| sha.len() == 64 && sha.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(|sha| sha.to_ascii_lowercase());
    Ok(HubFileIdentity {
        repository: repository.to_string(),
        file: HubFile {
            path: path.to_string(),
            size: entry
                .get("size")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            revision: identity.revision,
            sha256,
            canonical_repository: identity.canonical_repository,
        },
    })
}

/// Find the single Hub file whose published LFS SHA-256 matches local bytes.
/// This recovers the actual artifact path when a runtime sidecar only records
/// a repository and a mutable ref such as `main`.
pub(crate) fn find_file_by_sha256(
    repository: &str,
    revision: &str,
    sha256: &str,
) -> Result<HubFile> {
    let identity = repository_identity_at(repository, revision)?;
    let entries = repository_tree(repository, &identity.revision)?;
    let mut matches = entries.iter().filter(|entry| {
        entry
            .pointer("/lfs/oid")
            .and_then(Value::as_str)
            .is_some_and(|oid| oid.eq_ignore_ascii_case(sha256))
    });
    let entry = matches.next().with_context(|| {
        format!("{repository} has no file matching the local SHA-256 at {revision}")
    })?;
    if matches.next().is_some() {
        bail!("{repository} contains multiple files with that SHA-256 at {revision}");
    }
    Ok(HubFile {
        path: entry
            .get("path")
            .and_then(Value::as_str)
            .context("Hugging Face file metadata omitted its path")?
            .to_string(),
        size: entry
            .get("size")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        revision: identity.revision,
        sha256: Some(sha256.to_ascii_lowercase()),
        canonical_repository: identity.canonical_repository,
    })
}

pub(crate) fn gguf_files(repository: &str) -> Result<Vec<HubFile>> {
    let identity = repository_identity(repository)?;
    let revision = identity.revision;
    let canonical_repository = identity.canonical_repository;
    let entries = repository_tree(repository, &revision)?;
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
                revision: revision.clone(),
                sha256: entry
                    .pointer("/lfs/oid")
                    .and_then(Value::as_str)
                    .filter(|sha| {
                        sha.len() == 64 && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
                    .map(|sha| sha.to_ascii_lowercase()),
                canonical_repository: canonical_repository.clone(),
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
pub(crate) fn download_path(root: &Path, repository: &str, file: &str) -> Result<PathBuf> {
    let mut path = root.join("models");
    for segment in repository.split('/').chain(file.split('/')) {
        if segment.is_empty() || matches!(segment, "." | "..") {
            bail!("Hugging Face returned an unsafe model path");
        }
        path.push(segment);
    }
    Ok(path)
}

/// Fetch one file, printing progress. Downloads to a temporary name first so an
/// interrupted transfer never looks like a usable model.
pub(crate) fn download(root: &Path, repository: &str, file: &HubFile) -> Result<PathBuf> {
    let destination = download_path(root, repository, &file.path)?;
    if destination.is_file() {
        let sha256 = crate::adapters::file_sha256(&destination)?;
        if file
            .sha256
            .as_deref()
            .is_some_and(|expected| expected != sha256)
        {
            bail!(
                "the existing file at {} does not match Hugging Face's SHA-256",
                destination.display()
            );
        }
        crate::model_identity::record_huggingface_download(
            root,
            repository,
            &file.revision,
            &file.path,
            &sha256,
            file.sha256.as_deref(),
            file.canonical_repository.as_deref(),
            "computearena_download",
        )?;
        println!("Already downloaded: {}", destination.display());
        return Ok(destination);
    }
    let directory = destination
        .parent()
        .context("resolving the download directory")?;
    std::fs::create_dir_all(directory)
        .with_context(|| format!("creating {}", directory.display()))?;

    let url = format!(
        "{HUGGINGFACE_HOST}/{repository}/resolve/{}/{}?download=true",
        encode_segment(&file.revision),
        encode_path(&file.path)
    );
    println!("Downloading {} ({})", file.path, format_size(file.size));
    let mut response = request(&url, DOWNLOAD_TIMEOUT)?;
    let total = response.content_length().unwrap_or(file.size);
    let partial = destination.with_extension("part");
    let mut output =
        File::create(&partial).with_context(|| format!("creating {}", partial.display()))?;
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut received: u64 = 0;
    let mut digest = Sha256::new();
    let mut reported = Instant::now();
    loop {
        let count = response.read(&mut buffer).context("reading the download")?;
        if count == 0 {
            break;
        }
        output
            .write_all(&buffer[..count])
            .context("writing the download")?;
        digest.update(&buffer[..count]);
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
    let sha256 = format!("{:x}", digest.finalize());
    if file
        .sha256
        .as_deref()
        .is_some_and(|expected| expected != sha256)
    {
        let _ = std::fs::remove_file(&partial);
        bail!(
            "Hugging Face SHA-256 mismatch for {}: expected {}, got {sha256}",
            file.path,
            file.sha256.as_deref().unwrap_or("unknown")
        );
    }
    std::fs::rename(&partial, &destination).with_context(|| {
        format!(
            "moving {} into place at {}",
            partial.display(),
            destination.display()
        )
    })?;
    crate::model_identity::record_huggingface_download(
        root,
        repository,
        &file.revision,
        &file.path,
        &sha256,
        file.sha256.as_deref(),
        file.canonical_repository.as_deref(),
        "computearena_download",
    )?;
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
    fn parses_huggingface_file_urls() {
        assert_eq!(
            parse_file_url(
                "https://huggingface.co/bartowski/Qwen-GGUF/blob/main/models/Q4_K_M/model%20one.gguf?download=true"
            )
            .unwrap(),
            (
                "bartowski/Qwen-GGUF".to_string(),
                "main".to_string(),
                "models/Q4_K_M/model one.gguf".to_string()
            )
        );
        assert!(parse_file_url("https://example.com/a/b/blob/main/model.gguf").is_err());
    }

    #[test]
    fn downloads_mirror_the_repository_layout_under_the_data_directory() {
        let path =
            download_path(Path::new("/data"), "TheBloke/Qwen-GGUF", "q4/model.gguf").unwrap();
        assert_eq!(
            path,
            Path::new("/data/models/TheBloke/Qwen-GGUF/q4/model.gguf")
        );
        assert!(download_path(Path::new("/data"), "owner/model", "../model.gguf").is_err());
    }

    #[test]
    fn reads_one_canonical_base_model_from_hub_metadata() {
        let info = serde_json::json!({
            "cardData": {"base_model": "Qwen/Qwen3-4B"},
            "tags": ["base_model:quantized:Qwen/Qwen3-4B"]
        });
        assert_eq!(
            canonical_repository(&info).as_deref(),
            Some("Qwen/Qwen3-4B")
        );
    }

    #[test]
    fn merged_models_do_not_claim_one_canonical_identity() {
        let info = serde_json::json!({
            "cardData": {"base_model": ["one/model", "two/model"]}
        });
        assert_eq!(canonical_repository(&info), None);
    }

    #[test]
    fn only_quantizations_inherit_the_upstream_model() {
        let instruct = serde_json::json!({
            "cardData": {"base_model": "google/gemma-3-1b-pt"},
            "tags": ["base_model:google/gemma-3-1b-pt", "base_model:finetune:google/gemma-3-1b-pt"]
        });
        assert_eq!(canonical_repository(&instruct), None);
        let declared = serde_json::json!({
            "cardData": {"base_model": "Qwen/Qwen3-4B", "base_model_relation": "quantized"}
        });
        assert_eq!(
            canonical_repository(&declared).as_deref(),
            Some("Qwen/Qwen3-4B")
        );
        let undeclared = serde_json::json!({"cardData": {"base_model": "Qwen/Qwen3-4B"}});
        assert_eq!(canonical_repository(&undeclared), None);
    }

    #[test]
    fn revisions_and_paths_are_encoded_per_segment() {
        assert_eq!(encode_segment("refs/pr/1"), "refs%2Fpr%2F1");
        assert_eq!(encode_segment("../x"), "..%2Fx");
        assert_eq!(
            encode_path("Q4_K_M/model one.gguf"),
            "Q4_K_M/model%20one.gguf"
        );
    }
}
