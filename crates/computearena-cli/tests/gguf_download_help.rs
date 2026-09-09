use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn gguf_download_guidance_precedes_the_model_prompt_and_back_returns_to_menu() {
    let data = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_computearena"))
        .args(["llama-cpp", "--data-dir"])
        .arg(data.path())
        .args(["--api-url", "http://127.0.0.1:1/api/v1"])
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Open model selection, return without a model, then exit. No runtime needed.
    child.stdin.take().unwrap().write_all(b"2\n0\n6\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    let prompt = text.find("GGUF model path (or 0 to go back):").unwrap();
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
    assert!(!text.contains('\u{1b}'));
    assert!(!text.contains("Running the benchmark"));
}
