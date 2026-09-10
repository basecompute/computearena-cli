pub(crate) const REPORT_SCHEMA: &str = "computearena-benchmark/1";
pub(crate) const HARNESS_SCHEMA: &str = "basert-benchmark-harness/1";
#[cfg(test)]
pub(crate) const TELEMETRY_SCHEMA: &str = "basert-telemetry/3";
/// A future BaseRT harness must advertise both this schema and
/// `features.same_run_telemetry=true` before ComputeArena delegates telemetry
/// collection to it. Older replay-based harnesses remain on the external
/// observer path.
pub(crate) const BASERT_SAME_RUN_TELEMETRY_SCHEMA: &str = "basert-telemetry/4";
pub(crate) const SIGNATURE_DOMAIN: &[u8] = b"computearena-benchmark/1\0";

pub(crate) const SIGNATURE_ALGORITHM: &str = "ed25519";
pub(crate) const SIGNATURE_CANONICALIZATION: &str = "computearena-json-v1";
pub(crate) const RUNTIME_NAME: &str = "basert";
