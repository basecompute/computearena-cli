//! Black-box contract tests: no installed runtime, GPU, account, or external network.
#![cfg(unix)]

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
        let (url, received) = server(if status == 422 {
            vec![status, 201]
        } else {
            vec![status]
        });
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
    assert_eq!(b["benchmark"]["protocol"]["telemetry_available"], false);
    assert!(b["benchmark"].get("telemetry").is_none());
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
    failure(&f.run(&["--cooldown"]), "Run without --cooldown");
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
fn server(statuses: Vec<u16>) -> (String, thread::JoinHandle<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/api/v1", listener.local_addr().unwrap());
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
                    assert!(!headers.contains("authorization:"));
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
        let (url, received) = server(vec![201, 200]);
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
    let (url, received) = server(vec![201]);
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
