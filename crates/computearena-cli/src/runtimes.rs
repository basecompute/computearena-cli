//! Locating runtime executables, explaining how to obtain them, and installing
//! prebuilt releases when asked to.
//!
//! Discovery order is the same for every runtime: an explicit `--runtime-path`,
//! the runtime's environment variables, a copy installed by ComputeArena, PATH,
//! and finally the runtime's own conventional install location. The source is
//! reported back so people can see which executable will run.

use crate::adapters::Runtime;
use crate::config::HTTP_CONNECT_TIMEOUT;
use crate::reports::Paths;
use crate::ui::{
    choose, finish_activity, print_fields, prompt_yes_no, start_activity, MenuChoice, MenuItem,
    TerminalUi,
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) const BASERT_RELEASES: &str = "https://github.com/basecompute/baseRT/releases";
pub(crate) const BASERT_INSTALL_SCRIPT: &str = "curl -LsSf https://basecompute.co/install.sh | sh";
pub(crate) const LLAMA_CPP_RELEASES: &str = "https://github.com/ggml-org/llama.cpp/releases";
const GITHUB_API: &str = "https://api.github.com";
const USER_AGENT: &str = concat!("computearena-cli/", env!("CARGO_PKG_VERSION"));
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(900);
const RELEASE_LIST_SIZE: usize = 20;

fn platform_notice_for(runtime: Runtime, os: &str, arch: &str) -> Option<String> {
    (runtime == Runtime::Basert && os == "linux" && arch == "x86_64").then(|| {
        "BaseRT does not currently publish a prebuilt Linux x86-64 runtime. ComputeArena itself works on this architecture: choose llama.cpp, or pass a compatible basert-benchmark-harness that you built yourself."
            .to_string()
    })
}

/// A warning for platforms on which the ComputeArena client is published but
/// the selected runtime is not. This is advisory: a manually built compatible
/// harness remains usable.
pub(crate) fn platform_notice(runtime: Runtime) -> Option<String> {
    platform_notice_for(runtime, std::env::consts::OS, std::env::consts::ARCH)
}

pub(crate) fn print_platform_notice(ui: TerminalUi, runtime: Runtime) {
    if let Some(notice) = platform_notice(runtime) {
        println!();
        println!("{} {}", ui.warning("!"), ui.neutral(notice));
    }
}

// --- Discovery --------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    Override,
    Environment(&'static str),
    Managed,
    Path,
    KnownLocation,
}

impl Source {
    pub(crate) fn describe(self) -> String {
        match self {
            Self::Override => "from --runtime-path".to_string(),
            Self::Environment(variable) => format!("from {variable}"),
            Self::Managed => "installed by ComputeArena".to_string(),
            Self::Path => "on PATH".to_string(),
            Self::KnownLocation => "in its default install location".to_string(),
        }
    }
}

pub(crate) struct Located {
    pub(crate) path: PathBuf,
    pub(crate) source: Source,
}

pub(crate) fn executable_on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    })
}

pub(crate) fn executable_path(path: PathBuf) -> Result<PathBuf> {
    if path.components().count() == 1 {
        return executable_on_path(path.to_string_lossy().as_ref()).with_context(|| {
            format!(
                "benchmark harness was not found on PATH: {}",
                path.display()
            )
        });
    }
    if !path.is_file() {
        bail!("benchmark harness not found: {}", path.display());
    }
    Ok(path)
}

/// The BaseRT install directory used by the official installer.
pub(crate) fn basert_install_dir() -> Option<PathBuf> {
    match std::env::var_os("BASERT_INSTALL_DIR") {
        Some(directory) if !directory.is_empty() => Some(PathBuf::from(directory)),
        _ => dirs::home_dir().map(|home| home.join(".basert")),
    }
}

pub(crate) fn locate(
    runtime: Runtime,
    override_path: Option<PathBuf>,
    paths: &Paths,
) -> Result<Located> {
    let adapter = runtime.adapter();
    if let Some(path) = override_path {
        return Ok(Located {
            path: executable_path(path)?,
            source: Source::Override,
        });
    }
    for variable in adapter.environment_overrides() {
        if let Some(value) = std::env::var_os(variable) {
            if value.is_empty() {
                bail!("{variable} is set but empty");
            }
            return Ok(Located {
                path: executable_path(PathBuf::from(value))?,
                source: Source::Environment(variable),
            });
        }
    }
    if let Some(record) = InstallRecord::load(paths, runtime)? {
        if record.executable.is_file() {
            return Ok(Located {
                path: record.executable,
                source: Source::Managed,
            });
        }
    }
    if let Some(path) = executable_on_path(adapter.binary_name()) {
        return Ok(Located {
            path,
            source: Source::Path,
        });
    }
    for directory in adapter.known_locations() {
        let candidate = directory.join(adapter.binary_name());
        if candidate.is_file() {
            return Ok(Located {
                path: candidate,
                source: Source::KnownLocation,
            });
        }
    }
    bail!("{}", not_found_message(runtime));
}

fn not_found_message(runtime: Runtime) -> String {
    let adapter = runtime.adapter();
    let mut looked = vec!["PATH".to_string()];
    looked.extend(
        adapter
            .known_locations()
            .iter()
            .map(|directory| compact_path(directory)),
    );
    let install_hint = if platform_notice(runtime).is_some() {
        "ComputeArena cannot install a prebuilt BaseRT runtime on this architecture.".to_string()
    } else {
        format!(
            "Or run `computearena {} install` to let ComputeArena download it.",
            adapter.name()
        )
    };
    format!(
        "{} was not found; ComputeArena looked for {} on {}.\n{}\n{}",
        adapter.display_name(),
        adapter.binary_name(),
        looked.join(" and in "),
        manual_instructions(runtime).join("\n"),
        install_hint
    )
}

/// How to obtain the runtime without ComputeArena's help.
pub(crate) fn manual_instructions(runtime: Runtime) -> Vec<String> {
    match runtime {
        Runtime::Basert if platform_notice(runtime).is_some() => vec![
            platform_notice(runtime).unwrap(),
            "Choose llama.cpp for a supported prebuilt runtime on Linux x86-64.".to_string(),
            "If you built BaseRT yourself, pass --runtime-path /path/to/basert-benchmark-harness."
                .to_string(),
        ],
        Runtime::Basert => vec![
            format!("Install BaseRT with the official installer: {BASERT_INSTALL_SCRIPT}"),
            "Restart your terminal afterwards so basert-benchmark-harness is on PATH,".to_string(),
            "or pass --runtime-path ~/.basert/basert-benchmark-harness.".to_string(),
            format!("Releases: {BASERT_RELEASES}"),
        ],
        Runtime::LlamaCpp => vec![
            "On macOS: brew install llama.cpp".to_string(),
            format!("On any platform: download a build from {LLAMA_CPP_RELEASES}"),
            "Then add llama-bench to PATH, or pass --runtime-path /path/to/llama-bench."
                .to_string(),
        ],
    }
}

pub(crate) fn compact_path(path: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rest) = path.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

// --- Install records --------------------------------------------------------

/// What ComputeArena installed itself, kept beside the reports so discovery
/// prefers it over an older copy on PATH.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct InstallRecord {
    pub(crate) executable: PathBuf,
    pub(crate) version: String,
    pub(crate) asset: String,
    pub(crate) installed_at_unix_ms: u64,
}

impl InstallRecord {
    fn file(paths: &Paths, runtime: Runtime) -> PathBuf {
        paths
            .root
            .join("runtimes")
            .join(format!("{}.json", runtime.adapter().name()))
    }

    pub(crate) fn load(paths: &Paths, runtime: Runtime) -> Result<Option<Self>> {
        let file = Self::file(paths, runtime);
        let contents = match fs::read(&file) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).with_context(|| format!("reading {}", file.display())),
        };
        let value: Value = serde_json::from_slice(&contents)
            .with_context(|| format!("{} is not valid JSON", file.display()))?;
        Ok(Self::from_value(&value).map(|record| record.rebased(&paths.root)))
    }

    /// A record written before the data directory moved names the executable
    /// under the old root. The installed copy moved with everything else, so
    /// the path is rebuilt from its `runtimes/…` tail under the current root.
    fn rebased(mut self, root: &Path) -> Self {
        if self.executable.is_file() {
            return self;
        }
        let components: Vec<_> = self.executable.components().collect();
        if let Some(index) = components
            .iter()
            .position(|component| component.as_os_str() == "runtimes")
        {
            let candidate = components[index..]
                .iter()
                .fold(root.to_path_buf(), |path, component| path.join(component));
            if candidate.is_file() {
                self.executable = candidate;
            }
        }
        self
    }

    fn from_value(value: &Value) -> Option<Self> {
        Some(Self {
            executable: PathBuf::from(value["executable"].as_str()?),
            version: value["version"].as_str()?.to_string(),
            asset: value["asset"].as_str().unwrap_or_default().to_string(),
            installed_at_unix_ms: value["installed_at_unix_ms"].as_u64().unwrap_or_default(),
        })
    }

    pub(crate) fn save(&self, paths: &Paths, runtime: Runtime) -> Result<()> {
        let file = Self::file(paths, runtime);
        let directory = file.parent().context("install record has no parent")?;
        fs::create_dir_all(directory)
            .with_context(|| format!("creating {}", directory.display()))?;
        let value = json!({
            "executable": self.executable,
            "version": self.version,
            "asset": self.asset,
            "installed_at_unix_ms": self.installed_at_unix_ms,
        });
        fs::write(&file, serde_json::to_vec_pretty(&value)?)
            .with_context(|| format!("writing {}", file.display()))
    }
}

// --- Releases ---------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReleaseAsset {
    pub(crate) tag: String,
    pub(crate) name: String,
    pub(crate) url: String,
    pub(crate) size: u64,
    pub(crate) checksum_url: Option<String>,
    pub(crate) published_at: Option<String>,
}

/// The asset name each runtime publishes for a platform, or why there is none.
pub(crate) fn asset_rule(runtime: Runtime, os: &str, arch: &str) -> Result<AssetRule> {
    match runtime {
        Runtime::Basert => match (os, arch) {
            ("macos", "aarch64") => Ok(AssetRule {
                prefix: "basert-engine-macos-arm64-".to_string(),
                suffix: ".tar.gz",
                checksum_suffix: Some(".sha256"),
                backend: "Metal".to_string(),
            }),
            ("linux", "aarch64") => Ok(AssetRule {
                prefix: "basert-engine-linux-arm64-cuda-".to_string(),
                suffix: ".tar.gz",
                checksum_suffix: Some(".sha256"),
                backend: "CUDA (needs an NVIDIA GPU and driver)".to_string(),
            }),
            _ => bail!(
                "BaseRT publishes prebuilt bundles for macOS/arm64 and Linux/arm64 with CUDA, not {os}/{arch}. See {BASERT_RELEASES}."
            ),
        },
        Runtime::LlamaCpp => {
            let (platform, backend) = match (os, arch) {
                ("macos", "aarch64") => ("macos-arm64", "Metal"),
                ("macos", "x86_64") => ("macos-x64", "CPU"),
                ("linux", "x86_64") => ("ubuntu-x64", "CPU only; CUDA, Vulkan, and ROCm builds must be installed manually"),
                ("linux", "aarch64") => ("ubuntu-arm64", "CPU only; GPU builds must be installed manually"),
                _ => bail!(
                    "ComputeArena can install prebuilt llama.cpp builds for macOS and Linux, not {os}/{arch}. Download one yourself from {LLAMA_CPP_RELEASES}."
                ),
            };
            Ok(AssetRule {
                prefix: format!("llama-{{tag}}-bin-{platform}"),
                suffix: ".tar.gz",
                checksum_suffix: None,
                backend: backend.to_string(),
            })
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AssetRule {
    /// Literal prefix; `{tag}` stands for the release tag.
    prefix: String,
    suffix: &'static str,
    checksum_suffix: Option<&'static str>,
    pub(crate) backend: String,
}

impl AssetRule {
    fn matches(&self, tag: &str, name: &str) -> bool {
        let prefix = self.prefix.replace("{tag}", tag);
        name.starts_with(&prefix) && name.ends_with(self.suffix)
    }
}

/// Pick the newest release that carries this platform's bundle. llama.cpp
/// marks every build a pre-release and keeps an unrelated "latest" release,
/// so the list is scanned in order rather than trusting one entry.
pub(crate) fn select_asset(
    runtime: Runtime,
    releases: &[Value],
    os: &str,
    arch: &str,
) -> Result<ReleaseAsset> {
    let rule = asset_rule(runtime, os, arch)?;
    for release in releases {
        if release["draft"].as_bool() == Some(true) {
            continue;
        }
        let Some(tag) = release["tag_name"].as_str() else {
            continue;
        };
        if runtime == Runtime::LlamaCpp && !is_build_tag(tag) {
            continue;
        }
        let Some(assets) = release["assets"].as_array() else {
            continue;
        };
        let Some(asset) = assets.iter().find(|asset| {
            asset["name"]
                .as_str()
                .is_some_and(|name| rule.matches(tag, name))
        }) else {
            continue;
        };
        let name = asset["name"].as_str().unwrap_or_default().to_string();
        let checksum_url = rule.checksum_suffix.and_then(|suffix| {
            let expected = format!("{name}{suffix}");
            assets
                .iter()
                .find(|asset| asset["name"].as_str() == Some(expected.as_str()))
                .and_then(|asset| asset["browser_download_url"].as_str())
                .map(str::to_string)
        });
        return Ok(ReleaseAsset {
            tag: tag.to_string(),
            url: asset["browser_download_url"]
                .as_str()
                .context("release asset has no download URL")?
                .to_string(),
            size: asset["size"].as_u64().unwrap_or_default(),
            checksum_url,
            published_at: release["published_at"].as_str().map(str::to_string),
            name,
        });
    }
    bail!(
        "no {} release carries a prebuilt bundle for {os}/{arch}; see {}",
        runtime.adapter().display_name(),
        releases_page(runtime)
    );
}

fn is_build_tag(tag: &str) -> bool {
    tag.len() > 1 && tag.starts_with('b') && tag[1..].chars().all(|c| c.is_ascii_digit())
}

pub(crate) fn releases_page(runtime: Runtime) -> &'static str {
    match runtime {
        Runtime::Basert => BASERT_RELEASES,
        Runtime::LlamaCpp => LLAMA_CPP_RELEASES,
    }
}

fn github_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .context("creating HTTP client")
}

fn fetch_releases(runtime: Runtime) -> Result<Vec<Value>> {
    let url = match runtime {
        Runtime::Basert => format!("{GITHUB_API}/repos/basecompute/baseRT/releases/latest"),
        Runtime::LlamaCpp => {
            format!("{GITHUB_API}/repos/ggml-org/llama.cpp/releases?per_page={RELEASE_LIST_SIZE}")
        }
    };
    let response = github_client()?
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .with_context(|| format!("contacting {url}"))?;
    if !response.status().is_success() {
        bail!(
            "GitHub answered {} for {url}; try again later or install {} yourself",
            response.status(),
            runtime.adapter().display_name()
        );
    }
    let body = response.text().context("reading the release list")?;
    let value: Value = serde_json::from_str(&body).context("parsing the release list")?;
    Ok(match value {
        Value::Array(releases) => releases,
        release => vec![release],
    })
}

/// A `<hex>  <file>` line as written by shasum and sha256sum.
pub(crate) fn parse_checksum(contents: &str) -> Option<String> {
    let digest = contents.split_whitespace().next()?;
    (digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| digest.to_ascii_lowercase())
}

/// The version encoded in a bundle file name, for archives installed offline.
pub(crate) fn version_from_archive_name(runtime: Runtime, name: &str) -> Option<String> {
    let stem = name.strip_suffix(".tar.gz")?;
    match runtime {
        Runtime::Basert => stem.rsplit('-').next().map(str::to_string),
        Runtime::LlamaCpp => stem
            .strip_prefix("llama-")
            .and_then(|rest| rest.split("-bin-").next())
            .filter(|tag| is_build_tag(tag))
            .map(str::to_string),
    }
}

// --- Install plans ----------------------------------------------------------

pub(crate) enum ArchiveSource {
    Release(ReleaseAsset),
    Local(PathBuf),
}

pub(crate) struct InstallPlan {
    pub(crate) runtime: Runtime,
    pub(crate) source: ArchiveSource,
    pub(crate) version: String,
    pub(crate) destination: PathBuf,
    pub(crate) backend: String,
}

/// Where each runtime is installed. BaseRT goes where its official installer
/// puts it, so the basert model tools live alongside; llama.cpp goes under
/// ComputeArena's data directory, one folder per build.
pub(crate) fn destination(runtime: Runtime, paths: &Paths, version: &str) -> Result<PathBuf> {
    match runtime {
        Runtime::Basert => basert_install_dir().context("cannot locate the home directory"),
        Runtime::LlamaCpp => Ok(paths.root.join("runtimes").join("llama-cpp").join(version)),
    }
}

pub(crate) fn plan_install(
    runtime: Runtime,
    paths: &Paths,
    archive: Option<PathBuf>,
) -> Result<InstallPlan> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let (source, version, backend) = match archive {
        Some(archive) => {
            if !archive.is_file() {
                bail!("archive not found: {}", archive.display());
            }
            let name = archive
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            let version =
                version_from_archive_name(runtime, &name).unwrap_or_else(|| "local".to_string());
            let backend = asset_rule(runtime, os, arch)
                .map(|rule| rule.backend)
                .unwrap_or_else(|_| "as built".to_string());
            (ArchiveSource::Local(archive), version, backend)
        }
        None => {
            let rule = asset_rule(runtime, os, arch)?;
            let releases = fetch_releases(runtime)?;
            let asset = select_asset(runtime, &releases, os, arch)?;
            let version = asset.tag.trim_start_matches('v').to_string();
            (ArchiveSource::Release(asset), version, rule.backend)
        }
    };
    Ok(InstallPlan {
        runtime,
        destination: destination(runtime, paths, &version)?,
        source,
        version,
        backend,
    })
}

fn megabytes(size: u64) -> String {
    format!("{:.1} MB", size as f64 / 1_000_000.0)
}

pub(crate) fn print_plan(ui: TerminalUi, plan: &InstallPlan) {
    let adapter = plan.runtime.adapter();
    ui.section(&format!("Install {}", adapter.display_name()));
    let mut rows: Vec<(&str, String)> = Vec::new();
    match &plan.source {
        ArchiveSource::Release(asset) => {
            rows.push((
                "Release",
                match &asset.published_at {
                    Some(published) => format!(
                        "{} (published {})",
                        asset.tag,
                        &published[..published.len().min(10)]
                    ),
                    None => asset.tag.clone(),
                },
            ));
            rows.push((
                "Download",
                format!("{} · {}\n{}", asset.name, megabytes(asset.size), asset.url),
            ));
        }
        ArchiveSource::Local(archive) => {
            rows.push(("Archive", archive.display().to_string()));
        }
    }
    rows.push(("Destination", compact_path(&plan.destination)));
    rows.push(("Backend", plan.backend.clone()));
    rows.push((
        "Checksum",
        match &plan.source {
            ArchiveSource::Release(asset) if asset.checksum_url.is_some() => {
                "verified against the SHA-256 published with the release".to_string()
            }
            ArchiveSource::Release(_) => {
                "not published by llama.cpp; the archive is fetched over HTTPS from GitHub"
                    .to_string()
            }
            ArchiveSource::Local(_) => "none; a local archive is trusted as given".to_string(),
        },
    ));
    rows.push((
        "Afterwards",
        match plan.runtime {
            Runtime::Basert => format!(
                "{} and the basert model tools live in {}, the same place the official installer uses. Existing BaseRT bundle files there are replaced. Add that directory to PATH to use `basert pull` from your shell.",
                adapter.binary_name(),
                compact_path(&plan.destination)
            ),
            Runtime::LlamaCpp => "ComputeArena uses this copy for llama.cpp benchmarks; it is not added to PATH. Older copies installed by ComputeArena are removed. Delete the folder to uninstall.".to_string(),
        },
    ));
    if plan.runtime == Runtime::Basert {
        rows.push((
            "License",
            "the bundle includes the BaseRT engine license (LICENSE)".to_string(),
        ));
    }
    print_fields(ui, &rows);
}

// --- Downloading and extracting ---------------------------------------------

struct Downloaded {
    file: PathBuf,
    sha256: String,
}

fn download(ui: TerminalUi, asset: &ReleaseAsset, into: &Path) -> Result<Downloaded> {
    let client = github_client()?;
    let mut response = client
        .get(&asset.url)
        .send()
        .with_context(|| format!("downloading {}", asset.url))?;
    if !response.status().is_success() {
        bail!("GitHub answered {} for {}", response.status(), asset.url);
    }
    let total = response
        .content_length()
        .filter(|length| *length > 0)
        .unwrap_or(asset.size);
    let file = into.join(&asset.name);
    let mut output = File::create(&file).with_context(|| format!("creating {}", file.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 65536];
    let mut received: u64 = 0;
    let mut shown = u64::MAX;
    let progress = io::stdout().is_terminal();
    loop {
        let count = response.read(&mut buffer).context("reading the download")?;
        if count == 0 {
            break;
        }
        output.write_all(&buffer[..count])?;
        digest.update(&buffer[..count]);
        received += count as u64;
        if progress && total > 0 {
            let percent = received * 100 / total;
            if percent != shown {
                print!(
                    "\r  {} {}% of {}",
                    ui.muted("Downloading…"),
                    percent,
                    megabytes(total)
                );
                let _ = io::stdout().flush();
                shown = percent;
            }
        }
    }
    if progress {
        println!();
    }
    output.flush()?;
    Ok(Downloaded {
        file,
        sha256: format!("{:x}", digest.finalize()),
    })
}

fn fetch_checksum(url: &str) -> Result<String> {
    let contents = github_client()?
        .get(url)
        .send()
        .with_context(|| format!("downloading {url}"))?
        .error_for_status()
        .with_context(|| format!("downloading {url}"))?
        .text()
        .context("reading the checksum file")?;
    parse_checksum(&contents).with_context(|| format!("{url} does not contain a SHA-256 digest"))
}

/// Unpack a `.tar.gz` bundle into `destination`, refusing entries that would
/// land outside it, and return the runtime executable found inside.
pub(crate) fn extract_archive(
    archive: &Path,
    destination: &Path,
    binary_name: &str,
) -> Result<PathBuf> {
    fs::create_dir_all(destination)
        .with_context(|| format!("creating {}", destination.display()))?;
    let file = File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let mut tarball = tar::Archive::new(flate2::read::GzDecoder::new(file));
    tarball.set_preserve_permissions(true);
    tarball.set_overwrite(true);
    for entry in tarball.entries().context("reading the archive")? {
        let mut entry = entry.context("reading an archive entry")?;
        let path = entry
            .path()
            .context("reading an archive entry name")?
            .into_owned();
        if !entry
            .unpack_in(destination)
            .context("extracting the archive")?
        {
            bail!(
                "the archive tried to write outside the destination: {}",
                path.display()
            );
        }
    }
    find_executable(destination, binary_name, 0).with_context(|| {
        format!("the archive does not contain {binary_name}; nothing was recorded")
    })
}

fn find_executable(directory: &Path, binary_name: &str, depth: usize) -> Option<PathBuf> {
    if depth > 3 {
        return None;
    }
    let mut entries: Vec<_> = fs::read_dir(directory).ok()?.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in &entries {
        let path = entry.path();
        if path.is_file() && entry.file_name() == binary_name {
            return Some(path);
        }
    }
    entries
        .iter()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .find_map(|path| find_executable(&path, binary_name, depth + 1))
}

/// The official BaseRT installer clears the previous bundle before unpacking
/// so files dropped from a newer release cannot linger. Mirror its list; an
/// unrelated file in a custom install directory is left alone.
fn clear_basert_bundle(directory: &Path) -> Result<()> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        let bundled = name == "basert"
            || name.starts_with("basert-")
            || name.starts_with("baseRT_")
            || name.starts_with("libbaseRT")
            || name == "baseRT.metallib"
            || name == "LICENSE"
            || name == "NOTICE";
        if name == "include" && path.is_dir() {
            fs::remove_dir_all(&path).with_context(|| format!("removing {}", path.display()))?;
        } else if bundled && !path.is_dir() {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn mark_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(permissions.mode() | 0o755);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("marking {} executable", path.display()))
}

#[cfg(not(unix))]
fn mark_executable(_path: &Path) -> Result<()> {
    Ok(())
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Carry out a plan: download (or use the local archive), verify, unpack,
/// mark executables, and record what was installed. Returns the executable.
pub(crate) fn perform_install(
    ui: TerminalUi,
    paths: &Paths,
    plan: &InstallPlan,
) -> Result<PathBuf> {
    let adapter = plan.runtime.adapter();
    let staging = tempfile::Builder::new()
        .prefix("computearena-install-")
        .tempdir()
        .context("creating a temporary download directory")?;
    let (archive, asset_name) = match &plan.source {
        ArchiveSource::Release(asset) => {
            let started = start_activity(ui, format!("Downloading {}", asset.name));
            let downloaded = download(ui, asset, staging.path())?;
            finish_activity(
                ui,
                started,
                format!(
                    "Downloaded {}",
                    megabytes(fs::metadata(&downloaded.file)?.len())
                ),
            );
            if let Some(url) = &asset.checksum_url {
                let expected = fetch_checksum(url)?;
                if expected != downloaded.sha256 {
                    bail!(
                        "the download does not match the published SHA-256 (expected {expected}, got {}); nothing was installed",
                        downloaded.sha256
                    );
                }
                println!("{} SHA-256 matches the release", ui.success("✓"));
            }
            (downloaded.file, asset.name.clone())
        }
        ArchiveSource::Local(archive) => (
            archive.clone(),
            archive
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default(),
        ),
    };

    let started = start_activity(
        ui,
        format!("Unpacking into {}", compact_path(&plan.destination)),
    );
    match plan.runtime {
        Runtime::Basert => {
            fs::create_dir_all(&plan.destination)
                .with_context(|| format!("creating {}", plan.destination.display()))?;
            clear_basert_bundle(&plan.destination)?;
        }
        Runtime::LlamaCpp => {
            if plan.destination.exists() {
                fs::remove_dir_all(&plan.destination)
                    .with_context(|| format!("replacing {}", plan.destination.display()))?;
            }
        }
    }
    let executable = extract_archive(&archive, &plan.destination, adapter.binary_name())?;
    if plan.runtime == Runtime::Basert {
        for entry in fs::read_dir(&plan.destination)?.flatten() {
            let path = entry.path();
            if path.is_file() && entry.file_name().to_string_lossy().starts_with("basert") {
                mark_executable(&path)?;
            }
        }
        // The official installer's stamp, so it knows this release is present.
        fs::write(
            plan.destination.join(".release"),
            format!("v{}\n", plan.version),
        )?;
    } else {
        mark_executable(&executable)?;
        if let Some(parent) = plan.destination.parent() {
            for entry in fs::read_dir(parent).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() && path != plan.destination {
                    let _ = fs::remove_dir_all(&path);
                }
            }
        }
    }
    finish_activity(ui, started, format!("Unpacked {}", adapter.display_name()));

    InstallRecord {
        executable: executable.clone(),
        version: plan.version.clone(),
        asset: asset_name,
        installed_at_unix_ms: unix_ms(),
    }
    .save(paths, plan.runtime)?;
    Ok(executable)
}

/// The `install` action and the interactive "install it for me" option share
/// this: show the plan, ask unless told not to, install, and prove it runs.
pub(crate) fn install(
    runtime: Runtime,
    paths: &Paths,
    ui: TerminalUi,
    yes: bool,
    archive: Option<PathBuf>,
) -> Result<Option<Located>> {
    let started = start_activity(ui, "Looking up the current release…");
    let plan = plan_install(runtime, paths, archive)?;
    finish_activity(
        ui,
        started,
        match &plan.source {
            ArchiveSource::Release(_) => format!(
                "Latest release: {} {}",
                runtime.adapter().display_name(),
                plan.version
            ),
            ArchiveSource::Local(_) => format!(
                "Local bundle: {} {}",
                runtime.adapter().display_name(),
                plan.version
            ),
        },
    );
    print_plan(ui, &plan);
    if !yes {
        if !io::stdin().is_terminal() {
            bail!(
                "installation needs a terminal to confirm; pass --yes to install without prompts"
            );
        }
        if !prompt_yes_no("Download and install?", false)? {
            println!("Nothing was installed.");
            return Ok(None);
        }
    }
    let executable = perform_install(ui, paths, &plan)?;
    let located = Located {
        path: executable,
        source: Source::Managed,
    };
    report_found(ui, runtime, &located)?;
    Ok(Some(located))
}

// --- Feedback -----------------------------------------------------------------

/// Print the executable that will run, and check it is usable before anyone
/// picks a model. Returns the version when the runtime reports one.
pub(crate) fn report_found(ui: TerminalUi, runtime: Runtime, located: &Located) -> Result<()> {
    let adapter = runtime.adapter();
    let capabilities = adapter.probe(&located.path).with_context(|| {
        format!(
            "{} is not a usable {}",
            located.path.display(),
            adapter.display_name()
        )
    })?;
    let version = capabilities
        .pointer("/runtime/version")
        .and_then(Value::as_str)
        .map(|version| format!(" {version}"))
        .unwrap_or_default();
    println!(
        "{} Found {}{} {}",
        ui.success("✓"),
        ui.strong(adapter.display_name()),
        ui.strong(version),
        ui.muted(located.source.describe()),
    );
    println!("  {}", ui.neutral(compact_path(&located.path)));
    Ok(())
}

pub(crate) enum RuntimeSetup {
    Ready(PathBuf),
    Skipped,
    Back,
}

/// Selecting a runtime in the interactive session ends here: either its
/// executable is shown, or the visitor is told how to get it and offered an
/// installation. `Back` returns to the runtime choice.
pub(crate) fn ensure_runtime(
    runtime: Runtime,
    paths: &Paths,
    override_path: Option<PathBuf>,
    ui: TerminalUi,
) -> Result<RuntimeSetup> {
    let adapter = runtime.adapter();
    loop {
        let problem = match locate(runtime, override_path.clone(), paths) {
            Ok(located) => match report_found(ui, runtime, &located) {
                Ok(()) => return Ok(RuntimeSetup::Ready(located.path)),
                Err(error) => format!("{error:#}"),
            },
            Err(error) => format!("{error:#}"),
        };
        println!();
        println!(
            "{} {}",
            ui.error("✗"),
            problem.lines().next().unwrap_or_default()
        );
        println!();
        println!(
            "{}",
            ui.strong(format!(
                "How to install {} yourself",
                adapter.display_name()
            ))
        );
        for line in manual_instructions(runtime) {
            println!("  {}", ui.neutral(line));
        }
        println!();
        let items = [
            MenuItem::new(format!(
                "Install {} with ComputeArena",
                adapter.display_name()
            ))
            .detail(match runtime {
                Runtime::Basert => {
                    "Downloads the latest release bundle to the official install location"
                        .to_string()
                }
                Runtime::LlamaCpp => {
                    "Downloads the latest prebuilt build into ComputeArena's data directory"
                        .to_string()
                }
            }),
            MenuItem::new("Check again").detail("After installing it yourself"),
            MenuItem::new("Continue without it")
                .detail("Log in, submit, list, and verify still work"),
        ];
        match choose(ui, "Choose an option: ", &items, Some("Back"))? {
            MenuChoice::Item(0) => {
                if let Err(error) = install(runtime, paths, ui, false, None) {
                    eprintln!("{} {error:#}", ui.error("Installation failed:"));
                }
            }
            MenuChoice::Item(1) => {}
            MenuChoice::Item(_) => return Ok(RuntimeSetup::Skipped),
            MenuChoice::Escape => return Ok(RuntimeSetup::Back),
        }
        println!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_x86_64_explains_that_basert_has_no_prebuilt_runtime() {
        let notice = platform_notice_for(Runtime::Basert, "linux", "x86_64").unwrap();
        assert!(notice.contains("does not currently publish"));
        assert!(notice.contains("llama.cpp"));
        assert!(notice.contains("built yourself"));
        assert!(platform_notice_for(Runtime::LlamaCpp, "linux", "x86_64").is_none());
        assert!(platform_notice_for(Runtime::Basert, "linux", "aarch64").is_none());
    }

    fn llama_releases() -> Vec<Value> {
        vec![
            json!({"tag_name":"v0.4.0","draft":false,"assets":[{"name":"nightly-tag.txt","browser_download_url":"https://x/nightly-tag.txt","size":7}]}),
            json!({"tag_name":"b10884","draft":true,"assets":[{"name":"llama-b10884-bin-macos-arm64.tar.gz","browser_download_url":"https://x/draft","size":1}]}),
            json!({"tag_name":"b10883","draft":false,"published_at":"2026-09-09T17:29:28Z","assets":[
                {"name":"llama-b10883-bin-macos-x64.tar.gz","browser_download_url":"https://x/macos-x64","size":11193752},
                {"name":"llama-b10883-bin-macos-arm64.tar.gz","browser_download_url":"https://x/macos-arm64","size":11141038},
                {"name":"llama-b10883-bin-ubuntu-vulkan-x64.tar.gz","browser_download_url":"https://x/vulkan","size":30161304},
                {"name":"llama-b10883-bin-ubuntu-x64.tar.gz","browser_download_url":"https://x/ubuntu-x64","size":16813862},
                {"name":"llama-b10883-bin-win-cpu-x64.zip","browser_download_url":"https://x/win","size":18425091}
            ]}),
        ]
    }

    fn basert_release() -> Vec<Value> {
        vec![
            json!({"tag_name":"v0.2.4","draft":false,"published_at":"2026-09-09T13:30:21Z","assets":[
                {"name":"basert-engine-linux-arm64-cuda-0.2.4.tar.gz","browser_download_url":"https://x/linux","size":38257156},
                {"name":"basert-engine-linux-arm64-cuda-0.2.4.tar.gz.sha256","browser_download_url":"https://x/linux.sha256","size":110},
                {"name":"basert-engine-macos-arm64-0.2.4.tar.gz","browser_download_url":"https://x/macos","size":18544875},
                {"name":"basert-engine-macos-arm64-0.2.4.tar.gz.sha256","browser_download_url":"https://x/macos.sha256","size":105}
            ]}),
        ]
    }

    #[test]
    fn llama_cpp_assets_come_from_the_newest_published_build_for_the_platform() {
        let asset = select_asset(Runtime::LlamaCpp, &llama_releases(), "macos", "aarch64").unwrap();
        assert_eq!(asset.tag, "b10883");
        assert_eq!(asset.name, "llama-b10883-bin-macos-arm64.tar.gz");
        assert_eq!(asset.url, "https://x/macos-arm64");
        assert_eq!(asset.checksum_url, None);
        assert_eq!(asset.published_at.as_deref(), Some("2026-09-09T17:29:28Z"));
        let linux = select_asset(Runtime::LlamaCpp, &llama_releases(), "linux", "x86_64").unwrap();
        assert_eq!(linux.name, "llama-b10883-bin-ubuntu-x64.tar.gz");
        assert!(asset_rule(Runtime::LlamaCpp, "linux", "x86_64")
            .unwrap()
            .backend
            .contains("CPU only"));
        let windows = select_asset(Runtime::LlamaCpp, &llama_releases(), "windows", "x86_64");
        assert!(windows
            .unwrap_err()
            .to_string()
            .contains(LLAMA_CPP_RELEASES));
    }

    #[test]
    fn basert_assets_pair_with_their_published_checksum() {
        let asset = select_asset(Runtime::Basert, &basert_release(), "macos", "aarch64").unwrap();
        assert_eq!(asset.tag, "v0.2.4");
        assert_eq!(asset.name, "basert-engine-macos-arm64-0.2.4.tar.gz");
        assert_eq!(
            asset.checksum_url.as_deref(),
            Some("https://x/macos.sha256")
        );
        let linux = select_asset(Runtime::Basert, &basert_release(), "linux", "aarch64").unwrap();
        assert_eq!(linux.name, "basert-engine-linux-arm64-cuda-0.2.4.tar.gz");
        let intel = select_asset(Runtime::Basert, &basert_release(), "linux", "x86_64");
        assert!(intel.unwrap_err().to_string().contains(BASERT_RELEASES));
    }

    #[test]
    fn checksums_and_archive_versions_are_parsed_from_release_conventions() {
        assert_eq!(
            parse_checksum("ba96faa7d16d5862d5ef203e1552359259653e659a38adbb308eb00e4dbb3ebc  basert-engine-macos-arm64-0.2.4.tar.gz\n").as_deref(),
            Some("ba96faa7d16d5862d5ef203e1552359259653e659a38adbb308eb00e4dbb3ebc")
        );
        assert_eq!(parse_checksum("not a digest"), None);
        assert_eq!(
            version_from_archive_name(Runtime::LlamaCpp, "llama-b10883-bin-macos-arm64.tar.gz")
                .as_deref(),
            Some("b10883")
        );
        assert_eq!(
            version_from_archive_name(Runtime::Basert, "basert-engine-macos-arm64-0.2.4.tar.gz")
                .as_deref(),
            Some("0.2.4")
        );
        assert_eq!(
            version_from_archive_name(Runtime::LlamaCpp, "custom.zip"),
            None
        );
    }

    #[test]
    fn install_records_round_trip_and_missing_records_mean_nothing_installed() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = Paths::resolve(Some(temporary.path().to_path_buf())).unwrap();
        assert!(InstallRecord::load(&paths, Runtime::LlamaCpp)
            .unwrap()
            .is_none());
        let record = InstallRecord {
            executable: temporary.path().join("llama-bench"),
            version: "b10883".to_string(),
            asset: "llama-b10883-bin-macos-arm64.tar.gz".to_string(),
            installed_at_unix_ms: 1_757_000_000_000,
        };
        record.save(&paths, Runtime::LlamaCpp).unwrap();
        assert_eq!(
            InstallRecord::load(&paths, Runtime::LlamaCpp).unwrap(),
            Some(record)
        );
        assert!(InstallRecord::load(&paths, Runtime::Basert)
            .unwrap()
            .is_none());
    }

    #[cfg(unix)]
    #[test]
    fn archives_unpack_into_the_destination_and_refuse_to_escape_it() {
        use std::os::unix::fs::PermissionsExt;
        let temporary = tempfile::tempdir().unwrap();
        let archive = temporary.path().join("llama-b1-bin-macos-arm64.tar.gz");
        {
            let file = File::create(&archive).unwrap();
            let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
            let mut builder = tar::Builder::new(encoder);
            let script = b"#!/bin/sh\necho hi\n";
            let mut header = tar::Header::new_gnu();
            header.set_size(script.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, "llama-b1/llama-bench", &script[..])
                .unwrap();
            let mut header = tar::Header::new_gnu();
            header.set_size(2);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "llama-b1/libllama.dylib", &b"xx"[..])
                .unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }
        let destination = temporary
            .path()
            .join("runtimes")
            .join("llama-cpp")
            .join("b1");
        let executable = extract_archive(&archive, &destination, "llama-bench").unwrap();
        assert_eq!(executable, destination.join("llama-b1").join("llama-bench"));
        assert_eq!(
            fs::metadata(&executable).unwrap().permissions().mode() & 0o111,
            0o111
        );
        assert!(destination
            .join("llama-b1")
            .join("libllama.dylib")
            .is_file());

        let hostile = temporary.path().join("hostile.tar.gz");
        {
            let file = File::create(&hostile).unwrap();
            let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
            let mut builder = tar::Builder::new(encoder);
            // The writer refuses `..` paths, so write the name bytes directly to
            // build what a hostile archive would carry.
            let mut header = tar::Header::new_gnu();
            header.set_size(2);
            header.set_mode(0o644);
            let name = b"../escaped";
            header.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name);
            header.set_cksum();
            builder.append(&header, &b"xx"[..]).unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }
        let elsewhere = temporary.path().join("elsewhere");
        assert!(extract_archive(&hostile, &elsewhere, "llama-bench").is_err());
        assert!(!temporary.path().join("escaped").exists());
    }

    #[test]
    fn destinations_follow_each_runtime_s_convention() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = Paths::resolve(Some(temporary.path().to_path_buf())).unwrap();
        assert_eq!(
            destination(Runtime::LlamaCpp, &paths, "b10883").unwrap(),
            temporary
                .path()
                .join("runtimes")
                .join("llama-cpp")
                .join("b10883")
        );
        let basert = destination(Runtime::Basert, &paths, "0.2.4").unwrap();
        assert!(basert.ends_with(".basert") || std::env::var_os("BASERT_INSTALL_DIR").is_some());
    }

    #[test]
    fn manual_instructions_name_the_official_channels() {
        let basert = manual_instructions(Runtime::Basert).join("\n");
        if let Some(notice) = platform_notice(Runtime::Basert) {
            assert!(basert.contains(&notice));
            assert!(!basert.contains(BASERT_INSTALL_SCRIPT));
        } else {
            assert!(basert.contains(BASERT_INSTALL_SCRIPT));
        }
        assert!(basert.contains("--runtime-path"));
        let llama = manual_instructions(Runtime::LlamaCpp).join("\n");
        assert!(llama.contains("brew install llama.cpp"));
        assert!(llama.contains(LLAMA_CPP_RELEASES));
    }
}

#[cfg(test)]
mod install_record_tests {
    use super::*;

    #[test]
    fn a_record_from_the_old_data_directory_still_finds_the_moved_executable() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::resolve(Some(dir.path().join("computearena"))).unwrap();
        let executable = paths
            .root
            .join("runtimes")
            .join("llama-cpp")
            .join("b1")
            .join("llama-bench");
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        fs::write(&executable, b"#!/bin/sh\n").unwrap();
        let stale = dir
            .path()
            .join("basert")
            .join("computearena")
            .join("runtimes")
            .join("llama-cpp")
            .join("b1")
            .join("llama-bench");
        InstallRecord {
            executable: stale,
            version: "b1".to_string(),
            asset: String::new(),
            installed_at_unix_ms: 0,
        }
        .save(&paths, Runtime::LlamaCpp)
        .unwrap();

        let loaded = InstallRecord::load(&paths, Runtime::LlamaCpp)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.executable, executable);
    }

    #[test]
    fn a_record_whose_executable_is_gone_is_returned_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::resolve(Some(dir.path().join("computearena"))).unwrap();
        let missing = dir.path().join("elsewhere").join("llama-bench");
        InstallRecord {
            executable: missing.clone(),
            version: "b1".to_string(),
            asset: String::new(),
            installed_at_unix_ms: 0,
        }
        .save(&paths, Runtime::LlamaCpp)
        .unwrap();
        let loaded = InstallRecord::load(&paths, Runtime::LlamaCpp)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.executable, missing);
    }
}
