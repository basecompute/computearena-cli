use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

#[cfg(unix)]
#[test]
fn gguf_download_guidance_precedes_the_model_prompt_and_back_returns_to_menu() {
    use std::os::unix::fs::PermissionsExt;
    let data = tempfile::tempdir().unwrap();
    // Model selection now follows the runtime check, so a fake llama-bench
    // that only answers the capability probe stands in for llama.cpp.
    let bin = data.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let executable = bin.join("llama-bench");
    fs::write(
        &executable,
        "#!/bin/sh\nprintf '%s\\n' '--n-prompt --n-gen --n-depth --repetitions --no-warmup json'\n",
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_computearena"))
        .args(["llama-cpp", "--data-dir"])
        .arg(data.path().join("data"))
        .args(["--api-url", "http://127.0.0.1:1/api/v1"])
        .env("NO_COLOR", "1")
        .env("PATH", &bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Open model selection, return without a model, then exit. No benchmark runs.
    child.stdin.take().unwrap().write_all(b"1\n0\n6\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    let prompt = text
        .find("GGUF model path (absolute, relative, or ~/; 0 to go back):")
        .unwrap();
    for instruction in [
        "Need a GGUF model for llama.cpp?",
        "https://huggingface.co/models?library=gguf",
        "hf download <repo-id> <filename.gguf> --local-dir ./models",
        "https://huggingface.co/docs/huggingface_hub/guides/cli",
        "Files and versions",
        "local path below",
    ] {
        assert!(text.find(instruction).unwrap() < prompt);
    }
    assert_eq!(text.matches("Choose an option:").count(), 2);
    assert!(text.contains("Found llama.cpp on PATH"));
    assert!(!text.contains('\u{1b}'));
    assert!(!text.contains("Running the benchmark"));
}
