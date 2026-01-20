//! CLI wrapper around Orthanc that downloads DICOM series referenced by accession numbers.
//! 
//! It batches accessions from CSV/JSON, consults Orthanc and an optional analysis service,
//! and writes success/failure reports in CSV/JSON formats.
mod client;
mod config;
mod processor;

use anyhow::{Context, Result};
use clap::Parser;
use futures::stream::{self, StreamExt};
use indicatif::MultiProgress;
use std::path::PathBuf;
use std::sync::Arc;

use crate::client::OrthancClient;
use crate::config::{
    load_runtime_config, sanitize_optional_string, AnalysisConfig, EffectiveConfig,
    RuntimeConfigFile, DEFAULT_CONFIG_PATH,
};
use crate::processor::{process_single_accession, write_reports, ProcessResult};

#[derive(Parser)]
#[command(name = "dicom_download_cli")]
#[command(about = "Orthanc DICOM Batch Downloader", long_about = None)]
/// CLI flags that override runtime configuration before downloads begin.
struct Cli {
    /// Modality AET used for Orthanc queries (defaults to the configured value).
    #[arg(long, help = "DICOM Modality AET (e.g., INFINTT-SERVER)")]
    modality: Option<String>,

    /// Target AET that receives the pushed series (e.g., ORTHANC or RADAX).
    #[arg(long, help = "Target AET (e.g., ORTHANC | RADAX)")]
    target: Option<String>,

    /// Orthanc HTTP base URL (e.g., http://host:8042/).
    #[arg(long, help = "Orthanc Base URL")]
    url: Option<String>,

    /// Analysis service endpoint that classifies sampled series.
    #[arg(long, help = "Analysis Service URL")]
    analyze_url: Option<String>,

    /// HTTP basic auth username for Orthanc.
    #[arg(long)]
    username: Option<String>,

    /// HTTP basic auth password for Orthanc.
    #[arg(long)]
    password: Option<String>,

    /// Path to the CSV or JSON file listing accession numbers to process.
    #[arg(short, long)]
    input: PathBuf,

    /// Optional runtime config in TOML that supplies defaults for the CLI.
    #[arg(short, long, help = "TOML config file")]
    config: Option<PathBuf>,

    /// Optional destination for the CSV output report.
    #[arg(long)]
    report_csv: Option<PathBuf>,

    /// Optional destination for the JSON output report.
    #[arg(long)]
    report_json: Option<PathBuf>,

    /// Maximum number of concurrent accession downloads used for buffering.
    #[arg(short, long)]
    concurrency: Option<usize>,
}

/// Entrypoint that wires CLI args, runtime config, Orthanc client, and processor workers.
///
/// It loads overrides, creates the HTTP client, parses accessions, runs bounded async workers,
/// waits for them, then writes CSV/JSON reports and prints a summary.
#[tokio::main]
async fn main() -> Result<()> {
    let args = Cli::parse();
    let cfg_path = args.config.clone().unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));
    
    let runtime_file = load_runtime_config(Some(&cfg_path))?;
    let effective = merge_config(&args, runtime_file);

    let client = Arc::new(OrthancClient::new(
        &effective.url,
        &effective.analyze_url,
        &effective.target,
        effective.username.clone(),
        effective.password.clone(),
    )?);

    let accessions = config::parse_input_file(&args.input).context("Parse input failed")?;
    let analysis_config = Arc::new(AnalysisConfig::load(Some(&cfg_path))?);
    let mp = Arc::new(MultiProgress::new());

    println!("Processing {} accessions...", accessions.len());

    let results: Vec<ProcessResult> = stream::iter(accessions)
        .map(|acc| {
            let client = client.clone();
            let modality = effective.modality.clone();
            let mp = mp.clone();
            let config = analysis_config.clone();
            async move { process_single_accession(client, acc, modality, mp, config).await }
        })
        .buffer_unordered(effective.concurrency)
        .collect()
        .await;

    write_reports(&effective.report_csv, &effective.report_json, &results)?;
    
    let ok = results.iter().filter(|r| r.status == "Success").count();
    println!("Summary: {} Success, {} Failed/Partial.", ok, results.len() - ok);

    Ok(())
}

/// Merge CLI overrides with a parsed runtime config, falling back to crate defaults.
///
/// CLI flags take precedence, followed by the runtime file, and finally `EffectiveConfig::defaults()`.
fn merge_config(cli: &Cli, file: Option<RuntimeConfigFile>) -> EffectiveConfig {
    let mut cfg = EffectiveConfig::defaults();
    let f = file.unwrap_or_default();

    cfg.url = cli.url.clone().or(f.url).unwrap_or(cfg.url);
    cfg.analyze_url = cli.analyze_url.clone().or(f.analyze_url).unwrap_or(cfg.analyze_url);
    cfg.modality = cli.modality.clone().or(f.modality).unwrap_or(cfg.modality);
    cfg.target = cli.target.clone().or(f.target).unwrap_or(cfg.target);
    cfg.concurrency = cli.concurrency.or(f.concurrency).unwrap_or(cfg.concurrency);
    cfg.report_csv = cli.report_csv.clone().or(f.report_csv).unwrap_or(cfg.report_csv);
    cfg.report_json = cli.report_json.clone().or(f.report_json).unwrap_or(cfg.report_json);
    cfg.username = sanitize_optional_string(cli.username.clone()).or(sanitize_optional_string(f.username));
    cfg.password = sanitize_optional_string(cli.password.clone()).or(sanitize_optional_string(f.password));

    cfg
}
