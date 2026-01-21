# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Mini-PACS DICOM Batch Downloader - A Rust CLI tool for batch downloading DICOM files from Orthanc PACS servers. Supports two workflows: C-MOVE (push to target AET) and direct file download to local filesystem.

## Build and Run Commands

```bash
cd dicom_download_cli

# Build
cargo build
cargo build --release

# Run C-MOVE workflow (push to target AET)
cargo run -- remote -i <input.csv> [--url <orthanc>] [--modality <AET>] [--target <AET>]

# Run direct download workflow (save to local directory)
cargo run -- download -i <input.csv> --output <dir> [--url <orthanc>]

# Check/lint
cargo check
cargo clippy

# Format
cargo fmt
```

## Architecture

**Functional Core / Imperative Shell Pattern**: Core logic is pure functions with no IO; all side effects (HTTP, file IO) are isolated in the shell layer.

### Module Structure (`dicom_download_cli/src/`)

- **main.rs**: CLI entry point using `clap`. Defines two subcommands (`remote`, `download`), merges config precedence (CLI > TOML > defaults), orchestrates async workers with `buffer_unordered(concurrency)`.

- **client.rs**: `OrthancClient` - HTTP client for Orthanc REST API. Handles C-FIND queries, C-MOVE jobs, instance downloads, and Analyze API calls. Uses `reqwest` with optional Basic auth.

- **config.rs**: Configuration loading and parsing. Defines `AnalysisConfig` (whitelists, keywords), `RuntimeConfigFile` (TOML schema), `EffectiveConfig` (merged result). Contains `should_download()` decision function and input file parsers (CSV/JSON).

- **processor.rs**: Remote C-MOVE workflow implementation. Processes accessions through study lookup → series filtering → sample analysis → series download with progress tracking via `indicatif`.

### Config Precedence

CLI flags → `config/dicom_download_cli.toml` → Code defaults

### Key Data Flow

1. Parse accessions from CSV/JSON input (`AccessionNumber`/`accession`/`acc` columns)
2. Query Orthanc for studies by accession number
3. For each series: check direct keywords → sample and analyze → check whitelist → download
4. Write reports to CSV/JSON

### Async Concurrency

Uses `futures::stream::buffer_unordered(concurrency)` for bounded parallel processing of accessions and instances. Default concurrency is 5.

## Configuration

Runtime config file: `dicom_download_cli/config/dicom_download_cli.toml`

Key settings:
- `download_all`: Bypass all filtering, download everything
- `enable_whitelist`/`enable_direct_keywords`: Toggle filtering modes
- `series_whitelist`: Analyze API result types to download
- `direct_download_keywords`: Series descriptions to download without analysis

## AET Configuration

Before running, ensure Orthanc AET configuration:
- `--modality`: Remote AET registered in your Orthanc (e.g., `INFINTT-SERVER`)
- `--target`: How the remote Orthanc identifies your local Orthanc for C-MOVE
- Both PACS must have each other registered for C-MOVE to succeed

## Input Formats

**CSV**: Reads `AccessionNumber`, `accession`, or `acc` column (case-insensitive), falls back to first column

**JSON**: Array of strings `["A001", "A002"]` or objects `[{"AccessionNumber": "A001"}]`

## Dependencies

Key crates: `tokio` (async runtime), `reqwest` (HTTP), `clap` (CLI), `indicatif` (progress), `dicom-object` (DICOM parsing), `serde`/`csv` (serialization)