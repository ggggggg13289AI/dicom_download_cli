//! CLI wrapper around Orthanc that downloads DICOM series referenced by accession numbers.
//! 
//! It batches accessions from CSV/JSON, consults Orthanc and an optional analysis service,
//! and writes success/failure reports in CSV/JSON formats.
mod client;
mod config;
mod processor;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use futures::stream::{self, StreamExt};
use indicatif::MultiProgress;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs;

use crate::client::OrthancClient;
use crate::config::{
    load_runtime_config, sanitize_optional_string, AnalysisConfig, EffectiveConfig,
    RuntimeConfigFile, DEFAULT_CONFIG_PATH,
};
use crate::processor::{process_single_accession, summarize_status, write_reports, ProcessResult};

#[derive(Parser)]
#[command(name = "dicom_download_cli")]
#[command(about = "Orthanc DICOM Batch Downloader", long_about = None)]
/// Entry CLI that dispatches to subcommands.
struct Cli {
    /// Optional runtime config in TOML that supplies defaults for the CLI.
    #[arg(short, long, help = "TOML config file")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Remote C-MOVE flow (maps to legacy dicom_download.py)
    Remote(RemoteArgs),
    /// Direct file download flow (maps to download_dicom_matt_async.py)
    Download(DownloadArgs),
}

#[derive(Args, Clone)]
struct SharedArgs {
    /// Path to the CSV or JSON file listing accession numbers to process.
    #[arg(short, long)]
    input: PathBuf,

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

#[derive(Args, Clone)]
struct RemoteArgs {
    #[command(flatten)]
    shared: SharedArgs,
}

#[derive(Args, Clone)]
struct DownloadArgs {
    #[command(flatten)]
    shared: SharedArgs,

    /// Directory to write downloaded DICOM files.
    #[arg(long, value_name = "DIR")]
    output: PathBuf,
}

/// Entrypoint that wires CLI args, runtime config, Orthanc client, and processor workers.
///
/// It loads overrides, creates the HTTP client, parses accessions, runs bounded async workers,
/// waits for them, then writes CSV/JSON reports and prints a summary.
#[tokio::main]
async fn main() -> Result<()> {
    let args = Cli::parse();
    let cfg_path = args.config.clone().unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));

    match args.command {
        Commands::Remote(cmd) => run_remote(cmd, &cfg_path).await,
        Commands::Download(cmd) => run_download(cmd, &cfg_path).await,
    }
}

/// Merge CLI overrides with a parsed runtime config, falling back to crate defaults.
///
/// CLI flags take precedence, followed by the runtime file, and finally `EffectiveConfig::defaults()`.
fn merge_config(cli: &SharedArgs, file: Option<RuntimeConfigFile>) -> EffectiveConfig {
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

async fn run_remote(args: RemoteArgs, cfg_path: &PathBuf) -> Result<()> {
    let runtime_file = load_runtime_config(Some(cfg_path))?;
    let effective = merge_config(&args.shared, runtime_file);

    let client = Arc::new(OrthancClient::new(
        &effective.url,
        &effective.analyze_url,
        &effective.target,
        effective.username.clone(),
        effective.password.clone(),
    )?);

    let accessions = config::parse_input_file(&args.shared.input).context("Parse input failed")?;
    let analysis_config = Arc::new(AnalysisConfig::load(Some(cfg_path))?);
    let mp = Arc::new(MultiProgress::new());

    println!("Processing {} accessions via remote C-MOVE...", accessions.len());

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

async fn run_download(args: DownloadArgs, cfg_path: &PathBuf) -> Result<()> {
    let runtime_file = load_runtime_config(Some(cfg_path))?;
    let effective = merge_config(&args.shared, runtime_file);

    let client = Arc::new(OrthancClient::new(
        &effective.url,
        &effective.analyze_url,
        &effective.target,
        effective.username.clone(),
        effective.password.clone(),
    )?);

    let accessions = config::parse_input_file(&args.shared.input).context("Parse input failed")?;
    fs::create_dir_all(&args.output).await?;

    println!(
        "Processing {} accessions via direct download to {}...",
        accessions.len(),
        args.output.display()
    );

    let results: Vec<ProcessResult> = stream::iter(accessions)
        .map(|acc| {
            let client = client.clone();
            let output = args.output.clone();
            let concurrency = effective.concurrency;
            async move { download_accession(client, acc, output, concurrency).await }
        })
        .buffer_unordered(effective.concurrency)
        .collect()
        .await;

    write_reports(&effective.report_csv, &effective.report_json, &results)?;

    let ok = results.iter().filter(|r| r.status == "Success").count();
    println!("Summary: {} Success, {} Failed/Partial.", ok, results.len() - ok);
    Ok(())
}

async fn download_accession(
    client: Arc<OrthancClient>,
    acc: String,
    output_root: PathBuf,
    instance_concurrency: usize,
) -> ProcessResult {
    let mut res = ProcessResult {
        accession: acc.clone(),
        timestamp: chrono::Utc::now(),
        ..Default::default()
    };

    let study_ids = match client.find_study_ids_by_accession(&acc).await {
        Ok(ids) if !ids.is_empty() => ids,
        Ok(_) => {
            res.reason.push("Study not found".into());
            res.status = "Failed".into();
            return res;
        }
        Err(e) => {
            res.reason.push(format!("Study lookup failed: {}", e));
            res.status = "Failed".into();
            return res;
        }
    };

    let mut any_success = false;
    for study_id in study_ids {
        let study_meta = match client.get_study_meta(&study_id).await {
            Ok(m) => m,
            Err(e) => {
                res.reason.push(format!("Study meta failed: {}", e));
                continue;
            }
        };
        let study_uid = study_meta.study_uid.unwrap_or_else(|| study_id.clone());
        let series_ids = match client.list_series_ids(&study_id).await {
            Ok(list) => list,
            Err(e) => {
                res.reason.push(format!("Series list failed: {}", e));
                continue;
            }
        };

        for series_id in series_ids {
            let series_meta = match client.get_series_meta(&series_id).await {
                Ok(m) => m,
                Err(e) => {
                    res.reason.push(format!("Series meta failed: {}", e));
                    continue;
                }
            };

            let series_uid = series_meta.series_uid.unwrap_or_else(|| series_id.clone());
            let series_desc = series_meta
                .description
                .clone()
                .unwrap_or_else(|| series_uid.clone());
            let instances = series_meta.instances;
            if instances.is_empty() {
                continue;
            }

            let dest_dir = output_root.join(&study_uid).join(&series_uid);
            if let Err(e) = fs::create_dir_all(&dest_dir).await {
                res.reason
                    .push(format!("Create dir failed {}: {}", dest_dir.display(), e));
                res.failed_series.push(series_desc.clone());
                continue;
            }

            let results = stream::iter(instances.into_iter().map(|inst_id| {
                let client = client.clone();
                let dir = dest_dir.clone();
                async move {
                    let dest_path = dir.join(format!("{}.dcm", inst_id));
                    if dest_path.exists() {
                        return Ok(());
                    }
                    let data = client.download_instance_file(&inst_id).await?;
                    fs::write(dest_path, data).await.map_err(anyhow::Error::from)
                }
            }))
            .buffer_unordered(instance_concurrency)
            .collect::<Vec<Result<()>>>()
            .await;

            let failures = results.iter().filter(|r| r.is_err()).count();
            if failures == 0 {
                res.matched_series.push(series_desc.clone());
                res.downloaded_series.push(series_desc.clone());
                any_success = true;
            } else if failures < results.len() {
                res.matched_series.push(series_desc.clone());
                res.downloaded_series.push(series_desc.clone());
                res.reason.push(format!(
                    "{} failed out of {} instances for {}",
                    failures,
                    results.len(),
                    series_desc
                ));
                any_success = true;
            } else {
                res.failed_series.push(series_desc.clone());
                res.reason
                    .push(format!("All instances failed for {}", series_desc));
            }
        }
    }

    res.status = summarize_status(&res.downloaded_series, &res.reason);
    if !any_success && res.status == "Success" {
        res.status = "Failed".into();
    }
    res
}
