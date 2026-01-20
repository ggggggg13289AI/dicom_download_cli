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
3. Make sure the Orthanc instances you interact with advertise the AETs referenced by the CLI:
   - `modality` must match a remote AET entry that exists on the Orthanc serving the `--url` argument.
   - `target` is the AET name by which that remote Orthanc refers to your local destination Orthanc (i.e., how the remote server sees the Orthanc described by `--url`).
   - Both PACS configurations must be prepared (AET defined, network access allowed) before issuing MOVE jobs so the transfer succeeds.
4. From the workspace root:
   ```bash
   cd dicom_download_cli
   cargo run -- <input_path> [--url <orthanc>] [--analyze-url <analyze>] [--concurrency <n>]
   ```
5. Provide either a CSV (with `AccessionNumber` header) or JSON file as the `input_path`. The CLI emits progress to stderr and writes the success/failure report to the path provided via `--report-path`.

## Configuration reference

- `enable_whitelist`: `false` disables Analyze-driven filtering.
- `enable_direct_keywords`: `false` disables direct keyword matches.
- `download_all`: `true` always downloads every candidate series.
- `series_whitelist` / `direct_download_keywords`: the sets the CLI consults before downloading series.

## Documentation & reference

- Review `docs/dicom_download.md` for CLI argument definitions, concurrency guidance, and the decision matrix that keeps the core logic deterministic.
- `scripts/dicom_download.py` preserves the original Python workflow (Orthanc API usage, Analyze call, job monitoring) if you need to compare behaviors.

## Contribution notes

1. Keep the core logic pure (no HTTP/file IO within decision helpers).
2. Route all side effects through `OrthancClient`/`tokio::task`s and keep CLI flags documented in the spec.
3. Use `buffer_unordered(concurrency)` when processing accessions to limit Orthanc load.

---

# Mini-PACS DICOM 批次下載器（繁體中文）

本專案為基於 Orthanc 的 DICOM 批次下載工具。Rust 實作的 `dicom_download_cli` 套件依據 `docs/dicom_download.md` 描述的非同步 CLI 工作流程設計；原始的 Python 助手仍保留在 `scripts/`，作為參考或未來遷移的依據。

## 儲存庫結構

- `dicom_download_cli/`：採用 Functional Core / Imperative Shell 架構的 Rust CLI 工程，使用 `tokio`、`reqwest`、`clap`、`indicatif` 等函式庫，以確保跨平台非同步流程。
- `dicom_download_cli/src/`：CLI 入口與核心邏輯。
- `dicom_download_cli/config/dicom_download_cli.toml`：可選的執行時設定檔（白名單、指定關鍵字、下載開關）。
- `docs/dicom_download.md`：Rust CLI 所依循的功能規格（輸入格式、參數、決策規則、報表輸出）。
- `scripts/dicom_download.py`：提供原始 Python 工作流程，方便行為對照。

## 快速開始

1. 安裝 Rust（2021 edition），並確認 `cargo` 已加入系統 `PATH`。
2. 依需求編輯 `dicom_download_cli/config/dicom_download_cli.toml`，調整白名單、關鍵字等行為。
3. 操作前請先在相關 Orthanc 上設定好 AET：
   - `modality` 需與你透過 `--url` 指向的 Orthanc 中所定義的遠端 AET 相符。
   - `target` 則是該遠端 Orthanc 用來識別本地 Orthanc 的 AET 名稱（也就是該 Orthanc 在接收 MOVE 任務時，用來稱呼 `--url` 所指的實體）。
   - 確保雙方 PACS 均已註冊這些 AET 並能互相連線，才能讓 MOVE job 執行成功。
4. 於專案根目錄執行：
   ```bash
   cd dicom_download_cli
   cargo run -- <input_path> [--url <orthanc>] [--analyze-url <analyze>] [--concurrency <n>]
   ```
5. `input_path` 可指定含有 `AccessionNumber` 欄位的 CSV，或符合格式的 JSON 陣列；CLI 會於 stderr 顯示進度，並把成功/失敗報告寫入 `--report-path`。

## 設定檔參考

- `enable_whitelist`: 設為 `false` 則停用 Analyze 驅動的過濾。
- `enable_direct_keywords`: 設為 `false` 則停用關鍵字直下載判斷。
- `download_all`: 設為 `true` 則忽略分析，直接下載所有候選 Series。
- `series_whitelist` / `direct_download_keywords`: CLI 判斷是否下載時會參考的關鍵字集合。

## 文件與參考

- 詳閱 `docs/dicom_download.md`，了解 CLI 參數定義、併發控制與決策流程，以維持核心邏輯的確定性。
- `scripts/dicom_download.py` 保留了原始的 Python 實作（Orthanc API、Analyze 呼叫、工作監控），可用來比較行為差異。

## 貢獻須知

1. 保持核心邏輯為純函數（決策、狀態處理）：不要在決策 helper 中直接進行 HTTP / 檔案 IO。
2. 所有副作用（如網路呼叫、檔案操作）統一封裝在 `OrthancClient`、`tokio::task` 等層，CLI 旗標需要在規格中清楚紀錄。
3. 處理 Accession 時請使用 `buffer_unordered(concurrency)` 控制併發，以免超載 Orthanc。
