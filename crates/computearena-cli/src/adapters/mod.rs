//! Runtime-specific execution behind one report and CLI workflow.
pub(crate) mod basert;
pub(crate) mod chip;
mod gguf;
pub(crate) mod llama_cpp;

use anyhow::{Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Runtime {
    #[default]
    Basert,
    #[value(name = "llama-cpp", alias = "llamacpp")]
    LlamaCpp,
}

impl Runtime {
    pub(crate) fn adapter(self) -> Box<dyn RuntimeAdapter> {
        match self {
            Self::Basert => Box::new(basert::BaseRtAdapter),
            Self::LlamaCpp => Box::new(llama_cpp::LlamaCppAdapter),
        }
    }
}

pub(crate) struct BenchmarkRequest<'a> {
    pub model: &'a Path,
    pub pp: &'a str,
    pub tg: u32,
    pub reps: u32,
    pub warmup: u32,
    pub cooldown: bool,
}

pub(crate) struct RuntimeOutput {
    pub benchmark: Value,
    pub model: Value,
}

pub(crate) trait RuntimeAdapter {
    /// Identifier used in reports and on the command line.
    fn name(&self) -> &'static str;
    /// Name shown to people.
    fn display_name(&self) -> &'static str;
    /// The executable ComputeArena looks for.
    fn binary_name(&self) -> &'static str;
    /// Environment variables that point at the executable, most specific first.
    fn environment_overrides(&self) -> &'static [&'static str];
    /// Conventional install directories searched after PATH.
    fn known_locations(&self) -> Vec<PathBuf>;
    /// Check that the executable speaks this adapter's protocol and describe it.
    fn probe(&self, executable: &Path) -> Result<Value>;
    fn select_model(&self, paths: &crate::reports::Paths) -> Result<Option<PathBuf>>;
    fn confirm(&self, request: &BenchmarkRequest<'_>, yes: bool) -> Result<Option<bool>>;
    fn execute(&self, executable: &Path, request: &BenchmarkRequest<'_>) -> Result<RuntimeOutput>;
}

pub(crate) fn file_sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}
