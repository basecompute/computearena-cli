pub(crate) const REPORT_SCHEMA: &str = "computearena-benchmark/1";
pub(crate) const HARNESS_SCHEMA: &str = "basert-benchmark-harness/1";
#[cfg(test)]
pub(crate) const TELEMETRY_SCHEMA: &str = "basert-telemetry/3";
/// A future BaseRT harness must advertise both this schema and
/// `features.same_run_telemetry=true` before ComputeArena delegates telemetry
/// collection to it. Older replay-based harnesses remain on the external
/// observer path.
pub(crate) const BASERT_SAME_RUN_TELEMETRY_SCHEMA: &str = "basert-telemetry/4";
pub(crate) const BASERT_ISOLATED_PROTOCOL_SCHEMA: &str = "basert-throughput-protocol/1";
pub(crate) const THROUGHPUT_PROTOCOL_ID: &str = "computearena-throughput/2";
pub(crate) const HEADLINE_PROTOCOL_ID: &str = "computearena-throughput/3";
pub(crate) const BASERT_CAPACITY_PROTOCOL_SCHEMA: &str = "basert-throughput-protocol/2";
pub(crate) const HEADLINE_CONTEXT_CAPACITY: u64 = 4096;

/// Inputs are validated before execution. Custom sweeps without PP512 keep
/// their first requested PP size instead of introducing an unrequested test.
pub(crate) fn headline_prefill(pp: &str) -> &str {
    pp.split(',')
        .find(|size| *size == "512")
        .unwrap_or_else(|| pp.split(',').next().unwrap_or("512"))
}

pub(crate) fn headline_order(pp: &str, tg: u32) -> Vec<String> {
    let headline = headline_prefill(pp);
    let mut order = vec![format!("pp{headline}"), format!("tg{tg}")];
    order.extend(
        pp.split(',')
            .filter(|size| *size != headline)
            .map(|size| format!("pp{size}")),
    );
    order
}
pub(crate) const DECODE_INITIAL_CONTEXT_TOKENS: u64 = 1;
pub(crate) const SIGNATURE_DOMAIN: &[u8] = b"computearena-benchmark/1\0";

pub(crate) const SIGNATURE_ALGORITHM: &str = "ed25519";
pub(crate) const SIGNATURE_CANONICALIZATION: &str = "computearena-json-v1";
pub(crate) const RUNTIME_NAME: &str = "basert";
