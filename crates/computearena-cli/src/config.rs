use std::time::Duration;

pub(crate) const DEFAULT_API_URL: &str = "https://computearena.ai/api/v1";

pub(crate) const DEFAULT_PREFILL_TOKENS: &str = "128,256,512,1024,2048";
pub(crate) const DEFAULT_DECODE_TOKENS: u32 = 128;
pub(crate) const DEFAULT_REPETITIONS: u32 = 3;
pub(crate) const DEFAULT_WARMUP_REPETITIONS: u32 = 3;

pub(crate) const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const AUTH_HTTP_TIMEOUT: Duration = Duration::from_secs(15);
pub(crate) const DEFAULT_DEVICE_AUTH_POLL_INTERVAL_SECS: u64 = 3;
pub(crate) const MIN_DEVICE_AUTH_POLL_INTERVAL_SECS: u64 = 1;
pub(crate) const SUBMISSION_HTTP_TIMEOUT: Duration = Duration::from_secs(60);
pub(crate) const DEFAULT_DEVICE_AUTH_EXPIRES_SECS: u64 = 600;

pub(crate) const MODEL_SELECTOR_VISIBLE_ROWS: usize = 10;
pub(crate) const MODEL_ID_COLUMN_WIDTH: usize = 42;
pub(crate) const MODEL_VARIANT_COLUMN_WIDTH: usize = 20;
pub(crate) const MODEL_QUANT_COLUMN_WIDTH: usize = 12;

pub(crate) const PRIVATE_FILE_MODE: u32 = 0o600;

pub(crate) const BASECOMPUTE_WEBSITE: &str = "https://basecompute.co";
pub(crate) const BASECOMPUTE_DISCORD: &str = "https://discord.gg/tB9YFTKZUV";

pub(crate) const PRIMARY_HARNESS_NAME: &str = "basert-harness";
pub(crate) const LEGACY_HARNESS_NAME: &str = "baseRT_bench_multidevice";
pub(crate) const DEVELOPMENT_HARNESS_PATHS: [&str; 2] =
    ["build/basert-harness", "build/baseRT_bench_multidevice"];
