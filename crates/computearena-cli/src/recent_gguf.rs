//! Local picker history, deliberately separate from signed/uploadable reports.
use crate::reports::{set_private_permissions, Paths};
use crate::ui::{prompt, TerminalUi};
use anyhow::{Context, Result};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const HISTORY_FILE: &str = "recent-gguf.json";
const MAX_RECENT: usize = 10;
const MAX_HISTORY_BYTES: u64 = 128 * 1024;

fn load(paths: &Paths) -> Result<Vec<PathBuf>> {
    let file = match fs::File::open(paths.root.join(HISTORY_FILE)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_HISTORY_BYTES + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_HISTORY_BYTES,
        "history file is too large"
    );
    let entries: Vec<PathBuf> =
        serde_json::from_slice(&bytes).context("invalid recent GGUF history")?;
    let mut recent = Vec::new();
    for path in entries {
        if path.is_absolute() && !recent.contains(&path) {
            recent.push(path);
            if recent.len() == MAX_RECENT {
                break;
            }
        }
    }
    Ok(recent)
}

/// Only called after saving a successful benchmark, including noninteractive runs.
pub(crate) fn remember(paths: &Paths, model: &Path) -> Result<()> {
    let model = fs::canonicalize(model).context("resolving recent GGUF path")?;
    let mut recent = load(paths)?;
    recent.retain(|path| path != &model);
    recent.insert(0, model);
    recent.truncate(MAX_RECENT);
    fs::create_dir_all(&paths.root)?;
    let mut temp = tempfile::NamedTempFile::new_in(&paths.root)?;
    set_private_permissions(temp.as_file())?;
    serde_json::to_writer_pretty(&mut temp, &recent)?;
    temp.write_all(b"\n")?;
    temp.as_file().sync_all()?;
    temp.persist(paths.root.join(HISTORY_FILE))
        .map_err(|error| error.error)?;
    Ok(())
}

pub(crate) fn select(
    paths: &Paths,
    validate: impl Fn(&Path) -> Result<()>,
) -> Result<Option<PathBuf>> {
    let ui = TerminalUi::detect();
    let recent = load(paths).unwrap_or_else(|error| {
        eprintln!(
            "{} Could not read recent GGUF files: {error:#}. Enter a path instead.",
            ui.warning("!")
        );
        Vec::new()
    });
    if !recent.is_empty() {
        ui.section("Recently used GGUF models");
        println!(
            "{}",
            ui.muted("Most recent first · successful benchmarks · history stays on this device")
        );
        for (index, path) in recent.iter().enumerate() {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            let missing = if path.is_file() {
                ""
            } else {
                " [missing or unavailable]"
            };
            println!(
                "  {} {}{missing}",
                ui.brand_bold(format!("{}.", index + 1)),
                display_safe(&name)
            );
            println!("     {}", ui.muted(display_safe(&path.to_string_lossy())));
        }
        println!("  {} Enter another GGUF path", ui.brand_bold("p."));
        println!("  {} Back", ui.brand_bold("0."));
    }
    loop {
        let input = if recent.is_empty() {
            prompt_path()?
        } else {
            prompt("Choose a recent model number, p for another path, or 0 to go back: ")?
        };
        if input.is_empty() || input == "0" {
            return Ok(None);
        }
        let path = if !recent.is_empty() {
            if let Ok(number) = input.parse::<usize>() {
                let Some(path) = number.checked_sub(1).and_then(|i| recent.get(i)) else {
                    println!(
                        "{} Choose a number from 1 to {} or p for another path.",
                        ui.warning("!"),
                        recent.len()
                    );
                    continue;
                };
                path.clone()
            } else if input.eq_ignore_ascii_case("p") {
                let input = prompt_path()?;
                if input.is_empty() || input == "0" {
                    continue;
                }
                expand_path(&input)?
            } else {
                expand_path(&input)?
            }
        } else {
            expand_path(&input)?
        };
        if let Err(error) = validate(&path) {
            if recent.is_empty() {
                return Err(error);
            }
            eprintln!("{} Cannot use this GGUF file: {error:#}. Select another entry or enter its new path.", ui.warning("!"));
            continue;
        }
        return Ok(Some(path));
    }
}

fn prompt_path() -> Result<String> {
    prompt("GGUF model path (absolute, relative, or ~/; 0 to go back): ")
}

fn expand_path(input: &str) -> Result<PathBuf> {
    Ok(if let Some(rest) = input.strip_prefix("~/") {
        dirs::home_dir()
            .context("cannot locate home directory")?
            .join(rest)
    } else {
        PathBuf::from(input)
    })
}

fn display_safe(value: &str) -> String {
    let mut display = String::new();
    for c in value.chars() {
        if c.is_control() {
            display.extend(c.escape_default());
        } else {
            display.push(c);
        }
    }
    display
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_is_bounded_deduplicated_canonical_and_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::resolve(Some(dir.path().join("data"))).unwrap();
        assert!(load(&paths).unwrap().is_empty());
        for index in 0..12 {
            let model = dir.path().join(format!("{index}.gguf"));
            fs::write(&model, b"GGUF").unwrap();
            remember(&paths, &model).unwrap();
        }
        let model = dir.path().join("./5.gguf");
        remember(&paths, &model).unwrap();
        let recent = load(&paths).unwrap();
        assert_eq!(recent.len(), MAX_RECENT);
        assert_eq!(recent[0], fs::canonicalize(model).unwrap());
        assert!(recent[1].ends_with("11.gguf"));
        assert!(!recent.iter().any(|p| p.ends_with("0.gguf")));
        let other = Paths::resolve(Some(dir.path().join("other"))).unwrap();
        assert!(load(&other).unwrap().is_empty());
        assert!(!paths.reports.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(paths.root.join(HISTORY_FILE))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn corrupt_history_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::resolve(Some(dir.path().to_owned())).unwrap();
        let history = paths.root.join(HISTORY_FILE);
        fs::write(&history, b"broken").unwrap();
        let model = dir.path().join("model.gguf");
        fs::write(&model, b"GGUF").unwrap();
        assert!(load(&paths).is_err());
        assert!(remember(&paths, &model).is_err());
        assert_eq!(fs::read(history).unwrap(), b"broken");
        assert_eq!(
            display_safe("model\x1b[31m\n.gguf"),
            "model\\u{1b}[31m\\n.gguf"
        );
    }
}
