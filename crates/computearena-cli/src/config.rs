use std::time::Duration;

pub(crate) const DEFAULT_API_URL: &str = "https://computearena.ai/api/v1";

pub(crate) const DEFAULT_PREFILL_TOKENS: &str = "128,256,512,1024,2048,4096,8192,16384";
pub(crate) const DEFAULT_DECODE_TOKENS: u32 = 128;
pub(crate) const DEFAULT_REPETITIONS: u32 = 3;
pub(crate) const DEFAULT_WARMUP_REPETITIONS: u32 = 3;

// User-facing constants for ComputeArena's external conditioning protocol.
pub(crate) const CONDITIONING_STABLE_WINDOW_SECONDS: f64 = 10.0;
pub(crate) const CONDITIONING_MAXIMUM_WAIT_SECONDS: f64 = 180.0;
pub(crate) const CONDITIONING_FALLBACK_WAIT_SECONDS: f64 = 30.0;

pub(crate) const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const AUTH_HTTP_TIMEOUT: Duration = Duration::from_secs(15);
pub(crate) const DEFAULT_DEVICE_AUTH_POLL_INTERVAL_SECS: u64 = 3;
pub(crate) const MIN_DEVICE_AUTH_POLL_INTERVAL_SECS: u64 = 1;
pub(crate) const SUBMISSION_HTTP_TIMEOUT: Duration = Duration::from_secs(60);
pub(crate) const DEFAULT_DEVICE_AUTH_EXPIRES_SECS: u64 = 600;

/// Rows a selector leaves for the prompt, hint line, and surrounding output.
pub(crate) const MENU_RESERVED_ROWS: usize = 8;
pub(crate) const MENU_MINIMUM_ROWS: usize = 5;
pub(crate) const MODEL_ID_COLUMN_WIDTH: usize = 42;
pub(crate) const MODEL_VARIANT_COLUMN_WIDTH: usize = 20;
pub(crate) const MODEL_QUANT_COLUMN_WIDTH: usize = 12;

pub(crate) const PRIVATE_FILE_MODE: u32 = 0o600;

pub(crate) const HUGGINGFACE_HOST: &str = "https://huggingface.co";
pub(crate) const HUGGINGFACE_API: &str = "https://huggingface.co/api";

pub(crate) const COMPUTEARENA_WEBSITE: &str = "https://computearena.ai";
pub(crate) const COMPUTEARENA_DISCORD: &str = "https://discord.gg/CCT24GWhPG";

pub(crate) const BASERT_HARNESS_NAME: &str = "basert-benchmark-harness";
