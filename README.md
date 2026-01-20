# Mini-PACS DICOM Batch Downloader

A focused workspace for the Orthanc-based DICOM download tool. The Rust-based `dicom_download_cli` crate implements the async CLI described in `docs/dicom_download.md`, while the original Python helper stays under `scripts/` for reference and migration guidance.

## Repository layout

- `dicom_download_cli/`: Rust CLI crate (`tokio`, `reqwest`, `clap`, `indicatif`) that follows the Functional Core / Imperative Shell pattern described in `docs/dicom_download.md`.
- `dicom_download_cli/src/`: CLI entry point and core logic.
- `dicom_download_cli/config/dicom_download_cli.toml`: Optional runtime configuration (whitelist, direct keywords, download flags).
- `docs/dicom_download.md`: Feature specification that the Rust implementation targets (input formats, arguments, decision rules, reporting).
- `scripts/dicom_download.py`: Reference Python implementation which inspired the current Rust workflow.

## Getting started

1. Install Rust (2021 edition) and ensure `cargo` is on your `PATH`.
2. Configure `dicom_download_cli/config/dicom_download_cli.toml` as needed before running the CLI.
3. From the workspace root:
   ```bash
   cd dicom_download_cli
   cargo run -- <input_path> [--url <orthanc>] [--analyze-url <analyze>] [--concurrency <n>]
   ```
4. Provide either a CSV (with `AccessionNumber` header) or JSON file as the `input_path`. The CLI emits progress to stderr and writes the success/failure report to the path provided via `--report-path`.

## Configuration reference

- ` enable_whitelist`: `false` disables Analyze-driven filtering.
- ` enable_direct_keywords`: `false` disables direct keyword matches.
- ` download_all`: `true` always downloads every candidate series.
- ` series_whitelist` / `direct_download_keywords`: the sets the CLI consults before downloading series.

## Documentation & reference

- Review `docs/dicom_download.md` for CLI argument definitions, concurrency guidance, and the decision matrix that keeps the core logic deterministic.
- `scripts/dicom_download.py` preserves the original Python workflow (Orthanc API usage, Analyze call, job monitoring) if you need to compare behaviors.

## Contribution notes

1. Keep the core logic pure (no HTTP/file IO within decision helpers).
2. Route all side effects through `OrthancClient`/`tokio::task`s and keep CLI flags documented in the spec.
3. Use `buffer_unordered(concurrency)` when processing accessions to limit Orthanc load.
