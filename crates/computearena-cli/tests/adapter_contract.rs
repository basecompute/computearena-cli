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
        "Standard — native warmup",
        "Thermally controlled",
        "30s–9m 0s",
        "1m 30s",
        "reloads the model",
        "Total time =",
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
        .write_all(b"2\n/definitely-missing-computearena-model.gguf\n6\n")
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
    assert_eq!(telemetry["observer"]["requested_interval_ms"], 1000);
    assert!(
        telemetry["process_memory"]["statistics"]["sample_count"]
            .as_u64()
            .unwrap()
            >= 1
    );
    assert_eq!(
        report["benchmark"]["memory"]["process_peak_rss_mb"],
        telemetry["process_memory"]["statistics"]["peak"]
    );
    assert_eq!(telemetry["energy"]["available"], false);
    assert_eq!(telemetry["per_workload"]["available"], false);
    success(&f.verify());
    if let Some(path) = std::env::var_os("COMPUTEARENA_TELEMETRY_TEST_REPORT") {
        fs::copy(&f.report, path).unwrap();
    }
    report["benchmark"]["telemetry"]["observer"]["requested_interval_ms"] = json!(25);
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
            "Runtime:",
            "Model:",
            "Prefill:",
            "Decode:",
            "Sampling:",
            "Telemetry:",
            "Input:",
            "Output:",
            "Run profile",
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
fn systemic_submission_failure_stops_the_queue_but_report_rejection_does_not() {
    for status in [422, 429, 500] {
        let f = Fixture::new("basert");
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

struct Fixture {
    dir: tempfile::TempDir,
    runtime: &'static str,
    executable: PathBuf,
    model: PathBuf,
    report: PathBuf,
}

impl Fixture {
    fn new(runtime: &'static str) -> Self {
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
        };
        fixture.install(&fixture.result(), "");
        fixture
    }

    fn result(&self) -> Value {
        if self.runtime == "basert" {
            json!({"schema":"basert-benchmark-harness/1","mode":"text","runtime_version":"0.2.4",
                "chip":"Test CPU","backend":"CPU","params":{"pp":"128,512","tg":128,"reps":2},
                "telemetry":{"schema":"basert-telemetry/3"},
                "metrics":{"pp128_t_s":960.0,"pp512_t_s":3840.0,"decode_t_s":960.0},
                "raw_samples":{"prefill":{
                    "128":[{"tokens":128,"elapsed_ns":100000000},{"tokens":128,"elapsed_ns":200000000}],
                    "512":[{"tokens":512,"elapsed_ns":100000000},{"tokens":512,"elapsed_ns":200000000}]},
                    "decode":[{"generated_tokens":128,"elapsed_ns":100000000},{"generated_tokens":128,"elapsed_ns":200000000}]}})
        } else {
            Value::Array([(128,0),(512,0),(0,128)].into_iter().map(|(pp,tg)| json!({
                "build_commit":"abc123","build_number":123,"model_type":"Qwen3 Q4_K_M",
                "model_filename":self.model,"model_size":24,"model_n_params":4000000000u64,
                "n_prompt":pp,"n_gen":tg,"n_depth":0,"n_gpu_layers":0,"backends":"CPU",
                "cpu_info":"Test CPU","gpu_info":"","samples_ns":[100000000,200000000],
                // Reported aggregates are deliberately bogus: native samples are authoritative.
                "avg_ts":999999.0,"avg_ns":1
            })).collect())
        }
    }

    fn install(&self, result: &Value, before_result: &str) {
        let descriptor = json!({"schema":"basert-benchmark-harness-descriptor/1",
            "runtime":{"name":"basert","version":"0.2.4"},"result_schema":"basert-benchmark-harness/1"});
        let script = format!("#!/bin/sh\ncase \"$1\" in\n describe) printf '%s\\n' '{descriptor}';;\n --help) printf '%s\\n' '--n-prompt --n-gen --n-depth --repetitions --no-warmup json';;\n *) printf '%s\\n' \"$@\" > \"$ARENA_TEST_ARGS\"\n{before_result}\n/bin/cat <<'RESULT'\n{result}\nRESULT\n;;\nesac\n");
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
            .stdin(Stdio::null());
        cmd
    }

    fn run(&self, extra: &[&str]) -> Output {
        self.command()
            .arg(self.runtime)
            .arg("run")
            .arg(&self.model)
            .args([
                "--pp", "128,512", "--tg", "128", "--reps", "2", "--yes", "--output",
            ])
            .arg(&self.report)
            .args(extra)
            .output()
            .unwrap()
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
    assert_eq!(b["benchmark"]["params"]["decode_context_tokens"], 0);
    assert_eq!(b["benchmark"]["protocol"]["telemetry_available"], true);
    assert_eq!(
        b["benchmark"]["telemetry"]["scope"],
        "whole_runtime_process"
    );
    assert!(b["benchmark"]["telemetry"].get("memory_replay").is_none());
    assert!(a["benchmark"].get("telemetry").is_some());
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
        assert!(args.contains("\n128,512\n"));
        assert!(args.contains(fixture.model.to_str().unwrap()));
        assert!(!args.contains("--cooldown"));
        if fixture.runtime == "llama-cpp" {
            assert!(args.contains("-d\n0\n"));
            assert!(args.contains("-o\njson\n"));
            assert!(!args.contains("--telemetry"));
        } else {
            assert!(args.contains("--telemetry"));
        }
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
            let body = json!({"id":"test-submission","runtime_provenance":{
                "status":"mismatch","message":"Benchmark accepted. This binary differs from the registered release.",
                "download_url":"https://github.com/ggml-org/llama.cpp/releases"}}).to_string();
            write!(stream,"HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
        received
    });
    (url, handle)
}

#[test]
fn checksum_mismatch_is_informational_and_duplicate_submission_is_not_an_error() {
    for runtime in RUNTIMES {
        let f = Fixture::new(runtime);
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
    let f = Fixture::new("llama-cpp");
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
fn offline_report_cannot_be_uploaded_without_login() {
    let f = Fixture::new("basert");
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
