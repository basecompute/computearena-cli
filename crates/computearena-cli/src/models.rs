use crate::config::{MODEL_ID_COLUMN_WIDTH, MODEL_QUANT_COLUMN_WIDTH, MODEL_VARIANT_COLUMN_WIDTH};
use crate::theme::selector_theme;
use crate::ui::{finish_activity, prompt, start_activity, visible_rows, TerminalUi};
use anyhow::{bail, Context, Result};

use dialoguer::FuzzySelect;
use serde_json::{json, Value};
use std::fs::{self, File};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

const BASERT_REMOTE_MODELS_COMMAND: &str = "basert list --remote";
const BASERT_PULL_MODEL_COMMAND: &str = "basert pull <model-id>";

#[derive(Clone, Debug)]
pub(crate) struct InstalledModel {
    pub(crate) path: PathBuf,
    pub(crate) id: String,
    pub(crate) variant: String,
    pub(crate) architecture: String,
    pub(crate) quantization: String,
}
pub(crate) fn inspect_model(path: &Path) -> Result<Value> {
    let file = fs::metadata(path)
        .with_context(|| format!("reading model metadata for {}", path.display()))?;
    let header = read_base_header(path)
        .with_context(|| format!("reading BaseRT model header from {}", path.display()))?;
    let fallback_name = fallback_model_name(path);
    let mut model = json!({
        "name": fallback_name,
        "file_name": path.file_name().and_then(|name| name.to_str()).unwrap_or("unknown"),
        "size_bytes": file.len(),
        "format_schema": required_header_u64(&header, "schema")?,
        "architecture": required_header_string(&header, "arch")?,
        "quantization": required_header_string(&header, "quant_scheme")?,
        "quant_profile": header.get("quant_profile").and_then(Value::as_str),
        "target_backend": header.get("target_backend").and_then(Value::as_str),
        "source_sha256": header.pointer("/source/sha256").and_then(Value::as_str)
    });
    if let Some((id, variant)) = model_identity_from_path(path)? {
        model["name"] = json!(id);
        model["id"] = json!(id);
        model["variant"] = json!(variant);
    }
    Ok(model)
}

fn read_base_header(path: &Path) -> Result<Value> {
    const PREFIX_BYTES: usize = 16;
    const MAX_HEADER_BYTES: u64 = 64 * 1024 * 1024;

    let mut file = File::open(path)?;
    let mut prefix = [0_u8; PREFIX_BYTES];
    file.read_exact(&mut prefix)
        .context("model is smaller than the BaseRT header prefix")?;
    if &prefix[0..4] != b"BASE" {
        bail!("invalid BaseRT model magic");
    }
    let version = u32::from_le_bytes(prefix[4..8].try_into().unwrap());
    if version != 1 {
        bail!("unsupported BaseRT model format version {version}");
    }
    let header_len = u64::from_le_bytes(prefix[8..16].try_into().unwrap());
    if header_len > MAX_HEADER_BYTES {
        bail!("BaseRT model header exceeds {MAX_HEADER_BYTES} bytes");
    }
    let mut bytes = vec![0_u8; usize::try_from(header_len)?];
    file.read_exact(&mut bytes)
        .context("model contains a truncated BaseRT JSON header")?;
    serde_json::from_slice(&bytes).context("parsing BaseRT model JSON header")
}

fn required_header_string<'a>(header: &'a Value, field: &str) -> Result<&'a str> {
    header
        .get(field)
        .and_then(Value::as_str)
        .with_context(|| format!("BaseRT model header is missing {field}"))
}

fn required_header_u64(header: &Value, field: &str) -> Result<u64> {
    header
        .get(field)
        .and_then(Value::as_u64)
        .with_context(|| format!("BaseRT model header is missing {field}"))
}

pub(crate) fn fallback_model_name(path: &Path) -> String {
    let file_name = path.file_name().and_then(|name| name.to_str());
    if file_name == Some("model.base") {
        return path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .or_else(|| path.parent().and_then(Path::file_name))
            .and_then(|name| name.to_str())
            .unwrap_or("Unknown model")
            .to_string();
    }
    path.file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("Unknown model")
        .to_string()
}
/// What to call a model on screen. Installed BaseRT models are all stored as
/// `<id>/<variant>/model.base`, so the file name alone names every one of them
/// the same thing; the identity in the path is what people recognise.
pub(crate) fn display_name(path: &Path) -> String {
    match model_identity_from_path(path) {
        Ok(Some((id, variant))) => format!("{id}/{variant}"),
        _ => fallback_model_name(path),
    }
}

pub(crate) fn prompt_model_path() -> Result<Option<PathBuf>> {
    let ui = TerminalUi::detect();
    let started = start_activity(ui, "Scanning installed BaseRT model metadata…");
    let installed = installed_models()?;
    finish_activity(
        ui,
        started,
        format!("Found {} compatible model(s)", installed.len()),
    );
    if installed.is_empty() {
        print_model_acquisition_help(ui, true);
        return model_path_from_input(prompt("Model path: ")?).map(Some);
    }

    print_model_acquisition_help(ui, false);
    if io::stdin().is_terminal() && io::stderr().is_terminal() {
        prompt_model_path_interactive(&installed, ui)
    } else {
        prompt_model_path_numbered(&installed, ui)
    }
}

fn print_model_acquisition_help(ui: TerminalUi, no_models_installed: bool) {
    println!();
    if no_models_installed {
        println!(
            "{}",
            ui.neutral("No compatible BaseRT text models are installed yet.")
        );
    } else {
        println!("{}", ui.neutral("Need another BaseRT model?"));
    }
    println!(
        "  {}  {}",
        ui.neutral("Browse"),
        ui.accent_bold(BASERT_REMOTE_MODELS_COMMAND)
    );
    println!(
        "  {}    {}",
        ui.neutral("Pull"),
        ui.accent_bold(BASERT_PULL_MODEL_COMMAND)
    );
    println!(
        "{}",
        ui.muted("Run this step again after pulling to refresh the list.")
    );
}

fn prompt_model_path_interactive(
    installed: &[InstalledModel],
    ui: TerminalUi,
) -> Result<Option<PathBuf>> {
    let mut choices = model_choice_labels(installed);
    choices.push("Enter another model path…".to_string());
    println!(
        "{}",
        ui.neutral("Type to filter · ↑/↓ move · Enter select · Esc back")
    );
    io::stdout().flush()?;

    let theme = selector_theme();
    let selected = FuzzySelect::with_theme(&theme)
        .with_prompt(format!("Select a model · {} installed", installed.len()))
        .items(&choices)
        .max_length(visible_rows(choices.len()))
        .report(false)
        .interact_opt()
        .context("reading model selection")?;

    let Some(index) = selected else {
        println!("{} Model selection cancelled", ui.neutral("←"));
        return Ok(None);
    };
    let Some(model) = installed.get(index) else {
        return model_path_from_input(prompt("Model path: ")?).map(Some);
    };
    print_selected_model(model, ui);
    Ok(Some(model.path.clone()))
}

fn prompt_model_path_numbered(
    installed: &[InstalledModel],
    ui: TerminalUi,
) -> Result<Option<PathBuf>> {
    println!("Installed BaseRT models:");
    for (index, label) in model_choice_labels(installed).iter().enumerate() {
        println!("  {} {label}", ui.brand_bold(format!("{}.", index + 1)));
    }
    println!("  {} Enter another model path", ui.brand_bold("p."));
    let input = prompt("Choose a model number or enter a path: ")?;
    if matches!(input.to_ascii_lowercase().as_str(), "q" | "quit" | "back") {
        return Ok(None);
    }
    if let Ok(index) = input.parse::<usize>() {
        let model = index
            .checked_sub(1)
            .and_then(|i| installed.get(i))
            .context("model selection is out of range")?;
        print_selected_model(model, ui);
        return Ok(Some(model.path.clone()));
    }
    if input.eq_ignore_ascii_case("p") {
        return model_path_from_input(prompt("Model path: ")?).map(Some);
    }
    model_path_from_input(input).map(Some)
}

pub(crate) fn model_choice_labels(installed: &[InstalledModel]) -> Vec<String> {
    let id_width = installed
        .iter()
        .map(|model| model.id.chars().count())
        .max()
        .unwrap_or_default()
        .min(MODEL_ID_COLUMN_WIDTH);
    let variant_width = installed
        .iter()
        .map(|model| model.variant.chars().count())
        .max()
        .unwrap_or_default()
        .min(MODEL_VARIANT_COLUMN_WIDTH);
    let quantizations: Vec<String> = installed
        .iter()
        .map(|model| display_quantization(&model.quantization))
        .collect();
    let quant_width = quantizations
        .iter()
        .map(|quantization| quantization.chars().count())
        .max()
        .unwrap_or_default()
        .min(MODEL_QUANT_COLUMN_WIDTH);

    installed
        .iter()
        .zip(quantizations)
        .map(|(model, quantization)| {
            format!(
                "{:<id_width$}  {:<variant_width$}  {:<quant_width$}  {}",
                model.id, model.variant, quantization, model.architecture
            )
        })
        .collect()
}

pub(crate) fn display_quantization(value: &str) -> String {
    value
        .strip_prefix("base_q")
        .filter(|bits| !bits.is_empty() && bits.chars().all(|character| character.is_ascii_digit()))
        .map_or_else(|| value.to_string(), |bits| format!("Q{bits}"))
}

fn print_selected_model(model: &InstalledModel, ui: TerminalUi) {
    println!(
        "\n{} {}/{}",
        ui.success("Selected"),
        model.id,
        model.variant
    );
    println!("  Architecture  {}", model.architecture);
    println!(
        "  Quantisation  {}",
        display_quantization(&model.quantization)
    );
    println!("  Path          {}", compact_home_path(&model.path));
}

pub(crate) fn compact_home_path(path: &Path) -> String {
    dirs::home_dir()
        .and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf))
        .map_or_else(
            || path.display().to_string(),
            |relative| format!("~/{}", relative.display()),
        )
}

fn model_path_from_input(input: String) -> Result<PathBuf> {
    if input.is_empty() {
        bail!("a model path is required");
    }
    Ok(PathBuf::from(input))
}

fn model_cache_root() -> Result<PathBuf> {
    let root = match std::env::var_os("BASERT_MODELS_DIR") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        Some(_) => bail!("BASERT_MODELS_DIR is set but empty"),
        None => dirs::cache_dir()
            .context("could not determine the BaseRT model cache")?
            .join("baseRT")
            .join("models"),
    };
    Ok(root)
}

fn model_identity_from_path(path: &Path) -> Result<Option<(String, String)>> {
    let root = model_cache_root()?;
    let Some(parent) = path.parent() else {
        return Ok(None);
    };
    let Ok(relative) = parent.strip_prefix(root) else {
        return Ok(None);
    };
    let mut components: Vec<String> = relative
        .components()
        .filter_map(|component| component.as_os_str().to_str().map(str::to_string))
        .collect();
    if components.len() < 2 {
        return Ok(None);
    }
    let variant = components.pop().unwrap_or_default();
    Ok(Some((components.join("/"), variant)))
}

pub(crate) fn installed_models() -> Result<Vec<InstalledModel>> {
    let root = model_cache_root()?;
    if !root.is_dir() {
        return Ok(Vec::new());
    }

    let mut directories = vec![root.clone()];
    let mut models = Vec::new();
    while let Some(directory) = directories.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };
            if file_type.is_dir() {
                if path.file_name().and_then(|name| name.to_str()) != Some(".src") {
                    directories.push(path);
                }
            } else if file_type.is_file()
                && path.file_name().and_then(|name| name.to_str()) == Some("model.base")
            {
                let header = match read_base_header(&path) {
                    Ok(header) => header,
                    Err(_) => continue,
                };
                let Some(architecture) = header.get("arch").and_then(Value::as_str) else {
                    continue;
                };
                if architecture == "whisper" {
                    continue;
                }
                let Some(quantization) = header.get("quant_scheme").and_then(Value::as_str) else {
                    continue;
                };
                let Some((id, variant)) = model_identity_from_path(&path)? else {
                    continue;
                };
                models.push(InstalledModel {
                    architecture: architecture.to_string(),
                    quantization: quantization.to_string(),
                    variant,
                    id,
                    path,
                });
            }
        }
    }
    models.sort_by(|left, right| (&left.id, &left.variant).cmp(&(&right.id, &right.variant)));
    Ok(models)
}
