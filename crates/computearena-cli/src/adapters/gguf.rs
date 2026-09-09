//! Bounded GGUF metadata reader; tensor payloads are never loaded.
use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

fn integer(file: &mut File, bytes: usize) -> Result<u64> {
    let mut buf = [0_u8; 8];
    file.read_exact(&mut buf[..bytes])?;
    Ok(u64::from_le_bytes(buf))
}
fn string(file: &mut File) -> Result<String> {
    let len = integer(file, 8)?;
    if len > 1024 * 1024 {
        bail!("GGUF metadata string exceeds 1 MiB");
    }
    let mut bytes = vec![0; len as usize];
    file.read_exact(&mut bytes)?;
    String::from_utf8(bytes).context("GGUF metadata is not UTF-8")
}
fn value(file: &mut File, kind: u64, depth: usize) -> Result<Value> {
    if depth > 4 {
        bail!("GGUF metadata is too deeply nested");
    }
    match kind {
        0 | 1 | 7 => Ok(json!(integer(file, 1)?)),
        2 | 3 => Ok(json!(integer(file, 2)?)),
        4 | 5 => Ok(json!(integer(file, 4)?)),
        10 | 11 => Ok(json!(integer(file, 8)?)),
        6 => {
            file.seek(SeekFrom::Current(4))?;
            Ok(Value::Null)
        }
        12 => {
            file.seek(SeekFrom::Current(8))?;
            Ok(Value::Null)
        }
        8 => Ok(json!(string(file)?)),
        9 => {
            let child = integer(file, 4)?;
            let count = integer(file, 8)?;
            if count > 1_000_000 {
                bail!("GGUF metadata array exceeds limit");
            }
            for _ in 0..count {
                value(file, child, depth + 1)?;
                if file.stream_position()? > 64 * 1024 * 1024 {
                    bail!("GGUF metadata exceeds 64 MiB");
                }
            }
            Ok(Value::Null)
        }
        _ => bail!("unknown GGUF metadata type {kind}"),
    }
}
pub(crate) fn inspect(path: &Path) -> Result<Value> {
    let mut file = File::open(path)?;
    if integer(&mut file, 4)? != u32::from_le_bytes(*b"GGUF") as u64 {
        bail!("not a GGUF model");
    }
    let version = integer(&mut file, 4)?;
    if !matches!(version, 2 | 3) {
        bail!("unsupported GGUF version {version}");
    }
    let _tensors = integer(&mut file, 8)?;
    let count = integer(&mut file, 8)?;
    if count > 100_000 {
        bail!("too many GGUF metadata entries");
    }
    let mut metadata = Map::new();
    for _ in 0..count {
        let key = string(&mut file)?;
        let kind = integer(&mut file, 4)?;
        let item = value(&mut file, kind, 0)?;
        if matches!(
            key.as_str(),
            "general.name" | "general.architecture" | "general.file_type"
        ) {
            metadata.insert(key, item);
        }
        if file.stream_position()? > 64 * 1024 * 1024 {
            bail!("GGUF metadata exceeds 64 MiB");
        }
    }
    let file_type = metadata.get("general.file_type").and_then(Value::as_u64);
    let quant = match file_type {
        Some(0) => "F32",
        Some(1) => "F16",
        Some(2) => "Q4_0",
        Some(3) => "Q4_1",
        Some(7) => "Q8_0",
        Some(8) => "Q5_0",
        Some(9) => "Q5_1",
        Some(10) => "Q2_K",
        Some(11) => "Q3_K_S",
        Some(12) => "Q3_K_M",
        Some(13) => "Q3_K_L",
        Some(14) => "Q4_K_S",
        Some(15) => "Q4_K_M",
        Some(16) => "Q5_K_S",
        Some(17) => "Q5_K_M",
        Some(18) => "Q6_K",
        Some(32) => "BF16",
        _ => "unknown",
    };
    Ok(json!({
        "name": metadata.get("general.name").and_then(Value::as_str)
            .or_else(|| path.file_stem().and_then(|s| s.to_str())).unwrap_or("Unknown model"),
        "architecture": metadata.get("general.architecture"),
        "quantization": quant, "gguf_file_type": file_type,
        "format": "gguf", "format_version": version,
        "file_name": path.file_name().and_then(|s| s.to_str()),
        "size_bytes": file.metadata()?.len()
    }))
}
