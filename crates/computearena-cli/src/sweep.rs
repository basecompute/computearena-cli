//! Which benchmark reports ComputeArena accepts.
//!
//! Only complete runs are published: every default prefill size and the
//! default decode length, so every model and chip can be compared at every
//! size. A custom `--pp` or `--tg` still produces a valid signed report for
//! local use; it is just not submittable, and everything that touches such a
//! report says so, says why, and says how to get a submittable one.

use crate::config::{DEFAULT_DECODE_TOKENS, DEFAULT_PREFILL_TOKENS};
use crate::ui::TerminalUi;
use serde_json::Value;

/// What a run lacks against the full default sweep. Extra sizes are fine.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SweepGap {
    pub(crate) missing_prefill: Vec<u32>,
    /// The decode length that was measured instead of the default; zero when
    /// the report carries no decode measurement at all.
    pub(crate) decode: Option<u32>,
}

pub(crate) fn required_prefill() -> Vec<u32> {
    DEFAULT_PREFILL_TOKENS
        .split(',')
        .filter_map(|size| size.trim().parse().ok())
        .collect()
}

/// "PP128 to PP16384 and TG128"
pub(crate) fn required_summary() -> String {
    let required = required_prefill();
    format!(
        "PP{} to PP{} and TG{DEFAULT_DECODE_TOKENS}",
        required.first().copied().unwrap_or_default(),
        required.last().copied().unwrap_or_default()
    )
}

pub(crate) fn sweep_gap(prefill: &[u32], decode: Option<u32>) -> Option<SweepGap> {
    let missing_prefill: Vec<u32> = required_prefill()
        .into_iter()
        .filter(|size| !prefill.contains(size))
        .collect();
    let decode = match decode {
        Some(tokens) if tokens == DEFAULT_DECODE_TOKENS => None,
        other => Some(other.unwrap_or(0)),
    };
    (!missing_prefill.is_empty() || decode.is_some()).then_some(SweepGap {
        missing_prefill,
        decode,
    })
}

/// The gap a `run` invocation is about to produce, from its flags.
pub(crate) fn requested_gap(pp: &str, tg: u32) -> Option<SweepGap> {
    let prefill: Vec<u32> = pp
        .split(',')
        .filter_map(|size| size.trim().parse().ok())
        .collect();
    sweep_gap(&prefill, Some(tg))
}

/// The gap in a saved report, read from the signed samples the server
/// itself recomputes throughput from, with the request parameters as a
/// fallback for a report that records no samples.
pub(crate) fn report_gap(report: &Value) -> Option<SweepGap> {
    let mut prefill: Vec<u32> = report
        .pointer("/benchmark/raw_samples/prefill")
        .and_then(Value::as_object)
        .map(|groups| groups.keys().filter_map(|size| size.parse().ok()).collect())
        .unwrap_or_default();
    if prefill.is_empty() {
        prefill = report
            .pointer("/benchmark/params/pp")
            .and_then(Value::as_str)
            .map(|pp| {
                pp.split(',')
                    .filter_map(|size| size.trim().parse().ok())
                    .collect()
            })
            .unwrap_or_default();
    }
    let decode = report
        .pointer("/benchmark/raw_samples/decode/0/generated_tokens")
        .and_then(Value::as_u64)
        .or_else(|| {
            report
                .pointer("/benchmark/params/tg")
                .and_then(Value::as_u64)
        })
        .and_then(|tokens| u32::try_from(tokens).ok());
    sweep_gap(&prefill, decode)
}

impl SweepGap {
    /// "it is missing PP128, PP256 and PP512 and it measured TG64 instead of TG128"
    pub(crate) fn describe(&self) -> String {
        let mut parts = Vec::new();
        if !self.missing_prefill.is_empty() {
            let sizes: Vec<String> = self
                .missing_prefill
                .iter()
                .map(|size| format!("PP{size}"))
                .collect();
            parts.push(format!("it is missing {}", join_list(&sizes)));
        }
        match self.decode {
            Some(0) => parts.push(format!(
                "it has no TG{DEFAULT_DECODE_TOKENS} decode measurement"
            )),
            Some(tokens) => parts.push(format!(
                "it measured TG{tokens} instead of TG{DEFAULT_DECODE_TOKENS}"
            )),
            None => {}
        }
        parts.join(" and ")
    }

    /// Why a saved report cannot be uploaded, in words a person can act on.
    pub(crate) fn submission_blocker(&self) -> String {
        format!(
            "Partial run, not submittable: {}. ComputeArena accepts only runs with the full default sweep ({}). Run the benchmark again without --pp and --tg to get a submittable report.",
            self.describe(),
            required_summary()
        )
    }
}

/// Shown before a custom sweep starts, so nobody spends a long run on a
/// report they expected to upload.
pub(crate) fn print_local_only_notice(ui: TerminalUi, gap: &SweepGap) {
    println!(
        "{} {}",
        ui.warning("!"),
        ui.strong("This will be a local-only run.")
    );
    println!(
        "  {}",
        ui.neutral(format!(
            "Compared with the default sweep, {}.",
            gap.describe()
        ))
    );
    println!(
        "  {}",
        ui.neutral(format!(
            "ComputeArena accepts only runs with the full default sweep ({}), so this report can be saved, inspected and verified, but not submitted.",
            required_summary()
        ))
    );
    println!(
        "  {}",
        ui.neutral("Omit --pp and --tg to run the default sweep instead.")
    );
}

fn join_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [only] => only.clone(),
        [head @ .., last] => format!("{} and {last}", head.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_default_sweep_is_complete_and_extra_sizes_are_fine() {
        assert_eq!(
            requested_gap(DEFAULT_PREFILL_TOKENS, DEFAULT_DECODE_TOKENS),
            None
        );
        assert_eq!(
            requested_gap(
                &format!("{DEFAULT_PREFILL_TOKENS},32768"),
                DEFAULT_DECODE_TOKENS
            ),
            None
        );
        assert_eq!(required_summary(), "PP128 to PP16384 and TG128");
    }

    #[test]
    fn a_custom_sweep_names_exactly_what_is_missing() {
        let gap = requested_gap("2048", 128).unwrap();
        assert_eq!(
            gap.missing_prefill,
            vec![128, 256, 512, 1024, 4096, 8192, 16384]
        );
        assert_eq!(gap.decode, None);
        assert_eq!(
            gap.describe(),
            "it is missing PP128, PP256, PP512, PP1024, PP4096, PP8192 and PP16384"
        );
        let blocker = gap.submission_blocker();
        assert!(blocker.starts_with("Partial run, not submittable:"));
        assert!(blocker.contains("PP128 to PP16384 and TG128"));
        assert!(blocker.contains("without --pp and --tg"));

        let decode = requested_gap(DEFAULT_PREFILL_TOKENS, 64).unwrap();
        assert_eq!(decode.describe(), "it measured TG64 instead of TG128");
        let both = requested_gap("128,256,512,1024,2048,4096,8192", 256).unwrap();
        assert_eq!(
            both.describe(),
            "it is missing PP16384 and it measured TG256 instead of TG128"
        );
    }

    #[test]
    fn saved_reports_are_judged_by_their_signed_samples() {
        let groups: serde_json::Map<String, Value> = required_prefill()
            .into_iter()
            .map(|size| (size.to_string(), json!([{"tokens": size, "elapsed_ns": 1}])))
            .collect();
        let complete = json!({"benchmark": {"raw_samples": {
            "prefill": groups,
            "decode": [{"generated_tokens": 128, "elapsed_ns": 1}]
        }}});
        assert_eq!(report_gap(&complete), None);

        let partial = json!({"benchmark": {"raw_samples": {
            "prefill": {"2048": [{"tokens": 2048, "elapsed_ns": 1}]},
            "decode": [{"generated_tokens": 128, "elapsed_ns": 1}]
        }}});
        assert_eq!(report_gap(&partial).unwrap().missing_prefill.len(), 7);

        let from_params = json!({"benchmark": {"params": {"pp": "512", "tg": 128}}});
        assert_eq!(report_gap(&from_params).unwrap().missing_prefill.len(), 7);

        let nothing = json!({"benchmark": {}});
        let gap = report_gap(&nothing).unwrap();
        assert_eq!(gap.decode, Some(0));
        assert!(gap
            .describe()
            .contains("it has no TG128 decode measurement"));
    }
}
