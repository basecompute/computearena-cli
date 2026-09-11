# Model identity and provenance

ComputeArena needs to compare the same upstream model across runtimes without
pretending that converted files or quantizations are interchangeable. Every new
report therefore separates:

- `model.canonical`: the upstream Hugging Face model class, such as
  `Qwen/Qwen3-4B`;
- `model.artifact`: the repository, immutable revision, file path, format,
  runtime-namespaced quantization, and SHA-256 of the exact bytes used; and
- `model.provenance`: how those identities were obtained and verified.

The signed identity schema is `computearena-model/1`. The released compatibility
fields (`upstream_id`, `upstream_id_source`, `identity_verification`, and
`artifact_sha256`) remain populated so older servers can accept new reports.

## Managed downloads

For GGUF, ComputeArena resolves the repository's current ref to an immutable
commit, reads the publisher's `base_model` metadata, downloads that exact file,
hashes it while streaming, and compares it with the Hub's LFS object ID. A
content-addressed receipt is saved under `model-provenance/`.

For BaseRT, ComputeArena delegates to `basert pull`. It then hashes the installed
`.base` file, reads BaseRT's adjacent `hub.json` (the artifact repository, the
source repository, and usually a mutable ref, but never a file name), and looks
the hash up in the repository's file listing to recover the exact published
file, saving a receipt when it finds one. A model installed before ComputeArena
tracked provenance is matched the same way the first time it is benchmarked.
When no file matches (offline, or a locally converted artifact that was never
published), the report carries the repository and the hash but no file name;
the server then matches the hash against artifacts it has already verified
instead of being told a path that was never checked. Multiple internal
catalogue variants that map to one `basert pull --target` choice are shown
once; BaseRT selects the compatible artifact for the current backend.

## Models acquired elsewhere

Standard Hugging Face cache paths and embedded GGUF source URLs are retained as
evidence, but missing or ambiguous identity stays unresolved. A user can prove
that a copied or renamed file is one exact Hub object with:

```sh
computearena identify /path/to/model.base \
  https://huggingface.co/owner/repository/blob/<revision>/path/to/model.base
```

The command resolves the revision, fetches only file metadata, hashes the local
file, and saves a receipt only if the SHA-256 values match. A repository URL or
free-form model name is intentionally insufficient. Receipts are keyed by
SHA-256 and replaced when a later download or `identify` learns more about the
same bytes, so a corrected repository or model family takes effect on the next
benchmark.

## Model families

A repository's card is followed to an upstream model only when it declares
itself a quantization of that model (the Hub's `base_model:quantized:`
relation). Finetunes, adapters, and merges are different models, so an
instruct model stays distinct from the pretraining base its card links to. The
server applies the same rule and additionally follows a link only from
publishers it trusts to quantize or convert models; its decision wins, and a
difference between the CLI's recorded family and the server's grouping never
blocks a submission.

## Trust boundary

This proves which public artifact bytes were selected and preserves the
publisher's model lineage claim. It does not prove that a publisher labelled a
model correctly, that the runtime executed those bytes honestly, or that two
fine-tunes or merges with different lineage are equivalent. The server must
keep artifact and quantization dimensions when grouping by canonical model
class.
