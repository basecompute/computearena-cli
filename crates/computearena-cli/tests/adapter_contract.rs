//! Black-box contract tests: no installed runtime, GPU, account, or external network.
#![cfg(unix)]

#[test]
fn cooldown_plan_estimates_waits_and_requires_confirmation_before_execution() {
    let f = Fixture::new("llama-cpp");
    let output = f
        .command()
        .args(["llama-cpp", "run"])
        .arg(&f.model)
        .args(["--pp", "128,512", "--cooldown"])
        .output()
        .unwrap();
    failure(&output, "confirmation requires a terminal");
    for hint in [
        "Standard: no cooldown waits",
        "thermally controlled adds",
        "30s–9m 0s",
        "1m 30s",
    ] {
        assert!(
            text(&output).contains(hint),
            "missing {hint}: {}",
            text(&output)
        );
    }
    assert!(!f.report.exists());
    assert!(!f.dir.path().join("args").exists());
}

#[test]
fn closed_menu_input_exits_instead_of_prompting_forever() {
    let f = Fixture::new("llama-cpp");
    failure(
        &f.command().arg("llama-cpp").output().unwrap(),
        "input closed",
    );
}

#[test]
fn invalid_model_selection_returns_to_the_menu_instead_of_exiting() {
    let f = Fixture::new("llama-cpp");
    let mut child = f
        .command()
        .arg("llama-cpp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"1\n/definitely-missing-computearena-model.gguf\n6\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    success(&output);
    assert!(text(&output).contains("Could not select model:"));
    assert_eq!(text(&output).matches("Choose an option:").count(), 2);
    assert!(!f.report.exists());
}

#[test]
fn telemetry_is_collected_for_the_child_summarized_and_signature_protected() {
    let f = Fixture::new("llama-cpp");
    f.install(&f.result(), "/bin/sleep 1.2");
    let mut report = f.signed();
    let telemetry = &report["benchmark"]["telemetry"];
    assert_eq!(telemetry["schema"], "computearena-telemetry/1");
    let observed = &telemetry["workloads"]["pp512"];
    assert_eq!(observed["observer"]["requested_interval_ms"], 1000);
    assert!(
        observed["process_memory"]["statistics"]["sample_count"]
            .as_u64()
            .unwrap()
            >= 1
    );
    assert_eq!(
        report["benchmark"]["memory"]["process_peak_rss_mb"],
        telemetry["process_memory"]["statistics"]["peak"]
    );
    assert_eq!(observed["energy"]["available"], false);
    assert_eq!(observed["per_workload"]["available"], false);
    success(&f.verify());
    if let Some(path) = std::env::var_os("COMPUTEARENA_TELEMETRY_TEST_REPORT") {
        fs::copy(&f.report, path).unwrap();
    }
    report["benchmark"]["telemetry"]["workloads"]["pp512"]["observer"]["requested_interval_ms"] =
        json!(25);
    fs::write(&f.report, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    failure(&f.verify(), "signature verification failed");
}

#[test]
fn plans_share_layout_and_identify_full_binary_paths_before_execution() {
    for runtime in RUNTIMES {
        let f = Fixture::new(runtime);
        let output = f.run(&[]);
        success(&output);
        let output = text(&output);
        let plan = output.find("Benchmark plan").unwrap();
        let running = output.find("Running the benchmark").unwrap();
        let binary = fs::canonicalize(&f.executable).unwrap();
        assert!(output.find(binary.to_str().unwrap()).unwrap() < plan);
        assert!(output.contains(env!("CARGO_BIN_EXE_computearena")));
        let mut previous = plan;
        for label in [
            "Runtime",
            "Model",
            "Workloads",
            "Sampling",
            "Estimated",
            "Output",
            "Selected:",
        ] {
            let position = output[previous..].find(label).unwrap() + previous;
            assert!(position < running);
            previous = position;
        }
        assert!(output.contains("PP128, PP512"));
        assert!(output.contains("TG128"));
        assert!(output.contains("Nothing is uploaded automatically"));
        assert!(output.contains("sustained CPU/GPU load"));
        assert!(output.contains(fs::canonicalize(&f.model).unwrap().to_str().unwrap()));
    }
}

#[test]
fn relative_and_home_relative_model_paths_resolve_to_absolute_paths() {
    for home_relative in [false, true] {
        let f = Fixture::new("llama-cpp");
        let name = f.model.file_name().unwrap().to_str().unwrap();
        let supplied = if home_relative {
            format!("~/{name}")
        } else {
            name.to_owned()
        };
        let output = f
            .command()
            .current_dir(f.dir.path())
            .args([
                "llama-cpp",
                "run",
                &supplied,
                "--pp",
                "128,512",
                "--reps",
                "2",
                "--yes",
                "--output",
            ])
            .arg(&f.report)
            .output()
            .unwrap();
        success(&output);
        let absolute = fs::canonicalize(&f.model).unwrap();
        assert!(text(&output).contains(absolute.to_str().unwrap()));
        let args = fs::read_to_string(f.dir.path().join("args")).unwrap();
        assert!(args.lines().any(|arg| arg == absolute.to_str().unwrap()));
    }
}

#[test]
fn repeated_runs_have_unique_ids_but_keep_the_installation_identity() {
    for runtime in RUNTIMES {
        let f = Fixture::new(runtime);
        let first = f.signed();
        let original_bytes = fs::read(&f.report).unwrap();
        failure(&f.run(&[]), "refusing to overwrite existing report");
        assert_eq!(fs::read(&f.report).unwrap(), original_bytes);
        fs::rename(&f.report, f.dir.path().join("first.json")).unwrap();
        let second = f.signed();
        assert_ne!(first["run_id"], second["run_id"]);
        assert_eq!(first["installation"], second["installation"]);
        assert_eq!(first["runtime"]["binary"], second["runtime"]["binary"]);
        let reordered = format!(
            "{{{}}}",
            second
                .as_object()
                .unwrap()
                .iter()
                .rev()
                .map(|(key, value)| format!("{}:{}", serde_json::to_string(key).unwrap(), value))
                .collect::<Vec<_>>()
                .join(",")
        );
        fs::write(&f.report, reordered).unwrap();
        success(&f.verify());
    }
}

#[test]
fn select_all_reports_deleted_failures_and_still_uploads_other_benchmarks() {
    for statuses in [vec![409, 201, 200], vec![409]] {
        let f = Fixture::full_sweep("llama-cpp");
        let reports = f.dir.path().join("data/reports");
        fs::create_dir_all(&reports).unwrap();
        let originals: Vec<_> = (0..statuses.len())
            .map(|index| {
                let report = f.signed();
                let path = reports.join(format!("saved-{index}.json"));
                fs::rename(&f.report, &path).unwrap();
                (path, report)
            })
            .collect();
        let (url, received) = server(&f, statuses);
        let mut child = f
            .command()
            .args(["--api-url", &url, "submit", "--yes"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"all\n").unwrap();
        let output = child.wait_with_output().unwrap();
        failure(&output, "1 previously deleted");
        let printed = text(&output);
        for hint in [
            "Failed: previously deleted",
            "Select all does not restore",
            "rerun the same model with the same settings",
            "newly generated report as a separate benchmark",
            "local files are unchanged",
        ] {
            assert!(printed.contains(hint), "missing {hint}: {printed}");
        }
        if originals.len() == 3 {
            assert!(
                printed.contains("1 uploaded, 1 already present"),
                "{printed}"
            );
        } else {
            assert!(
                printed.contains("0 uploaded, 0 already present"),
                "{printed}"
            );
        }
        let attempted = received.join().unwrap();
        assert_eq!(attempted.len(), originals.len());
        for (path, report) in originals {
            assert!(attempted.contains(&report));
            assert_eq!(
                serde_json::from_slice::<Value>(&fs::read(path).unwrap()).unwrap(),
                report
            );
        }
    }
}

#[test]
fn systemic_submission_failure_stops_the_queue_but_report_rejection_does_not() {
    for status in [422, 429, 500] {
        let f = Fixture::full_sweep("basert");
        let first = f.signed();
        let first_path = f.dir.path().join("first.json");
        fs::rename(&f.report, &first_path).unwrap();
        let second = f.signed();
        let (url, received) = server(
            &f,
            if status == 422 {
                vec![status, 201]
            } else {
                vec![status]
            },
        );
        let output = f
            .command()
            .args(["--api-url", &url, "submit", "--yes"])
            .arg(first_path)
            .arg(&f.report)
            .output()
            .unwrap();
        failure(&output, "were not submitted");
        if status == 422 {
            assert!(text(&output).contains("Benchmark submitted"));
            assert_eq!(received.join().unwrap(), vec![first, second]);
        } else {
            assert!(text(&output).contains("Not attempted"));
            assert_eq!(received.join().unwrap(), vec![first]);
        }
        success(&f.verify());
    }
}

#[test]
fn basert_rejects_measurements_that_do_not_match_the_requested_workload() {
    for (pointer, replacement) in [
        ("/raw_samples/prefill/128/0/tokens", json!(512)),
        ("/raw_samples/decode/0/generated_tokens", json!(256)),
        (
            "/raw_samples/decode/0/elapsed_ns",
            json!(9007199254740992u64),
        ),
        (
            "/raw_samples/decode",
            json!([{"generated_tokens":128,"elapsed_ns":100}]),
        ),
        ("/params/tg", json!(256)),
    ] {
        let f = Fixture::new("basert");
        let mut result = f.result();
        *result.pointer_mut(pointer).unwrap() = replacement;
        f.install(&result, "");
        let output = f.run(&[]);
        assert!(
            !output.status.success(),
            "accepted incorrect {pointer}: {}",
            text(&output)
        );
        assert!(!f.report.exists());
    }
    let f = Fixture::new("basert");
    let mut result = f.result();
    result["raw_samples"]["prefill"]
        .as_object_mut()
        .unwrap()
        .remove("512");
    f.install(&result, "");
    assert!(!f.run(&[]).status.success());
    assert!(!f.report.exists());
}

#[test]
fn incompatible_binaries_and_wrong_model_formats_fail_with_guidance() {
    for runtime in RUNTIMES {
        let f = Fixture::new(runtime);
        fs::write(&f.executable, "#!/bin/sh\nprintf '%s' '{}'\n").unwrap();
        let output = f.run(&[]);
        assert!(!output.status.success());
        assert!(!f.report.exists());
        if runtime == "llama-cpp" {
            assert!(text(&output).contains("https://github.com/ggml-org/llama.cpp/releases"));
            assert!(text(&output).contains("does not support"));
        } else {
            failure(&output, "compatible BaseRT benchmark protocol");
        }
    }
    let f = Fixture::new("llama-cpp");
    fs::write(&f.model, b"BASE not a GGUF").unwrap();
    failure(&f.run(&[]), "needs a GGUF model file");
    assert!(!f.dir.path().join("args").exists());
}

#[test]
fn unsigned_malformed_and_all_invalid_reports_are_never_submitted() {
    for runtime in RUNTIMES {
        let f = Fixture::new(runtime);
        let valid = f.signed();
        for pointer in [
            "/installation/key_id",
            "/signature/algorithm",
            "/signature/canonicalization",
            "/signature/value",
        ] {
            let mut changed = valid.clone();
            *changed.pointer_mut(pointer).unwrap() = json!("invalid");
            fs::write(&f.report, serde_json::to_vec(&changed).unwrap()).unwrap();
            assert!(!f.verify().status.success());
        }
        fs::write(&f.report, b"{broken JSON").unwrap();
        let output = f
            .command()
            .args(["submit", "--yes", "--skip-invalid"])
            .arg(&f.report)
            .output()
            .unwrap();
        failure(
            &output,
            "no valid benchmarks were selected; nothing was uploaded",
        );
        assert!(text(&output).contains("Invalid JSON"));
        assert!(!text(&output).contains("Submitting report"));
    }
}

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const RUNTIMES: [&str; 2] = ["basert", "llama-cpp"];

/// The default sweep. ComputeArena accepts only runs that contain all of it.
const DEFAULT_SWEEP: [u64; 8] = [128, 256, 512, 1024, 2048, 4096, 8192, 16384];

struct Fixture {
    dir: tempfile::TempDir,
    runtime: &'static str,
    executable: PathBuf,
    model: PathBuf,
    report: PathBuf,
    /// Prefill sizes the fake runtime measures.
    sizes: Vec<u64>,
}

impl Fixture {
    /// A short custom sweep (PP128 and PP512): quick to reason about in
    /// protocol tests, and a local-only run as far as submission goes.
    fn new(runtime: &'static str) -> Self {
        Self::with_sizes(runtime, &[128, 512])
    }

    /// The full default sweep, run without `--pp` and `--tg` exactly as the
    /// CLI tells people to: the only kind of report that can be submitted.
    fn full_sweep(runtime: &'static str) -> Self {
        Self::with_sizes(runtime, &DEFAULT_SWEEP)
    }

    fn is_default_sweep(&self) -> bool {
        self.sizes == DEFAULT_SWEEP
    }

    fn with_sizes(runtime: &'static str, sizes: &[u64]) -> Self {
        // Spaces exercise command argument handling rather than shell interpolation.
        let dir = tempfile::Builder::new()
            .prefix("arena contract ")
            .tempdir()
            .unwrap();
        let executable = dir.path().join(if runtime == "basert" {
            "basert-benchmark-harness"
        } else {
            "llama-bench"
        });
        let model = dir.path().join(if runtime == "basert" {
            "Qwen3.base"
        } else {
            "Qwen3.gguf"
        });
        let mut header;
        if runtime == "basert" {
            let metadata =
                serde_json::to_vec(&json!({"schema":7,"arch":"qwen","quant_scheme":"base_q4"}))
                    .unwrap();
            header = b"BASE".to_vec();
            header.extend(1_u32.to_le_bytes());
            header.extend((metadata.len() as u64).to_le_bytes());
            header.extend(metadata);
        } else {
            header = b"GGUF".to_vec();
            header.extend(3_u32.to_le_bytes());
            header.extend(0_u64.to_le_bytes());
            header.extend(0_u64.to_le_bytes());
        }
        fs::write(&model, header).unwrap();
        let report = dir.path().join("report.json");
        let fixture = Self {
            dir,
            runtime,
            executable,
            model,
            report,
            sizes: sizes.to_vec(),
        };
        fixture.install(&fixture.result(), "");
        fixture
    }

    fn result(&self) -> Value {
        if self.runtime == "basert" {
            let pp = self
                .sizes
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(",");
            // Every workload takes 100 ms and then 200 ms, so its mean rate is
            // 7.5 tokens per token of workload: PP128 is 960 tok/s, PP512 3840.
            let mut metrics = serde_json::Map::new();
            let mut prefill = serde_json::Map::new();
            for size in &self.sizes {
                metrics.insert(format!("pp{size}_t_s"), json!(*size as f64 * 7.5));
                prefill.insert(
                    size.to_string(),
                    json!([{"tokens":size,"elapsed_ns":100000000},{"tokens":size,"elapsed_ns":200000000}]),
                );
            }
            metrics.insert("decode_t_s".into(), json!(960.0));
            json!({"schema":"basert-benchmark-harness/1","mode":"text","runtime_version":"0.2.4",
                "chip":"Test CPU","backend":"CPU","params":{"pp":pp,"tg":128,"reps":2},
                "metrics":metrics,
                "raw_samples":{"prefill":prefill,
                    "decode":[{"generated_tokens":128,"elapsed_ns":100000000},{"generated_tokens":128,"elapsed_ns":200000000}]}})
        } else {
            let workloads = self.sizes.iter().map(|pp| (*pp, 0)).chain([(0, 128)]);
            Value::Array(workloads.map(|(pp,tg)| json!({
                "build_commit":"abc123","build_number":123,"model_type":"Qwen3 Q4_K_M",
                "model_filename":self.model,"model_size":24,"model_n_params":4000000000u64,
                "n_prompt":pp,"n_gen":tg,"n_depth":if pp == 0 { 1 } else { 0 },"n_gpu_layers":0,"backends":"CPU",
                "cpu_info":"Test CPU","gpu_info":"","samples_ns":[100000000,200000000],
                // Reported aggregates are deliberately bogus: native samples are authoritative.
                "avg_ts":999999.0,"avg_ns":1
            })).collect())
        }
    }

    fn install(&self, result: &Value, before_result: &str) {
        let native_same_run = result.pointer("/telemetry/schema").and_then(Value::as_str)
            == Some("basert-telemetry/4");
        let telemetry_schema = if native_same_run {
            "basert-telemetry/4"
        } else {
            "basert-telemetry/3"
        };
        let descriptor = json!({"schema":"basert-benchmark-harness-descriptor/1",
            "runtime":{"name":"basert","version":"0.2.4"},"result_schema":"basert-benchmark-harness/1",
            "telemetry_schema":telemetry_schema,
            "capacity_protocol_schema":"basert-throughput-protocol/2",
            "features":{"telemetry":true,"same_run_telemetry":native_same_run,
                "headline_context_capacity":result.pointer("/protocol/schema").and_then(Value::as_str)==Some("basert-throughput-protocol/2")}});
        let result_script = if self.runtime == "llama-cpp" {
            let rows = result.as_array().unwrap();
            let prefill: Value = rows
                .iter()
                .filter(|row| row["n_prompt"].as_u64().unwrap_or(0) > 0)
                .cloned()
                .collect();
            let decode: Value = rows
                .iter()
                .filter(|row| row["n_gen"].as_u64().unwrap_or(0) > 0)
                .cloned()
                .collect();
            // The headline workload (PP512) runs first and on its own; the
            // other sizes follow in one sweep. Rows are chosen by the position
            // the fixture gave them, so they keep their place even when a test
            // deliberately corrupts a token count.
            let headline_index = self.sizes.iter().position(|size| *size == 512).unwrap_or(0);
            let headline_size = self.sizes[headline_index];
            let headline: Value = prefill
                .as_array()
                .unwrap()
                .iter()
                .skip(headline_index)
                .take(1)
                .cloned()
                .collect();
            let remaining: Value = prefill
                .as_array()
                .unwrap()
                .iter()
                .enumerate()
                .filter(|(index, _)| *index != headline_index)
                .map(|(_, row)| row.clone())
                .collect();
            format!(
                "tg=0\npp=0\nwhile [ \"$#\" -gt 0 ]; do\ncase \"$1\" in\n-n) shift; tg=\"$1\";;\n-p) shift; pp=\"$1\";;\nesac\nshift\ndone\nif [ \"$tg\" != 0 ]; then\n/bin/cat <<'RESULT'\n{decode}\nRESULT\nelif [ \"$pp\" = {headline_size} ]; then\n/bin/cat <<'RESULT'\n{headline}\nRESULT\nelse\n/bin/cat <<'RESULT'\n{remaining}\nRESULT\nfi"
            )
        } else {
            format!("/bin/cat <<'RESULT'\n{result}\nRESULT")
        };
        let script = format!("#!/bin/sh\ncase \"$1\" in\n describe) printf '%s\\n' '{descriptor}';;\n --help) printf '%s\\n' '--n-prompt --n-gen --n-depth --repetitions --no-warmup json';;\n *) printf '%s\\n' \"$@\" >> \"$ARENA_TEST_ARGS\"\n{before_result}\n{result_script}\n;;\nesac\n");
        fs::write(&self.executable, script).unwrap();
        fs::set_permissions(&self.executable, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_computearena"));
        cmd.env_clear()
            .env("HOME", self.dir.path())
            .env("PATH", self.dir.path())
            .env("NO_COLOR", "1")
            .env("ARENA_TEST_ARGS", self.dir.path().join("args"))
            .env("NO_PROXY", "*")
            .args(["--data-dir"])
            .arg(self.dir.path().join("data"))
            .env("COMPUTEARENA_API_URL", "http://127.0.0.1:1/api/v1")
            // No test may ask GitHub which BaseRT is newest: the lookup is
            // pointed at a closed port unless a test serves its own answer.
            .env(
                "COMPUTEARENA_BASERT_RELEASE_API",
                "http://127.0.0.1:1/latest",
            )
            .stdin(Stdio::null());
        cmd
    }

    fn run(&self, extra: &[&str]) -> Output {
        self.run_command(extra).output().unwrap()
    }

    /// What the last BaseRT release lookup is remembered to have found.
    fn remember_latest_basert(&self, version: &str) {
        let data = self.dir.path().join("data");
        fs::create_dir_all(&data).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        fs::write(
            data.join("basert-update-check.json"),
            serde_json::to_vec(&json!({
                "checkedAtUnixSeconds": now,
                "latestVersion": version,
                "releaseUrl": "https://example.test/release"
            }))
            .unwrap(),
        )
        .unwrap();
    }

    /// A BaseRT harness that advertises the headline-first protocol.
    fn install_headline_capable(&self) {
        let mut result = self.result();
        result["params"]["ctx"] = json!(4096);
        result["protocol"] = json!({"schema":"basert-throughput-protocol/2","profile":"basert-bench-capacity/1",
            "context_isolation":"headline_then_per_prefill","context_capacity_policy":"basert_bench_default",
            "model_load_in_timing":false,"execution_layout":"headline_then_prefill_processes",
            "execution_order":["pp512","tg128","pp128"],
            "prefill":{"128":{"initial_context_tokens":0,"context_capacity_tokens":4096},
                "512":{"initial_context_tokens":0,"context_capacity_tokens":4096}},
            "decode":{"initial_context_tokens":1,"context_capacity_tokens":4096,"seed_prefill_in_timing":false},
            "measurement":{"timed_repetitions":2,"requested_warmup_repetitions":3,
                "warmup_policy":"fixed_repetitions","minimum_warmup_s":0,
                "telemetry":"disabled","cooldown":false,"timing":"harness_existing_token_operations"}});
        self.install(&result, "");
    }

    fn run_command(&self, extra: &[&str]) -> Command {
        let mut command = self.command();
        command.arg(self.runtime).arg("run").arg(&self.model);
        if !self.is_default_sweep() {
            let pp = self
                .sizes
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(",");
            command.args(["--pp", &pp, "--tg", "128"]);
        }
        command
            .args(["--reps", "2", "--yes", "--output"])
            .arg(&self.report)
            .args(extra);
        command
    }

    fn signed(&self) -> Value {
        success(&self.run(&[]));
        serde_json::from_slice(&fs::read(&self.report).unwrap()).unwrap()
    }

    fn verify(&self) -> Output {
        self.command()
            .arg("verify")
            .arg(&self.report)
            .output()
            .unwrap()
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}
fn success(output: &Output) {
    assert!(output.status.success(), "{}", text(output));
    assert!(
        !text(output).contains('\u{1b}'),
        "NO_COLOR must suppress ANSI escapes"
    );
}
fn failure(output: &Output, reason: &str) {
    assert!(
        !output.status.success(),
        "unexpected success: {}",
        text(output)
    );
    assert!(
        text(output).contains(reason),
        "expected {reason:?}: {}",
        text(output)
    );
}

#[test]
fn runtimes_agree_on_samples_units_and_rates_without_faking_protocol_equivalence() {
    let base = Fixture::new("basert");
    let llama = Fixture::new("llama-cpp");
    let a = base.signed();
    let b = llama.signed();
    for field in ["raw_samples", "metrics", "chip", "backend"] {
        assert_eq!(a["benchmark"][field], b["benchmark"][field], "{field}");
    }
    assert_eq!(b["benchmark"]["metrics"]["pp512_t_s"], 3840.0);
    assert_eq!(b["benchmark"]["protocol"]["warmup"], "runtime_native");
    assert_eq!(b["benchmark"]["params"]["decode_context_tokens"], 1);
    assert_eq!(b["benchmark"]["protocol"]["telemetry_available"], true);
    assert_eq!(
        b["benchmark"]["telemetry"]["scope"],
        "headline_then_prefill_processes"
    );
    assert!(b["benchmark"]["telemetry"].get("memory_replay").is_none());
    assert_eq!(
        a["benchmark"]["telemetry"]["schema"],
        "computearena-telemetry/1"
    );
    assert_eq!(
        a["benchmark"]["telemetry"]["scope"],
        "whole_runtime_process"
    );
    assert_eq!(
        a["benchmark"]["telemetry"]["adapter"]["mode"],
        "external_whole_process"
    );
    assert_eq!(
        a["benchmark"]["telemetry"]["conditioning"]["mode"],
        "warmup_only"
    );
    assert!(a["benchmark"]["telemetry"].get("memory_replay").is_none());
    for (fixture, report) in [(&base, a), (&llama, b)] {
        assert_eq!(
            report["runtime"]["binary"]["sha256"],
            format!(
                "{:x}",
                Sha256::digest(fs::read(&fixture.executable).unwrap())
            )
        );
        assert_eq!(report["runtime"]["binary"]["os"], std::env::consts::OS);
        assert_eq!(report["runtime"]["binary"]["arch"], std::env::consts::ARCH);
        assert!(!report
            .to_string()
            .contains(fixture.dir.path().to_str().unwrap()));
        success(&fixture.verify());
        let args = fs::read_to_string(fixture.dir.path().join("args")).unwrap();
        if fixture.runtime == "basert" {
            assert!(args.contains("\n128,512\n"));
        } else {
            let prompts: Vec<_> = args
                .lines()
                .collect::<Vec<_>>()
                .windows(2)
                .filter(|pair| pair[0] == "-p")
                .map(|pair| pair[1])
                .collect();
            assert_eq!(prompts, ["512", "0", "128"]);
            assert!(!args.contains("--ctx"));
            assert!(!args.contains("-d\n4096\n"));
        }
        assert!(args.contains(fixture.model.to_str().unwrap()));
        assert!(!args.contains("--cooldown"));
        assert!(!args.contains("--telemetry"));
        if fixture.runtime == "llama-cpp" {
            assert!(args.contains("-d\n0\n"));
            assert!(args.contains("-d\n1\n"));
            assert!(args.contains("-o\njson\n"));
        }
    }
}

#[test]
fn basert_native_same_run_telemetry_is_selected_by_capability_not_version() {
    let f = Fixture::new("basert");
    let mut result = f.result();
    result["telemetry"] = json!({"schema":"basert-telemetry/4","marker":"native-same-run"});
    f.install(&result, "");

    let report = f.signed();
    assert_eq!(
        report["benchmark"]["telemetry"]["schema"],
        "basert-telemetry/4"
    );
    assert_eq!(
        report["benchmark"]["telemetry"]["marker"],
        "native-same-run"
    );
    assert!(report["benchmark"]["telemetry"].get("adapter").is_none());
    let args = fs::read_to_string(f.dir.path().join("args")).unwrap();
    assert!(args.contains("--telemetry"));
}

#[test]
fn headline_capable_basert_signs_new_metadata_without_changing_requested_repetitions() {
    let f = Fixture::new("basert");
    f.install_headline_capable();
    let result = f.result();
    let signed = f.signed();
    assert_eq!(
        signed["benchmark"]["protocol"]["id"],
        "computearena-throughput/3"
    );
    assert_eq!(signed["benchmark"]["raw_samples"], result["raw_samples"]);
    let args = fs::read_to_string(f.dir.path().join("args")).unwrap();
    assert!(args.contains("--headline-first"));
    assert!(!args.contains("--isolated-workloads"));
    assert!(args.contains("-r\n2\n-w\n3\n"));
    success(&f.verify());
    if let Some(path) = std::env::var_os("COMPUTEARENA_HEADLINE_TEST_REPORT") {
        fs::copy(&f.report, path).unwrap();
    }
}

#[test]
fn signed_fields_cannot_be_tampered_with_in_either_runtime() {
    for runtime in RUNTIMES {
        let f = Fixture::new(runtime);
        let original = f.signed();
        for (pointer, replacement) in [
            ("/runtime/name", json!("other")),
            ("/runtime/binary/sha256", json!("0".repeat(64))),
            ("/runtime/binary/version", json!("forged")),
            ("/runtime/binary/os", json!("forged")),
            ("/benchmark/metrics/decode_t_s", json!(999999)),
            ("/benchmark/raw_samples/decode/0/elapsed_ns", json!(1)),
            ("/benchmark/params/tg", json!(256)),
            ("/model/name", json!("another model")),
            ("/run_id", json!("forged")),
            ("/created_at_unix_ms", json!(1)),
        ] {
            let mut changed = original.clone();
            *changed
                .pointer_mut(pointer)
                .unwrap_or_else(|| panic!("missing {pointer}")) = replacement;
            fs::write(&f.report, serde_json::to_vec(&changed).unwrap()).unwrap();
            let output = f.verify();
            failure(&output, "signature verification failed");
            assert_eq!(
                text(&output)
                    .matches("signature verification failed")
                    .count(),
                1
            );
        }
        let mut unsigned = original.clone();
        unsigned.as_object_mut().unwrap().remove("signature");
        fs::write(&f.report, serde_json::to_vec(&unsigned).unwrap()).unwrap();
        failure(&f.verify(), "report is unsigned");
        // JSON formatting is not part of signed content; installed binary is not needed.
        fs::write(&f.report, serde_json::to_vec_pretty(&original).unwrap()).unwrap();
        fs::remove_file(&f.executable).unwrap();
        success(&f.verify());
    }
}

#[test]
fn executable_mutation_or_runtime_failure_never_creates_a_signed_report() {
    for runtime in RUNTIMES {
        for (script, reason) in [
            (
                "printf '\\n# changed\\n' >> \"$0\"",
                "executable changed during the benchmark",
            ),
            ("exit 7", "exited with"),
            ("printf 'not json'; exit 0", "JSON"),
        ] {
            let f = Fixture::new(runtime);
            f.install(&f.result(), script);
            failure(&f.run(&[]), reason);
            assert!(!f.report.exists());
        }
    }
}

#[test]
fn invalid_native_measurements_are_rejected_before_signing() {
    for (pointer, value, reason) in [
        ("/0/n_depth", json!(512), "context depth"),
        ("/0/n_prompt", json!(2048), "unexpected workload"),
        ("/0/samples_ns", json!([0, 1]), "invalid sample duration"),
        ("/0/samples_ns", json!([-1, 1]), "invalid sample duration"),
        (
            "/0/samples_ns",
            json!([9007199254740992u64, 1]),
            "invalid sample duration",
        ),
        ("/0/samples_ns", json!([1]), "repetition count"),
        ("/1/build_commit", json!("other"), "changed build_commit"),
    ] {
        let f = Fixture::new("llama-cpp");
        let mut result = f.result();
        *result.pointer_mut(pointer).unwrap() = value;
        f.install(&result, "");
        failure(&f.run(&[]), reason);
        assert!(!f.report.exists());
    }
    for runtime in RUNTIMES {
        let f = Fixture::new(runtime);
        let mut result = f.result();
        if runtime == "basert" {
            result["raw_samples"]["decode"] = json!([]);
        } else {
            result.as_array_mut().unwrap().pop();
        }
        f.install(&result, "");
        assert!(!f.run(&[]).status.success());
        assert!(!f.report.exists());
    }
}

#[test]
fn discovery_help_runtime_scoping_and_unsupported_options_are_actionable() {
    for runtime in RUNTIMES {
        let f = Fixture::new(runtime);
        let help = f
            .command()
            .args([runtime, "run", "--help"])
            .output()
            .unwrap();
        success(&help);
        assert!(text(&help).contains(&format!("computearena {runtime} run")));
        failure(
            &f.command()
                .args([
                    runtime,
                    if runtime == "basert" {
                        "llama-cpp"
                    } else {
                        "basert"
                    },
                ])
                .output()
                .unwrap(),
            "unrecognized subcommand",
        );
        let renamed = f.dir.path().join("explicit executable");
        fs::rename(&f.executable, &renamed).unwrap();
        failure(&f.run(&[]), "PATH");
        assert!(!f.report.exists());
        success(&f.run(&["--runtime-path", renamed.to_str().unwrap()]));
    }
    let f = Fixture::new("llama-cpp");
    let help = f
        .command()
        .args(["llama-cpp", "run", "--cooldown", "--help"])
        .output()
        .unwrap();
    success(&help);
    assert!(text(&help).contains("--cooldown"));
    assert!(!f.dir.path().join("args").exists());
    success(&f.run(&["--warmup", "0"]));
    assert!(fs::read_to_string(f.dir.path().join("args"))
        .unwrap()
        .contains("--no-warmup"));
    let report: Value = serde_json::from_slice(&fs::read(&f.report).unwrap()).unwrap();
    assert_eq!(report["benchmark"]["protocol"]["warmup"], "disabled");
}

#[test]
fn runtime_menus_can_exit_and_saved_reports_share_list_and_inspect_commands() {
    for runtime in RUNTIMES {
        let f = Fixture::new(runtime);
        let mut child = f
            .command()
            .arg(runtime)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"6\n").unwrap();
        let output = child.wait_with_output().unwrap();
        success(&output);
        for label in [
            "Run benchmarks",
            "Submit previous benchmarks",
            "Verify a local benchmark",
            "Choose an option:",
        ] {
            assert!(text(&output).contains(label));
        }
        assert!(text(&output).contains(&format!("Runtime: {runtime}")));
        let report = f.signed();
        let reports = f.dir.path().join("data/reports");
        fs::create_dir_all(&reports).unwrap();
        fs::copy(&f.report, reports.join("saved.json")).unwrap();
        let listed = f.command().args(["list", "--json"]).output().unwrap();
        success(&listed);
        let rows: Value = serde_json::from_slice(&listed.stdout).unwrap();
        assert_eq!(rows[0]["runtime"], runtime);
        assert_eq!(rows[0]["status"], "valid");
        let inspected = f.command().arg("inspect").arg(&f.report).output().unwrap();
        success(&inspected);
        assert_eq!(
            serde_json::from_slice::<Value>(&inspected.stdout).unwrap(),
            report
        );
    }
}

/// Minimal bounded HTTP server: checks actual upload bytes, not a mocked client function.
fn server(f: &Fixture, statuses: Vec<u16>) -> (String, thread::JoinHandle<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/api/v1", listener.local_addr().unwrap());
    fs::write(
        f.dir.path().join("data/auth.json"),
        serde_json::to_vec(&json!({
            "version":1, "origins": { (url.clone()): {
                "access_token":"ca_cli_test_authenticated_token",
                "username":"tester", "expires_at":"2099-01-01T00:00:00Z"
            }}
        }))
        .unwrap(),
    )
    .unwrap();
    let handle = thread::spawn(move || {
        let mut received = Vec::new();
        for status in statuses {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(e) => panic!("expected upload before deadline: {e}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut data = Vec::new();
            let (offset, length) = loop {
                let mut buf = [0; 4096];
                let n = stream.read(&mut buf).unwrap();
                assert!(n > 0);
                data.extend_from_slice(&buf[..n]);
                if let Some(end) = data.windows(4).position(|s| s == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&data[..end]).to_lowercase();
                    assert!(headers.starts_with("post /api/v1/submissions http/1.1"));
                    assert!(
                        headers.contains("authorization: bearer ca_cli_test_authenticated_token")
                    );
                    let len = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .unwrap()
                        .trim()
                        .parse::<usize>()
                        .unwrap();
                    break (end + 4, len);
                }
                assert!(data.len() < 65536);
            };
            while data.len() < offset + length {
                let mut buf = [0; 4096];
                let n = stream.read(&mut buf).unwrap();
                assert!(n > 0);
                data.extend_from_slice(&buf[..n]);
            }
            received.push(serde_json::from_slice(&data[offset..offset + length]).unwrap());
            let body = if status == 409 {
                json!({"error":{"code":"submission_deleted","message":"Report deleted"}})
                    .to_string()
            } else {
                json!({"id":"test-submission","runtime_provenance":{
                "status":"mismatch","message":"Benchmark accepted. This binary differs from the registered release.",
                "download_url":"https://github.com/ggml-org/llama.cpp/releases"}}).to_string()
            };
            write!(stream,"HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
        received
    });
    (url, handle)
}

#[test]
fn checksum_mismatch_is_informational_and_duplicate_submission_is_not_an_error() {
    for runtime in RUNTIMES {
        let f = Fixture::full_sweep(runtime);
        let report = f.signed();
        let (url, received) = server(&f, vec![201, 200]);
        for message in ["Benchmark submitted", "Already submitted"] {
            let output = f
                .command()
                .args(["--api-url", &url, "submit", "--yes"])
                .arg(&f.report)
                .output()
                .unwrap();
            success(&output);
            assert!(text(&output).contains(message));
            assert!(text(&output).contains("Benchmark accepted."));
            assert!(text(&output)
                .contains("Official downloads: https://github.com/ggml-org/llama.cpp/releases"));
            assert!(text(&output).contains("publicly accessible"));
            assert!(!text(&output).contains("Rejected"));
        }
        assert_eq!(received.join().unwrap(), vec![report.clone(), report]);
    }
}

#[test]
fn mixed_batch_requires_explicit_skip_and_uploads_only_the_valid_report() {
    let f = Fixture::full_sweep("llama-cpp");
    let valid = f.signed();
    let invalid = f.dir.path().join("tampered.json");
    let mut changed = valid.clone();
    changed["benchmark"]["metrics"]["decode_t_s"] = json!(1);
    fs::write(&invalid, serde_json::to_vec(&changed).unwrap()).unwrap();
    let output = f
        .command()
        .args(["submit", "--yes"])
        .arg(&f.report)
        .arg(&invalid)
        .output()
        .unwrap();
    failure(&output, "refusing a partial non-interactive submission");
    assert!(!text(&output).contains("Submitting report"));
    let (url, received) = server(&f, vec![201]);
    let output = f
        .command()
        .args(["--api-url", &url, "submit", "--yes", "--skip-invalid"])
        .arg(&f.report)
        .arg(&invalid)
        .output()
        .unwrap();
    success(&output);
    assert!(text(&output).contains("signature verification failed"));
    assert_eq!(received.join().unwrap(), vec![valid]);
}

#[test]
fn successful_gguf_runs_populate_the_picker_and_number_selection_reuses_the_file() {
    let f = Fixture::new("llama-cpp");
    let report = f.signed();
    let history = f.dir.path().join("data/recent-gguf.json");
    let expected = fs::canonicalize(&f.model).unwrap();
    let entries: Vec<PathBuf> = serde_json::from_slice(&fs::read(&history).unwrap()).unwrap();
    assert_eq!(entries, vec![expected.clone()]);
    assert!(!report.to_string().contains(expected.to_str().unwrap()));
    let mut child = f
        .command()
        .args([
            "llama-cpp",
            "run",
            "--pp",
            "128,512",
            "--tg",
            "128",
            "--reps",
            "2",
            "--yes",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"99\n1\n").unwrap();
    let output = child.wait_with_output().unwrap();
    success(&output);
    let output = text(&output);
    assert!(output.contains("Recently used GGUF models"));
    assert!(output.contains("Choose a number from 1 to 1"));
    assert!(output.contains("Saved signed benchmark"));
    assert!(!output.contains('\u{1b}'));
    let args = fs::read_to_string(f.dir.path().join("args")).unwrap();
    assert!(args.lines().any(|arg| arg == expected.to_str().unwrap()));
    let entries: Vec<PathBuf> = serde_json::from_slice(&fs::read(history).unwrap()).unwrap();
    assert_eq!(entries, vec![expected]);
}

#[test]
fn missing_recent_gguf_can_be_replaced_with_a_manual_path() {
    let f = Fixture::new("llama-cpp");
    f.signed();
    let moved = f.dir.path().join("moved model.gguf");
    fs::rename(&f.model, &moved).unwrap();
    let mut child = f
        .command()
        .args([
            "llama-cpp",
            "run",
            "--pp",
            "128,512",
            "--tg",
            "128",
            "--reps",
            "2",
            "--yes",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("1\np\n{}\n", moved.display()).as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    success(&output);
    assert!(text(&output).contains("[missing or unavailable]"));
    assert!(text(&output).contains("Cannot use this GGUF file:"));
    assert!(text(&output).contains("Saved signed benchmark"));
    let entries: Vec<PathBuf> =
        serde_json::from_slice(&fs::read(f.dir.path().join("data/recent-gguf.json")).unwrap())
            .unwrap();
    assert_eq!(entries[0], fs::canonicalize(moved).unwrap());
}

#[test]
fn recent_history_failure_does_not_prevent_a_signed_benchmark_or_manual_selection() {
    let f = Fixture::new("llama-cpp");
    fs::create_dir_all(f.dir.path().join("data")).unwrap();
    let history = f.dir.path().join("data/recent-gguf.json");
    fs::write(&history, b"broken").unwrap();
    let mut child = f
        .command()
        .args([
            "llama-cpp",
            "run",
            "--pp",
            "128,512",
            "--tg",
            "128",
            "--reps",
            "2",
            "--yes",
            "--output",
        ])
        .arg(&f.report)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{}\n", f.model.display()).as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    success(&output);
    assert!(text(&output).contains("Could not read recent GGUF files"));
    assert!(text(&output).contains("Report saved, but could not update recent GGUF files"));
    success(&f.verify());
    assert_eq!(fs::read(history).unwrap(), b"broken");
}

#[test]
fn failed_benchmarks_and_basert_runs_do_not_create_gguf_history() {
    let f = Fixture::new("llama-cpp");
    f.install(&f.result(), "exit 1");
    assert!(!f.run(&[]).status.success());
    assert!(!f.dir.path().join("data/recent-gguf.json").exists());
    let f = Fixture::new("basert");
    f.signed();
    assert!(!f.dir.path().join("data/recent-gguf.json").exists());
}

#[test]
fn the_default_sweep_is_submittable_and_says_how_to_submit() {
    for runtime in RUNTIMES {
        let f = Fixture::full_sweep(runtime);
        let output = f.run(&[]);
        success(&output);
        let printed = text(&output);
        assert!(!printed.contains("local-only run"), "{printed}");
        assert!(!printed.contains("Local only:"), "{printed}");
        assert!(printed.contains("computearena submit"), "{printed}");
        let report: Value = serde_json::from_slice(&fs::read(&f.report).unwrap()).unwrap();
        let measured: Vec<u64> = report["benchmark"]["raw_samples"]["prefill"]
            .as_object()
            .unwrap()
            .keys()
            .map(|size| size.parse().unwrap())
            .collect();
        for size in DEFAULT_SWEEP {
            assert!(
                measured.contains(&size),
                "PP{size} missing from {measured:?}"
            );
        }
        success(&f.verify());
    }
}

#[test]
fn a_custom_sweep_is_a_local_only_run_and_says_so_before_and_after_it_runs() {
    for runtime in RUNTIMES {
        let f = Fixture::new(runtime);
        let output = f.run(&[]);
        success(&output);
        let printed = text(&output);
        let missing = "it is missing PP256, PP1024, PP2048, PP4096, PP8192 and PP16384";
        // In the plan, before the run starts: what it will lack, and how to
        // get a submittable one.
        let notice = printed
            .find("This will be a local-only run.")
            .expect(&printed);
        assert!(
            printed.find("Benchmark plan").unwrap() < notice,
            "{printed}"
        );
        assert!(
            notice < printed.find("Running the benchmark").unwrap(),
            "{printed}"
        );
        assert!(
            printed.contains("Local only: a partial run cannot be submitted"),
            "{printed}"
        );
        assert!(printed.contains(missing), "{printed}");
        assert!(printed.contains("PP128 to PP16384 and TG128"), "{printed}");
        assert!(printed.contains("Omit --pp and --tg"), "{printed}");
        // After the run: no invitation to submit a report that would be refused.
        assert!(
            printed.contains("Local only: Partial run, not submittable:"),
            "{printed}"
        );
        assert!(!printed.contains("computearena submit"), "{printed}");
        // The report itself is a good signed report.
        success(&f.verify());
    }
}

#[test]
fn partial_runs_are_refused_at_submission_with_what_is_missing_and_how_to_fix_it() {
    for runtime in RUNTIMES {
        let f = Fixture::new(runtime);
        let original = f.signed();
        let (url, received) = server(&f, vec![]);
        let output = f
            .command()
            .args(["--api-url", &url, "submit", "--yes"])
            .arg(&f.report)
            .output()
            .unwrap();
        failure(
            &output,
            "nothing was uploaded: the selected benchmark is a partial run",
        );
        let printed = text(&output);
        for expected in [
            "Partial runs (valid reports, local only)",
            "Partial run, not submittable: it is missing PP256, PP1024, PP2048, PP4096, PP8192 and PP16384",
            "ComputeArena accepts only runs with the full default sweep (PP128 to PP16384 and TG128)",
            "Run the benchmark again without --pp and --tg",
        ] {
            assert!(printed.contains(expected), "missing {expected:?}: {printed}");
        }
        // Refused before login is even asked for, and never sent.
        assert!(!printed.contains("Login is required"), "{printed}");
        assert!(!printed.contains("Submitting report"), "{printed}");
        assert!(!printed.contains("Invalid reports:"), "{printed}");
        assert!(received.join().unwrap().is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&f.report).unwrap()).unwrap(),
            original
        );
        success(&f.verify());
    }
}

#[test]
fn a_partial_run_in_a_batch_is_left_local_while_complete_runs_upload() {
    let partial = Fixture::new("llama-cpp");
    partial.signed();
    let f = Fixture::full_sweep("llama-cpp");
    let complete = f.signed();
    let partial_path = f.dir.path().join("partial.json");
    fs::copy(&partial.report, &partial_path).unwrap();

    // Non-interactive and not told to skip: nothing is sent.
    let output = f
        .command()
        .args(["submit", "--yes"])
        .arg(&f.report)
        .arg(&partial_path)
        .output()
        .unwrap();
    failure(&output, "1 of the selected benchmarks is a partial run");
    assert!(!text(&output).contains("Submitting report"));

    let (url, received) = server(&f, vec![201]);
    let output = f
        .command()
        .args(["--api-url", &url, "submit", "--yes", "--skip-invalid"])
        .arg(&f.report)
        .arg(&partial_path)
        .output()
        .unwrap();
    success(&output);
    let printed = text(&output);
    assert!(
        printed.contains("Partial run, not submittable"),
        "{printed}"
    );
    assert!(
        printed.contains("1 partial run(s) were not uploaded and stay local"),
        "{printed}"
    );
    assert_eq!(received.join().unwrap(), vec![complete]);
}

#[test]
fn saved_report_lists_mark_partial_runs_as_local_only() {
    let f = Fixture::new("basert");
    success(&f.run(&[]));
    let reports = f.dir.path().join("data/reports");
    fs::create_dir_all(&reports).unwrap();
    fs::copy(&f.report, reports.join("partial.json")).unwrap();
    let listed = f.command().args(["list"]).output().unwrap();
    success(&listed);
    let printed = text(&listed);
    assert!(printed.contains("LOCAL ONLY"), "{printed}");
    assert!(
        printed.contains("Partial run, not submittable"),
        "{printed}"
    );
    let json = f.command().args(["list", "--json"]).output().unwrap();
    success(&json);
    let summaries: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(summaries[0]["status"], "valid");
    assert_eq!(summaries[0]["submittable"], false);
    assert!(summaries[0]["submission_blocker"]
        .as_str()
        .unwrap()
        .starts_with("Partial run, not submittable"));
}

/// Serves one "latest release" answer the way GitHub does, on a loopback port.
fn release_feed(tag: &str) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/latest", listener.local_addr().unwrap());
    let body = json!({"tag_name": tag, "html_url": "https://example.test/release"}).to_string();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0);
            request.extend_from_slice(&buffer[..read]);
        }
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    });
    (url, handle)
}

/// How the advice ends depends on whether BaseRT publishes a bundle for the
/// machine running the tests; both endings are correct.
fn names_a_way_to_update(printed: &str) -> bool {
    printed.contains("Update with `computearena basert install`")
        || printed.contains("No prebuilt BaseRT is published for this platform")
}

#[test]
fn an_older_basert_is_named_before_the_plan_with_what_it_signs_and_how_to_update() {
    let f = Fixture::new("basert");
    let output = f.run(&[]);
    success(&output);
    let printed = text(&output);
    let summary = "BaseRT 0.2.4 predates the current benchmark protocol.";
    let notice = printed.find(summary).expect(&printed);
    assert!(
        notice < printed.find("Benchmark plan").unwrap(),
        "{printed}"
    );
    assert!(
        printed.contains("signed as computearena-throughput-legacy/1, marked not comparable"),
        "{printed}"
    );
    // Offline, the release to move to is the first one with the protocol.
    assert!(
        printed.contains("BaseRT 0.2.5 or newer measures PP512 and TG128 first"),
        "{printed}"
    );
    assert!(names_a_way_to_update(&printed), "{printed}");
    // Said once: the end of the run does not repeat it.
    assert_eq!(printed.matches(summary).count(), 1, "{printed}");
    // It is advice, not a gate: the report is signed as before.
    let report: Value = serde_json::from_slice(&fs::read(&f.report).unwrap()).unwrap();
    assert_eq!(
        report["benchmark"]["protocol"]["id"],
        "computearena-throughput-legacy/1"
    );
    success(&f.verify());

    // With the newest release known, the advice names it.
    let f = Fixture::new("basert");
    f.remember_latest_basert("0.2.6");
    let printed = text(&f.run(&[]));
    assert!(
        printed.contains("BaseRT 0.2.6 measures PP512 and TG128 first"),
        "{printed}"
    );

    // llama.cpp has nothing to do with any of this.
    let llama = Fixture::new("llama-cpp");
    llama.remember_latest_basert("9.9.9");
    let printed = text(&llama.run(&[]));
    assert!(!printed.contains("BaseRT"), "{printed}");
}

#[test]
fn a_current_basert_is_told_about_a_newer_release_and_nothing_else() {
    let f = Fixture::new("basert");
    f.install_headline_capable();
    f.remember_latest_basert("0.2.4");
    let output = f.run(&[]);
    success(&output);
    let printed = text(&output);
    assert!(!printed.contains("is available"), "{printed}");
    assert!(!printed.contains("predates"), "{printed}");

    fs::remove_file(&f.report).unwrap();
    f.remember_latest_basert("9.9.9");
    let output = f.run(&[]);
    success(&output);
    let printed = text(&output);
    let notice = printed
        .find("BaseRT 9.9.9 is available (installed: 0.2.4).")
        .expect(&printed);
    assert!(
        notice < printed.find("Benchmark plan").unwrap(),
        "{printed}"
    );
    assert!(!printed.contains("predates"), "{printed}");
    assert!(names_a_way_to_update(&printed), "{printed}");

    // A harness named by hand is not something an install would replace.
    fs::remove_file(&f.report).unwrap();
    let output = f
        .run_command(&["--runtime-path", f.executable.to_str().unwrap()])
        .output()
        .unwrap();
    success(&output);
    let printed = text(&output);
    assert!(
        printed.contains("This harness was chosen with --runtime-path"),
        "{printed}"
    );
}

#[test]
fn the_release_lookup_runs_beside_the_benchmark_and_is_remembered() {
    let f = Fixture::new("basert");
    f.install_headline_capable();
    let (feed, served) = release_feed("v9.9.9");
    let output = f
        .run_command(&[])
        .env("COMPUTEARENA_BASERT_RELEASE_API", &feed)
        .output()
        .unwrap();
    success(&output);
    served.join().unwrap();
    let printed = text(&output);
    // Whether the answer arrived before the plan or during the run, it is
    // said exactly once.
    assert_eq!(
        printed
            .matches("BaseRT 9.9.9 is available (installed: 0.2.4).")
            .count(),
        1,
        "{printed}"
    );
    let remembered: Value = serde_json::from_slice(
        &fs::read(f.dir.path().join("data/basert-update-check.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(remembered["latestVersion"], "9.9.9");

    // The next run answers from that, without a lookup: the feed is gone.
    fs::remove_file(&f.report).unwrap();
    let output = f
        .run_command(&[])
        .env("COMPUTEARENA_BASERT_RELEASE_API", &feed)
        .output()
        .unwrap();
    success(&output);
    let printed = text(&output);
    let notice = printed
        .find("BaseRT 9.9.9 is available (installed: 0.2.4).")
        .expect(&printed);
    assert!(
        notice < printed.find("Benchmark plan").unwrap(),
        "{printed}"
    );
}

#[test]
fn an_unreachable_release_feed_never_delays_or_fails_a_run() {
    let f = Fixture::new("basert");
    f.install_headline_capable();
    let started = Instant::now();
    let output = f.run(&[]);
    success(&output);
    assert!(started.elapsed() < Duration::from_secs(20));
    let printed = text(&output);
    assert!(!printed.contains("is available"), "{printed}");
    assert!(!f.dir.path().join("data/basert-update-check.json").exists());
}

#[test]
fn the_printed_session_names_an_older_basert_when_it_finds_it() {
    let f = Fixture::new("basert");
    f.remember_latest_basert("0.2.6");
    let mut child = f
        .command()
        .arg("basert")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"6\n").unwrap();
    let output = child.wait_with_output().unwrap();
    success(&output);
    let printed = text(&output);
    let found = printed.find("Found BaseRT 0.2.4").expect(&printed);
    let notice = printed
        .find("BaseRT 0.2.4 predates the current benchmark protocol.")
        .expect(&printed);
    assert!(found < notice, "{printed}");
    assert!(
        notice < printed.find("Run benchmarks").unwrap(),
        "{printed}"
    );
    assert!(
        printed.contains("BaseRT 0.2.6 measures PP512 and TG128 first"),
        "{printed}"
    );
}

#[test]
fn offline_report_cannot_be_uploaded_without_login() {
    let f = Fixture::full_sweep("basert");
    let original = f.signed();
    let output = f
        .command()
        .args(["submit", "--yes"])
        .arg(&f.report)
        .output()
        .unwrap();
    failure(&output, "Login is required");
    assert!(text(&output).contains("http://127.0.0.1:1/api/v1 login"));
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&f.report).unwrap()).unwrap(),
        original
    );
    success(&f.verify());
}

#[test]
fn model_identity_cannot_be_overridden_in_either_runtime() {
    for runtime in RUNTIMES {
        let f = Fixture::new(runtime);
        failure(
            &f.run(&["--model-id", "example/Distinct-Instruct-MoE"]),
            "unexpected argument",
        );
        assert!(!f.report.exists());
        success(&f.run(&[]));
        let mut report: Value = serde_json::from_slice(&fs::read(&f.report).unwrap()).unwrap();
        assert!(report["model"]["upstream_id"].is_null());
        assert_eq!(report["model"]["upstream_id_source"], "unresolved");
        assert_eq!(report["model"]["identity_verification"], "unverified");
        let digest = report["model"]["artifact_sha256"].as_str().unwrap();
        assert!(digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()));
        success(&f.verify());
        report["model"]["upstream_id"] = json!("example/Other");
        fs::write(&f.report, serde_json::to_vec(&report).unwrap()).unwrap();
        failure(&f.verify(), "signature verification failed");
    }
}

/// ik_llama.cpp's llama-bench: no `-d`, decode seeded with `-gp`, warmup
/// switched with `-w`, capability booleans instead of `backends`.
const IK_LLAMA_BENCH: &str = r##"#!/bin/sh
if [ "$1" = --help ]; then
printf '%s\n' '-p, --n-prompt <n> -n, --n-gen <n> -gp <pp,tg> -r, --repetitions <n> -o, --output <csv|json|md|sql> -w, --warmup <0|1>'
exit 0
fi
printf '%s\n' "$@" >> "$ARENA_TEST_ARGS"
pp=0
gp=
while [ "$#" -gt 0 ]; do
case "$1" in
-p) shift; pp="$1";;
-gp) shift; gp="$1";;
esac
shift
done
row() {
printf '%s{"build_commit":"def456","build_number":456,"model_type":"Qwen3 Q4_K_M","model_size":24,"model_n_params":4000000000,"cuda":false,"vulkan":false,"metal":false,"sycl":false,"n_gpu_layers":0,"fused_moe":true,"cpu_info":"Test CPU","gpu_info":"","n_prompt":%s,"n_gen":%s,"test":"%s","samples_ns":[100000000,200000000]}' "$@"
}
printf '['
if [ -n "$gp" ]; then
row '' "${gp%,*}" "${gp#*,}" "tg${gp#*,}@pp${gp%,*}"
else
sep=
IFS=,
for size in $pp; do row "$sep" "$size" 0 "pp$size"; sep=,; done
fi
printf ']'
"##;

#[test]
fn an_ik_llama_cpp_build_runs_the_same_protocol_and_is_named_in_the_signed_report() {
    let f = Fixture::full_sweep("llama-cpp");
    fs::write(&f.executable, IK_LLAMA_BENCH).unwrap();
    fs::set_permissions(&f.executable, fs::Permissions::from_mode(0o755)).unwrap();
    success(&f.run(&[]));
    let args = fs::read_to_string(f.dir.path().join("args")).unwrap();
    assert!(args.contains("-p\n512\n-n\n0\n"), "{args}");
    assert!(args.contains("-p\n0\n-n\n0\n-gp\n1,128\n"), "{args}");
    assert!(!args.contains("-d\n") && !args.contains("-w\n"), "{args}");
    let report: Value = serde_json::from_slice(&fs::read(&f.report).unwrap()).unwrap();
    assert_eq!(report["runtime"]["name"], "llama-cpp");
    assert_eq!(
        report["runtime"]["binary"]["descriptor"]["dialect"],
        "ik_llama.cpp"
    );
    let benchmark = &report["benchmark"];
    assert_eq!(benchmark["runtime_version"], "ik_llama.cpp b456 (def456)");
    assert_eq!(benchmark["protocol"]["id"], "computearena-throughput/2");
    assert_eq!(benchmark["params"]["decode_context_tokens"], 1);
    assert_eq!(benchmark["metrics"]["pp512_t_s"], 3840.0);
    assert_eq!(benchmark["metrics"]["decode_t_s"], 960.0);
    assert_eq!(benchmark["runtime_configuration"]["fused_moe"], true);
    success(&f.verify());

    fs::remove_file(&f.report).unwrap();
    fs::remove_file(f.dir.path().join("args")).unwrap();
    success(&f.run(&["--warmup", "0"]));
    let args = fs::read_to_string(f.dir.path().join("args")).unwrap();
    assert!(
        args.contains("-w\n0\n") && !args.contains("--no-warmup"),
        "{args}"
    );
}
