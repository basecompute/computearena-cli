use serde_json::{json, Value};
use std::fs;
use std::process::Command;

#[cfg(unix)]
#[test]
fn llama_cpp_offline_run_signs_runtime_identity_and_remains_verifiable_after_upgrade() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let model = dir.path().join("Qwen3-4B.gguf");
    let mut header = Vec::from(*b"GGUF");
    header.extend_from_slice(&3_u32.to_le_bytes());
    header.extend_from_slice(&0_u64.to_le_bytes());
    header.extend_from_slice(&0_u64.to_le_bytes());
    fs::write(&model, header).unwrap();

    let rows: Vec<Value> = [(128, 0), (512, 0), (0, 128)]
        .into_iter()
        .map(|(pp, tg)| {
            json!({
                "build_commit":"abc123", "build_number":123, "model_type":"Qwen3 Q4_K_M",
                "model_filename":model, "model_size":24, "model_n_params":4000000000u64,
                "n_prompt":pp, "n_gen":tg, "n_depth":0, "n_gpu_layers":99,
                "gpu_info":"Apple M5 Pro", "cpu_info":"Apple M5 Pro", "backends":"Metal",
                "samples_ns":[100000000,200000000]
            })
        })
        .collect();
    let executable = dir.path().join("llama-bench");
    fs::write(&executable, format!(
        "#!/bin/sh\nif [ \"$1\" = --help ]; then\nprintf '%s\\n' '--n-prompt --n-gen --n-depth --repetitions --no-warmup json'\nelse\ncat <<'JSON'\n{}\nJSON\nfi\n",
        serde_json::to_string(&rows).unwrap()
    )).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    let report = dir.path().join("report.json");
    let output = Command::new(env!("CARGO_BIN_EXE_computearena"))
        .args(["llama-cpp", "--runtime-path"])
        .arg(&executable)
        .arg("--data-dir")
        .arg(dir.path().join("data"))
        .arg("run")
        .arg(&model)
        .args(["--pp", "128,512", "--reps", "2", "--yes", "--output"])
        .arg(&report)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&fs::read(&report).unwrap()).unwrap();
    assert_eq!(value["runtime"]["name"], "llama-cpp");
    assert_eq!(value["benchmark"]["schema"], "computearena-measurements/1");
    assert_eq!(
        value["runtime"]["binary"]["sha256"].as_str().unwrap().len(),
        64
    );
    assert!(!fs::read_to_string(&report)
        .unwrap()
        .contains(dir.path().to_str().unwrap()));
    fs::write(&executable, "updated binary").unwrap();
    let verified = Command::new(env!("CARGO_BIN_EXE_computearena"))
        .arg("verify")
        .arg(&report)
        .output()
        .unwrap();
    assert!(
        verified.status.success(),
        "{}",
        String::from_utf8_lossy(&verified.stderr)
    );
    if let Some(path) = std::env::var_os("COMPUTEARENA_TEST_REPORT") {
        fs::copy(&report, path).unwrap();
    }
    let mut changed = value;
    changed["runtime"]["binary"]["sha256"] = json!("0".repeat(64));
    fs::write(&report, serde_json::to_vec(&changed).unwrap()).unwrap();
    let verified = Command::new(env!("CARGO_BIN_EXE_computearena"))
        .arg("verify")
        .arg(&report)
        .output()
        .unwrap();
    assert!(!verified.status.success());
    assert!(String::from_utf8_lossy(&verified.stderr).contains("signature verification failed"));
}

#[cfg(unix)]
#[test]
fn basert_uses_the_same_signed_binary_identity_flow() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let model = dir.path().join("model.base");
    let metadata =
        serde_json::to_vec(&json!({"schema":7,"arch":"qwen","quant_scheme":"base_q4"})).unwrap();
    let mut header = Vec::from(*b"BASE");
    header.extend_from_slice(&1_u32.to_le_bytes());
    header.extend_from_slice(&(metadata.len() as u64).to_le_bytes());
    header.extend(metadata);
    fs::write(&model, header).unwrap();
    let descriptor = json!({"schema":"basert-benchmark-harness-descriptor/1",
        "runtime":{"name":"basert","version":"0.2.4"},"result_schema":"basert-benchmark-harness/1",
        "telemetry_schema":"basert-telemetry/3",
        "features":{"telemetry":true,"same_run_telemetry":false}});
    let result = json!({"schema":"basert-benchmark-harness/1","mode":"text","runtime_version":"0.2.4",
        "chip":"Apple M5 Pro","backend":"metal","params":{"pp":"512","tg":128},
        "metrics":{"pp512_t_s":5120.0,"decode_t_s":128.0},
        "raw_samples":{"prefill":{"512":[{"tokens":512,"elapsed_ns":100000000}]},
            "decode":[{"generated_tokens":128,"elapsed_ns":1000000000}]}});
    let executable = dir.path().join("basert-benchmark-harness");
    fs::write(&executable, format!("#!/bin/sh\nif [ \"$1\" = describe ]; then\nprintf '%s\\n' '{}'\nelse\nprintf '%s\\n' '{}'\nfi\n",
        descriptor, result)).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    let report = dir.path().join("report.json");
    let output = Command::new(env!("CARGO_BIN_EXE_computearena"))
        .args(["basert", "--runtime-path"])
        .arg(&executable)
        .arg("--data-dir")
        .arg(dir.path().join("data"))
        .arg("run")
        .arg(&model)
        .args(["--pp", "512", "--reps", "1", "--yes", "--output"])
        .arg(&report)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&fs::read(&report).unwrap()).unwrap();
    assert_eq!(value["runtime"]["name"], "basert");
    assert_eq!(
        value["runtime"]["binary"]["sha256"].as_str().unwrap().len(),
        64
    );
    assert_eq!(value["runtime"]["binary"]["version"], "0.2.4");
    assert_eq!(
        value["benchmark"]["telemetry"]["schema"],
        "computearena-telemetry/1"
    );
    let verified = Command::new(env!("CARGO_BIN_EXE_computearena"))
        .arg("verify")
        .arg(report)
        .output()
        .unwrap();
    assert!(verified.status.success());
}
