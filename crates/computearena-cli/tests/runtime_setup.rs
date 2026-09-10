//! Runtime selection feedback: numbered choices, the executable that will
//! run, installation guidance, and installs from a local bundle. No network,
//! GPU, or real runtime is used; fake executables are POSIX shell scripts.
#![cfg(unix)]

use serde_json::{json, Value};
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("arena setup ")
            .tempdir()
            .unwrap();
        fs::create_dir_all(dir.path().join("bin")).unwrap();
        Self { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// A shell script that answers the probes of both adapters and returns a
    /// canned llama-bench measurement for runs.
    fn fake_runtime(&self, at: &Path) {
        let descriptor = json!({"schema":"basert-benchmark-harness-descriptor/1",
            "runtime":{"name":"basert","version":"0.2.4"},"result_schema":"basert-benchmark-harness/1"});
        let rows: Vec<Value> = [(128, 0), (512, 0), (0, 128)]
            .into_iter()
            .map(|(pp, tg)| {
                json!({"build_commit":"abc123","build_number":123,"model_type":"Qwen3 Q4_K_M",
                    "model_filename":"model.gguf","model_size":24,"model_n_params":4000000000u64,
                    "n_prompt":pp,"n_gen":tg,"n_depth":0,"n_gpu_layers":0,"backends":"CPU",
                    "cpu_info":"Test CPU","gpu_info":"","samples_ns":[100000000,200000000]})
            })
            .collect();
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in\n describe) printf '%s\\n' '{descriptor}';;\n --help) printf '%s\\n' '--n-prompt --n-gen --n-depth --repetitions --no-warmup json';;\n *) /bin/cat <<'RESULT'\n{}\nRESULT\n;;\nesac\n",
            Value::Array(rows)
        );
        fs::create_dir_all(at.parent().unwrap()).unwrap();
        fs::write(at, script).unwrap();
        fs::set_permissions(at, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn gguf(&self) -> PathBuf {
        let model = self.path().join("Qwen3.gguf");
        let mut header = b"GGUF".to_vec();
        header.extend(3_u32.to_le_bytes());
        header.extend(0_u64.to_le_bytes());
        header.extend(0_u64.to_le_bytes());
        fs::write(&model, header).unwrap();
        model
    }

    /// A release-style bundle: one top-level folder holding the executable.
    fn bundle(&self, name: &str, entry: &str) -> PathBuf {
        let executable = self.path().join("bundle-src").join(entry);
        self.fake_runtime(&executable);
        let archive = self.path().join(name);
        let file = File::create(&archive).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        builder.append_path_with_name(&executable, entry).unwrap();
        builder.into_inner().unwrap().finish().unwrap();
        archive
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_computearena"));
        cmd.env_clear()
            .env("HOME", self.path())
            .env("PATH", self.path().join("bin"))
            .env("NO_COLOR", "1")
            .env("NO_PROXY", "*")
            .env("BASERT_INSTALL_DIR", self.path().join("basert-home"))
            .env("COMPUTEARENA_API_URL", "http://127.0.0.1:1/api/v1")
            .arg("--data-dir")
            .arg(self.path().join("data"));
        cmd
    }

    fn interact(&self, args: &[&str], input: &str) -> Output {
        let mut child = self
            .command()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn line_index(text: &str, needle: &str) -> usize {
    text.lines()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("missing {needle:?} in:\n{text}"))
}

#[test]
fn the_runtime_chooser_lists_one_numbered_option_per_line() {
    let sandbox = Sandbox::new();
    let output = sandbox.interact(&[], "0\n");
    assert!(output.status.success(), "{}", text(&output));
    let text = text(&output);
    assert!(!text.contains('\u{1b}'));
    let heading = line_index(&text, "Choose a runtime");
    let basert = line_index(&text, "1. BaseRT");
    let llama = line_index(&text, "2. llama.cpp");
    let exit = line_index(&text, "0. Exit");
    assert!(heading < basert && basert < llama && llama < exit);
    assert!(text
        .lines()
        .nth(basert)
        .unwrap()
        .trim_start()
        .starts_with("1. BaseRT"));
    assert!(text.contains("Runtime:"));
    assert!(!text.contains("Choose a runtime: 1."));
}

#[test]
fn choosing_a_runtime_reports_the_executable_that_will_run() {
    let sandbox = Sandbox::new();
    sandbox.fake_runtime(&sandbox.path().join("bin").join("llama-bench"));
    let output = sandbox.interact(&[], "llama-cpp\n6\n");
    assert!(output.status.success(), "{}", text(&output));
    let text = text(&output);
    let found = line_index(&text, "Found llama.cpp on PATH");
    let path = text.lines().nth(found + 1).unwrap();
    assert!(path.contains("bin/llama-bench"), "{path}");
    assert!(text.contains("Runtime: llama-cpp"));
    assert!(line_index(&text, "Runtime: llama-cpp") > found);
    for option in [
        "1. Log in",
        "2. Run benchmarks",
        "3. Submit previous benchmarks",
        "4. List local benchmarks",
        "5. Verify a local benchmark",
        "6. Exit",
    ] {
        assert!(text.contains(option), "{option}");
    }
    assert!(!text.contains("How to install"));
}

#[test]
fn a_missing_runtime_explains_how_to_get_it_and_offers_to_install() {
    let sandbox = Sandbox::new();
    // Choose llama.cpp, decline to install, check again, continue without it, then exit.
    let output = sandbox.interact(&[], "2\n2\n3\n6\n");
    assert!(output.status.success(), "{}", text(&output));
    let text = text(&output);
    assert!(text.contains("llama.cpp was not found"));
    assert!(text.contains("looked for llama-bench on PATH"));
    let how = line_index(&text, "How to install llama.cpp yourself");
    assert!(text.contains("brew install llama.cpp"));
    assert!(text.contains("https://github.com/ggml-org/llama.cpp/releases"));
    assert!(text.contains("--runtime-path /path/to/llama-bench"));
    let install = line_index(&text, "1. Install llama.cpp with ComputeArena");
    assert!(how < install);
    assert!(text.contains("Downloads the latest prebuilt build into ComputeArena's data directory"));
    assert!(text.contains("2. Check again"));
    assert!(text.contains("3. Continue without it"));
    assert!(text.contains("0. Back"));
    assert_eq!(text.matches("How to install llama.cpp yourself").count(), 2);
    assert!(text.contains("Runtime: llama-cpp · executable not set up yet"));
    assert!(text.contains("Choose an option:"));
}

#[test]
fn back_from_a_missing_runtime_returns_to_the_runtime_choice() {
    let sandbox = Sandbox::new();
    let output = sandbox.interact(&[], "1\n0\n0\n");
    assert!(output.status.success(), "{}", text(&output));
    let text = text(&output);
    assert!(text.contains("BaseRT was not found"));
    assert!(text.contains("curl -LsSf https://basecompute.co/install.sh | sh"));
    assert!(text.contains("basert-home") || text.contains("~/.basert"));
    assert_eq!(text.matches("Choose a runtime").count(), 2);
}

#[test]
fn a_named_runtime_session_without_the_executable_can_exit_from_the_options() {
    let sandbox = Sandbox::new();
    let output = sandbox.interact(&["basert"], "0\n");
    assert!(output.status.success(), "{}", text(&output));
    let text = text(&output);
    assert!(text.contains("BaseRT was not found"));
    assert!(!text.contains("Choose a runtime"));
    assert!(!text.contains("Run benchmarks"));
}

#[test]
fn installing_from_a_local_bundle_records_the_copy_and_runs_benchmarks_with_it() {
    let sandbox = Sandbox::new();
    let archive = sandbox.bundle(
        "llama-b777-bin-macos-arm64.tar.gz",
        "llama-b777/llama-bench",
    );
    let output = sandbox
        .command()
        .args(["llama-cpp", "install", "--yes", "--archive"])
        .arg(&archive)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output));
    let text_install = text(&output);
    assert!(text_install.contains("Install llama.cpp"));
    assert!(text_install.contains("Destination"));
    assert!(text_install.contains("runtimes/llama-cpp/b777"));
    assert!(text_install.contains("Found llama.cpp installed by ComputeArena"));

    let record: Value = serde_json::from_slice(
        &fs::read(sandbox.path().join("data/runtimes/llama-cpp.json")).unwrap(),
    )
    .unwrap();
    let executable = PathBuf::from(record["executable"].as_str().unwrap());
    assert!(executable.ends_with("runtimes/llama-cpp/b777/llama-b777/llama-bench"));
    assert!(executable.is_file());
    assert_eq!(record["version"], "b777");
    assert_eq!(record["asset"], "llama-b777-bin-macos-arm64.tar.gz");

    // The session now finds the installed copy without anything on PATH.
    let session = sandbox.interact(&["llama-cpp"], "6\n");
    assert!(session.status.success(), "{}", text(&session));
    assert!(text(&session).contains("Found llama.cpp installed by ComputeArena"));

    // And benchmarks run with it, signing the report as usual.
    let model = sandbox.gguf();
    let report = sandbox.path().join("report.json");
    let run = sandbox
        .command()
        .args(["llama-cpp", "run"])
        .arg(&model)
        .args(["--pp", "128,512", "--reps", "2", "--yes", "--output"])
        .arg(&report)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(run.status.success(), "{}", text(&run));
    let value: Value = serde_json::from_slice(&fs::read(&report).unwrap()).unwrap();
    assert_eq!(value["runtime"]["name"], "llama-cpp");
    assert!(text(&run).contains("llama-b777/llama-bench"));

    // Installing again replaces the previous copy instead of piling up folders.
    let newer = sandbox.bundle(
        "llama-b778-bin-macos-arm64.tar.gz",
        "llama-b778/llama-bench",
    );
    let again = sandbox
        .command()
        .args(["llama-cpp", "install", "--yes", "--archive"])
        .arg(&newer)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(again.status.success(), "{}", text(&again));
    assert!(!sandbox.path().join("data/runtimes/llama-cpp/b777").exists());
    assert!(sandbox.path().join("data/runtimes/llama-cpp/b778").exists());
}

#[test]
fn basert_installs_into_the_official_location_and_is_found_there() {
    let sandbox = Sandbox::new();
    let archive = sandbox.bundle(
        "basert-engine-macos-arm64-0.2.4.tar.gz",
        "basert-benchmark-harness",
    );
    let output = sandbox
        .command()
        .args(["basert", "install", "--yes", "--archive"])
        .arg(&archive)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output));
    let text_install = text(&output);
    assert!(text_install.contains("Install BaseRT"));
    assert!(text_install.contains("basert-home"));
    assert!(text_install.contains("Found BaseRT 0.2.4 installed by ComputeArena"));
    let home = sandbox.path().join("basert-home");
    assert!(home.join("basert-benchmark-harness").is_file());
    assert_eq!(
        fs::read_to_string(home.join(".release")).unwrap().trim(),
        "v0.2.4"
    );

    // Without the record, the official install location is still searched.
    fs::remove_file(sandbox.path().join("data/runtimes/basert.json")).unwrap();
    let session = sandbox.interact(&["basert"], "6\n");
    assert!(session.status.success(), "{}", text(&session));
    assert!(text(&session).contains("Found BaseRT 0.2.4 in its default install location"));
}

#[test]
fn install_refuses_to_guess_without_a_terminal_and_names_the_flag() {
    let sandbox = Sandbox::new();
    let archive = sandbox.bundle(
        "llama-b779-bin-macos-arm64.tar.gz",
        "llama-b779/llama-bench",
    );
    let output = sandbox
        .command()
        .args(["llama-cpp", "install", "--archive"])
        .arg(&archive)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(text(&output).contains("--yes"));
    assert!(!sandbox.path().join("data/runtimes/llama-cpp.json").exists());
}

#[test]
fn non_interactive_runs_without_a_runtime_point_at_install() {
    let sandbox = Sandbox::new();
    let model = sandbox.gguf();
    let output = sandbox
        .command()
        .args(["llama-cpp", "run"])
        .arg(&model)
        .args(["--yes"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let text = text(&output);
    assert!(text.contains("PATH"));
    assert!(text.contains("computearena llama-cpp install"));
}
