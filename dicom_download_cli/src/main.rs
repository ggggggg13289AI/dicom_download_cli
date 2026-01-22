//! CLI wrapper around Orthanc that downloads DICOM series referenced by accession numbers.
//!
//! It batches accessions from CSV/JSON, consults Orthanc and an optional analysis service,
//! and writes success/failure reports in CSV/JSON formats.
mod checker;
mod client;
mod config;
mod converter;
mod processor;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use futures::stream::{self, StreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;

use crate::client::{
    parse_dicom_study_info, DicomStudyInfo, DownloadPlan, OrthancClient, SeriesDownloadPlan,
};
use crate::config::{
    load_runtime_config, sanitize_optional_string, AnalysisConfig, ConversionConfig,
    EffectiveConfig, PerInstanceConfig, RuntimeConfigFile, DEFAULT_CONFIG_PATH,
};
use crate::converter::{check_dcm2niix_available, convert_series_to_nifti, delete_dicom_files};
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
    /// Check and fix DICOM file structure issues (DWI b-value, ADC duplicates)
    Check(CheckArgs),
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

    /// Directory to write downloaded files (will contain dicom/ and niix/ subdirectories).
    #[arg(long, value_name = "DIR")]
    output: PathBuf,

    /// Enable dcm2niix conversion to NIfTI format after download.
    #[arg(long)]
    convert: bool,

    /// Retry count per instance (default: 3)
    #[arg(long, default_value = "3")]
    retry_count: usize,

    /// Timeout per instance in seconds (default: 60)
    #[arg(long, default_value = "60")]
    timeout: u64,
}

#[derive(Args, Clone)]
struct CheckArgs {
    /// Root directory containing downloaded DICOM files.
    /// Expected structure: input/dicom/PatientID_StudyDate_Modality_Accession/SeriesFolder/
    #[arg(short, long, value_name = "DIR")]
    input: PathBuf,

    /// Dry-run mode: show what would be done without making changes.
    #[arg(long)]
    dry_run: bool,

    /// Output report path (CSV format).
    #[arg(long)]
    report_csv: Option<PathBuf>,

    /// Output report path (JSON format).
    #[arg(long)]
    report_json: Option<PathBuf>,
}

/// Entrypoint that wires CLI args, runtime config, Orthanc client, and processor workers.
///
/// It loads overrides, creates the HTTP client, parses accessions, runs bounded async workers,
/// waits for them, then writes CSV/JSON reports and prints a summary.
#[tokio::main]
async fn main() -> Result<()> {
    let args = Cli::parse();
    let cfg_path = args
        .config
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));

    match args.command {
        Commands::Remote(cmd) => run_remote(cmd, &cfg_path).await,
        Commands::Download(cmd) => run_download(cmd, &cfg_path).await,
        Commands::Check(cmd) => run_check(cmd).await,
    }
}

/// Merge CLI overrides with a parsed runtime config, falling back to crate defaults.
///
/// CLI flags take precedence, followed by the runtime file, and finally `EffectiveConfig::defaults()`.
fn merge_config(cli: &SharedArgs, file: Option<RuntimeConfigFile>) -> EffectiveConfig {
    let mut cfg = EffectiveConfig::defaults();
    let f = file.unwrap_or_default();

    cfg.url = cli.url.clone().or(f.url).unwrap_or(cfg.url);
    cfg.analyze_url = cli
        .analyze_url
        .clone()
        .or(f.analyze_url)
        .unwrap_or(cfg.analyze_url);
    cfg.modality = cli.modality.clone().or(f.modality).unwrap_or(cfg.modality);
    cfg.target = cli.target.clone().or(f.target).unwrap_or(cfg.target);
    cfg.concurrency = cli.concurrency.or(f.concurrency).unwrap_or(cfg.concurrency);
    cfg.report_csv = cli
        .report_csv
        .clone()
        .or(f.report_csv)
        .unwrap_or(cfg.report_csv);
    cfg.report_json = cli
        .report_json
        .clone()
        .or(f.report_json)
        .unwrap_or(cfg.report_json);
    cfg.username =
        sanitize_optional_string(cli.username.clone()).or(sanitize_optional_string(f.username));
    cfg.password =
        sanitize_optional_string(cli.password.clone()).or(sanitize_optional_string(f.password));

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

    println!(
        "Processing {} accessions via remote C-MOVE...",
        accessions.len()
    );

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
    println!(
        "Summary: {} Success, {} Failed/Partial.",
        ok,
        results.len() - ok
    );

    Ok(())
}

async fn run_check(args: CheckArgs) -> Result<()> {
    use crate::checker::{run_check, write_csv_report, write_json_report};

    println!("DICOM Structure Checker");
    println!("=======================");
    println!("Input directory: {}", args.input.display());
    println!("Mode: {}", if args.dry_run { "DRY-RUN (no changes will be made)" } else { "EXECUTE" });
    println!();

    // Run the check
    let report = run_check(&args.input, args.dry_run).await?;

    // Print summary
    println!("\n========== Summary ==========");
    println!("Total studies scanned: {}", report.summary.total_studies);
    println!("Series with issues: {}", report.summary.total_series_checked);
    println!("Files checked: {}", report.summary.total_files_checked);
    println!("DWI fixes (moves): {}", report.summary.dwi_fixes);
    println!("ADC duplicates removed: {}", report.summary.adc_duplicates_removed);
    println!("Total moves: {}", report.summary.total_moves);
    println!("Total deletes: {}", report.summary.total_deletes);

    if args.dry_run {
        println!("\n[DRY-RUN] No changes were made. Run without --dry-run to apply fixes.");
    }

    // Write reports if requested
    if let Some(csv_path) = &args.report_csv {
        write_csv_report(&report, csv_path)?;
    }
    if let Some(json_path) = &args.report_json {
        write_json_report(&report, json_path)?;
    }

    Ok(())
}

async fn run_download(args: DownloadArgs, cfg_path: &PathBuf) -> Result<()> {
    let runtime_file = load_runtime_config(Some(cfg_path))?;
    let effective = merge_config(&args.shared, runtime_file.clone());

    // Get conversion config from runtime file or use defaults
    let conversion_config = runtime_file
        .as_ref()
        .and_then(|f| f.conversion.clone())
        .unwrap_or_default();

    // Determine if conversion is enabled (CLI flag takes precedence)
    let convert_enabled = args.convert || conversion_config.is_enabled();

    // Check dcm2niix availability if conversion is enabled
    if convert_enabled {
        let dcm2niix_path = conversion_config.get_dcm2niix_path();
        if !check_dcm2niix_available(dcm2niix_path) {
            eprintln!(
                "Warning: dcm2niix not found at '{}'. Conversion will be skipped.",
                dcm2niix_path
            );
        }
    }

    let client = Arc::new(OrthancClient::new(
        &effective.url,
        &effective.analyze_url,
        &effective.target,
        effective.username.clone(),
        effective.password.clone(),
    )?);

    let accessions = config::parse_input_file(&args.shared.input).context("Parse input failed")?;

    // Create subdirectory structure: output/dicom/ and output/niix/
    let dicom_root = args.output.join("dicom");
    let niix_root = args.output.join("niix");
    fs::create_dir_all(&dicom_root).await?;
    if convert_enabled {
        fs::create_dir_all(&niix_root).await?;
    }

    // let analyze_enabled =
    //     args.shared.analyze_url.is_some() || effective.analyze_url != config::DEFAULT_ANALYZE_URL;

    let analyze_enabled = args.shared.analyze_url.is_some()
        || runtime_file
            .as_ref()
            .and_then(|f| f.analyze_url.as_ref())
            .is_some();
    println!(
        "Processing {} accessions via direct download to {}...",
        accessions.len(),
        args.output.display()
    );
    println!("  DICOM output: {}", dicom_root.display());
    if convert_enabled {
        println!("  NIfTI output: {}", niix_root.display());
    }
    println!(
        "Analyze API: {}",
        if analyze_enabled {
            "enabled"
        } else {
            "disabled (using SeriesDescription)"
        }
    );
    println!(
        "dcm2niix conversion: {}",
        if convert_enabled {
            "enabled"
        } else {
            "disabled"
        }
    );

    let retry_config = RetryConfig {
        max_retries: args.retry_count,
        timeout: Duration::from_secs(args.timeout),
    };

    let conversion_config = Arc::new(conversion_config);

    // Get per-instance config from runtime file or use defaults
    let per_instance_config = runtime_file
        .as_ref()
        .and_then(|f| f.per_instance.clone())
        .unwrap_or_default();
    let per_instance_config = Arc::new(per_instance_config);

    if per_instance_config.is_enabled() {
        println!(
            "Per-instance analysis: enabled (triggers: {:?})",
            per_instance_config.get_trigger_prefixes()
        );
    }

    // 循序處理每個 accession（一個一個 study 下載）
    // Series/Instance 層級使用併發
    let mut results: Vec<ProcessResult> = Vec::with_capacity(accessions.len());
    for acc in accessions {
        let result = download_accession_v2(
            client.clone(),
            acc,
            dicom_root.clone(),
            niix_root.clone(),
            effective.concurrency,
            analyze_enabled,
            convert_enabled,
            conversion_config.clone(),
            per_instance_config.clone(),
            retry_config.clone(),
        )
        .await;
        results.push(result);
    }

    write_reports(&effective.report_csv, &effective.report_json, &results)?;

    let ok = results.iter().filter(|r| r.status == "Success").count();
    let converted = results
        .iter()
        .map(|r| r.converted_series.len())
        .sum::<usize>();
    let conversion_failed = results
        .iter()
        .map(|r| r.conversion_failed.len())
        .sum::<usize>();

    println!(
        "\nSummary: {} Success, {} Failed/Partial.",
        ok,
        results.len() - ok
    );
    if convert_enabled {
        println!(
            "Conversion: {} series converted, {} failed.",
            converted, conversion_failed
        );
    }
    Ok(())
}

// ============================================================================
// 新版下載邏輯（對齊 Python download_dicom_async.py）
// ============================================================================

/// 重試設定
#[derive(Clone)]
struct RetryConfig {
    max_retries: usize,
    timeout: Duration,
}

/// 下載結果狀態
#[derive(Clone, Debug)]
enum DownloadResult {
    Completed,
    Skipped,
    Failed(String),
}

/// 無效路徑字元集合（與 Python 對齊）
const INVALID_PATH_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Windows 保留檔名（不區分大小寫）
const WINDOWS_RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 檢查是否為 Windows 保留檔名
fn is_windows_reserved_name(name: &str) -> bool {
    let upper = name.to_uppercase();
    WINDOWS_RESERVED_NAMES.contains(&upper.as_str())
}

/// 清理路徑片段，移除無效字元並處理 Windows 保留檔名
fn sanitize_segment(text: &str) -> String {
    let cleaned: String = text
        .trim()
        .chars()
        .map(|c| {
            if INVALID_PATH_CHARS.contains(&c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else if is_windows_reserved_name(&cleaned) {
        // 為 Windows 保留名稱加上底線前綴
        format!("_{}", cleaned)
    } else {
        cleaned
    }
}

/// 產生安全的 DICOM 檔名（處理 Windows 保留名稱）
fn safe_dicom_filename(instance_id: &str) -> String {
    let base_name = sanitize_segment(instance_id);
    format!("{}.dcm", base_name)
}

/// 產生 study 資料夾名稱（與 Python 對齊）
fn generate_study_folder_name(info: &DicomStudyInfo) -> String {
    format!(
        "{}_{}_{}_{}",
        sanitize_segment(&info.patient_id),
        sanitize_segment(&info.study_date),
        sanitize_segment(&info.modality),
        sanitize_segment(&info.accession_number)
    )
}

/// 產生 series 資料夾名稱（Linus Good Taste: 統一處理，消除 DWI 特殊情況）
fn generate_series_folder_name(
    series_type: &str,
    series_number: Option<&str>,
    type_counts: &HashMap<String, usize>,
) -> String {
    let count = *type_counts.get(series_type).unwrap_or(&1);

    // 統一模式：只要同類型有多個，就加編號
    if count > 1 {
        let num = series_number
            .and_then(|n| n.parse::<u32>().ok())
            .map(|n| format!("{:03}", n))
            .unwrap_or_else(|| "000".to_string());
        format!("{}_{}", series_type, num)
    } else {
        series_type.to_string()
    }
}

/// 建立下載計畫（與 Python build_download_plan 對齊）
/// 支援 per-instance 分析模式：當第一個 instance 的 series_type 匹配 trigger_prefixes 時，
/// 對所有 instances 進行個別分析並分組到不同資料夾。
async fn build_download_plan(
    client: Arc<OrthancClient>,
    accession: &str,
    analyze_enabled: bool,
    per_instance_config: &PerInstanceConfig,
) -> Result<Vec<DownloadPlan>> {
    let mut plans = Vec::new();

    let study_ids = client.find_study_ids_by_accession(accession).await?;
    if study_ids.is_empty() {
        return Ok(plans);
    }

    for study_id in study_ids {
        let series_ids = match client.list_series_ids(&study_id).await {
            Ok(ids) => ids,
            Err(_) => continue,
        };

        let mut series_info: Vec<(String, String, Option<String>, Vec<String>)> = Vec::new();
        let mut study_folder_name: Option<String> = None;

        for series_id in &series_ids {
            let meta = match client.get_series_meta(series_id).await {
                Ok(m) => m,
                Err(_) => continue,
            };

            if meta.instances.is_empty() {
                continue;
            }

            // 取第一個 instance 的 DICOM bytes
            let first_instance = &meta.instances[0];
            let dicom_data = match client.download_instance_file(first_instance).await {
                Ok(d) => d,
                Err(e) => {
                    eprintln!(
                        "Warning: Failed to download first instance {} for series {}: {}",
                        first_instance, series_id, e
                    );
                    continue;
                }
            };

            // 解析 DICOM 標籤取得 study folder 名稱（只需做一次）
            if study_folder_name.is_none() {
                if let Ok(info) = parse_dicom_study_info(&dicom_data) {
                    study_folder_name = Some(generate_study_folder_name(&info));
                }
            }

            // 決定 series_type（支援 per-instance 模式）
            let first_series_type = if analyze_enabled {
                // 呼叫 Analyze API 分析第一個 instance
                match client.analyze_dicom_data(dicom_data).await {
                    Ok(Some(t)) if t.to_lowercase() != "unknown" => t,
                    _ => meta
                        .description
                        .clone()
                        .unwrap_or_else(|| "Unknown".to_string()),
                }
            } else {
                meta.description
                    .clone()
                    .unwrap_or_else(|| "Unknown".to_string())
            };

            // 檢查是否需要 per-instance 分析
            if analyze_enabled && per_instance_config.should_analyze(&first_series_type) {
                // Per-instance 模式：分析每個 instance 並按 type 分組
                let analyze_concurrency = per_instance_config.get_analyze_concurrency();

                // 並發分析所有 instances
                let instance_types: Vec<(String, String)> = stream::iter(meta.instances.iter().cloned())
                    .map(|inst_id| {
                        let client = client.clone();
                        async move {
                            let inst_type = match client.download_instance_file(&inst_id).await {
                                Ok(data) => match client.analyze_dicom_data(data).await {
                                    Ok(Some(t)) if t.to_lowercase() != "unknown" => t,
                                    _ => "Unknown".to_string(),
                                },
                                Err(_) => "Unknown".to_string(),
                            };
                            (inst_id, inst_type)
                        }
                    })
                    .buffer_unordered(analyze_concurrency)
                    .collect()
                    .await;

                // 按 series_type 分組 instances
                let mut grouped: HashMap<String, Vec<String>> = HashMap::new();
                for (inst_id, inst_type) in instance_types {
                    grouped.entry(inst_type).or_default().push(inst_id);
                }

                // 為每個分組創建 series_info 條目
                for (group_type, instances) in grouped {
                    series_info.push((
                        series_id.clone(),
                        group_type,
                        meta.series_number.clone(),
                        instances,
                    ));
                }
            } else {
                // 標準模式：所有 instances 使用相同 series_type
                series_info.push((
                    series_id.clone(),
                    first_series_type,
                    meta.series_number.clone(),
                    meta.instances.clone(),
                ));
            }
        }

        // 計算每個 series_type 的出現次數
        let mut type_counts: HashMap<String, usize> = HashMap::new();
        for (_, series_type, _, _) in &series_info {
            *type_counts.entry(series_type.clone()).or_insert(0) += 1;
        }

        // 產生 SeriesDownloadPlan
        let series_plans: Vec<SeriesDownloadPlan> = series_info
            .into_iter()
            .map(|(_, series_type, series_number, instances)| {
                let series_folder = generate_series_folder_name(
                    &series_type,
                    series_number.as_deref(),
                    &type_counts,
                );
                SeriesDownloadPlan {
                    series_folder,
                    instances,
                }
            })
            .collect();

        plans.push(DownloadPlan {
            study_folder: study_folder_name.unwrap_or_else(|| format!("{}_unknown", accession)),
            series: series_plans,
        });
    }

    Ok(plans)
}

/// 帶重試的下載函數
async fn download_with_retry(
    client: &OrthancClient,
    instance_id: &str,
    dest_path: &Path,
    config: &RetryConfig,
) -> DownloadResult {
    // 處理 max_retries = 0 的邊界情況
    if config.max_retries == 0 {
        return DownloadResult::Failed("No retries configured".to_string());
    }

    for attempt in 0..config.max_retries {
        match tokio::time::timeout(config.timeout, client.download_instance_file(instance_id)).await
        {
            Ok(Ok(data)) => {
                // 使用 create_new(true) 原子寫入，避免 TOCTOU 競態條件
                match OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(dest_path)
                    .await
                {
                    Ok(mut file) => {
                        if let Err(e) = file.write_all(&data).await {
                            if attempt < config.max_retries - 1 {
                                tokio::time::sleep(Duration::from_secs((attempt + 1) as u64)).await;
                                continue;
                            }
                            return DownloadResult::Failed(format!("Write failed: {}", e));
                        }
                        return DownloadResult::Completed;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        // 檔案已存在，跳過
                        return DownloadResult::Skipped;
                    }
                    Err(e) => {
                        if attempt < config.max_retries - 1 {
                            tokio::time::sleep(Duration::from_secs((attempt + 1) as u64)).await;
                            continue;
                        }
                        return DownloadResult::Failed(format!("File create failed: {}", e));
                    }
                }
            }
            Ok(Err(e)) => {
                if attempt < config.max_retries - 1 {
                    tokio::time::sleep(Duration::from_secs((attempt + 1) as u64)).await;
                    continue;
                }
                return DownloadResult::Failed(format!("Download failed: {}", e));
            }
            Err(_) => {
                // Timeout
                if attempt < config.max_retries - 1 {
                    tokio::time::sleep(Duration::from_secs(((attempt + 1) * 2) as u64)).await;
                    continue;
                }
                return DownloadResult::Failed("Timeout".to_string());
            }
        }
    }
    // 當 max_retries > 0 時，迴圈內所有分支都會 return，不會到達這裡
    unreachable!("download_with_retry loop should always return within the loop")
}

/// 進度追蹤器（使用 indicatif）
struct DownloadProgressTracker {
    completed: AtomicUsize,
    failed: AtomicUsize,
    skipped: AtomicUsize,
    start_time: Instant,
    pb: ProgressBar,
}

impl DownloadProgressTracker {
    fn new(total: usize, mp: &MultiProgress, series_name: &str) -> Self {
        let pb = mp.add(ProgressBar::new(total as u64));
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}")
                .unwrap()
                .progress_chars("=>-"),
        );
        pb.set_message(series_name.to_string());

        Self {
            completed: AtomicUsize::new(0),
            failed: AtomicUsize::new(0),
            skipped: AtomicUsize::new(0),
            start_time: Instant::now(),
            pb,
        }
    }

    fn update(&self, result: &DownloadResult) {
        match result {
            DownloadResult::Completed => {
                self.completed.fetch_add(1, Ordering::Relaxed);
            }
            DownloadResult::Failed(err) => {
                eprintln!("Download failed: {}", err);
                self.failed.fetch_add(1, Ordering::Relaxed);
            }
            DownloadResult::Skipped => {
                self.skipped.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.pb.inc(1);
    }

    fn finish(&self) {
        let completed = self.completed.load(Ordering::Relaxed);
        let failed = self.failed.load(Ordering::Relaxed);
        let skipped = self.skipped.load(Ordering::Relaxed);
        let elapsed = self.start_time.elapsed().as_secs_f64();

        self.pb.finish_with_message(format!(
            "Done: {} ok, {} skip, {} fail ({:.1}s)",
            completed, skipped, failed, elapsed
        ));
    }
}

/// 新版下載函數（對齊 Python download_dicom_async.py）
async fn download_accession_v2(
    client: Arc<OrthancClient>,
    acc: String,
    dicom_root: PathBuf,
    niix_root: PathBuf,
    instance_concurrency: usize,
    analyze_enabled: bool,
    convert_enabled: bool,
    conversion_config: Arc<ConversionConfig>,
    per_instance_config: Arc<PerInstanceConfig>,
    retry_config: RetryConfig,
) -> ProcessResult {
    let mut res = ProcessResult {
        accession: acc.clone(),
        timestamp: chrono::Utc::now(),
        ..Default::default()
    };

    // 建立下載計畫
    let plans = match build_download_plan(client.clone(), &acc, analyze_enabled, &per_instance_config).await {
        Ok(p) if !p.is_empty() => p,
        Ok(_) => {
            res.reason.push("No studies found".into());
            res.status = "Failed".into();
            return res;
        }
        Err(e) => {
            res.reason.push(format!("Build plan failed: {}", e));
            res.status = "Failed".into();
            return res;
        }
    };

    let mp = MultiProgress::new();
    let mut any_success = false;

    // Check dcm2niix availability once
    let dcm2niix_available = if convert_enabled {
        check_dcm2niix_available(conversion_config.get_dcm2niix_path())
    } else {
        false
    };

    for plan in plans {
        let dicom_study_dir = dicom_root.join(&plan.study_folder);
        let niix_study_dir = niix_root.join(&plan.study_folder);

        for series_plan in &plan.series {
            let series_dir = dicom_study_dir.join(&series_plan.series_folder);
            if let Err(e) = fs::create_dir_all(&series_dir).await {
                res.reason
                    .push(format!("Create dir failed {}: {}", series_dir.display(), e));
                res.failed_series.push(series_plan.series_folder.clone());
                continue;
            }

            let tracker = Arc::new(DownloadProgressTracker::new(
                series_plan.instances.len(),
                &mp,
                &series_plan.series_folder,
            ));

            let results: Vec<DownloadResult> = stream::iter(series_plan.instances.iter().cloned())
                .map(|inst_id| {
                    let client = client.clone();
                    let dir = series_dir.clone();
                    let cfg = retry_config.clone();
                    let tracker = tracker.clone();
                    async move {
                        let dest_path = dir.join(safe_dicom_filename(&inst_id));
                        let result = download_with_retry(&client, &inst_id, &dest_path, &cfg).await;
                        tracker.update(&result);
                        result
                    }
                })
                .buffer_unordered(instance_concurrency)
                .collect()
                .await;

            tracker.finish();

            let failures = results
                .iter()
                .filter(|r| matches!(r, DownloadResult::Failed(_)))
                .count();

            let series_download_success = if failures == 0 {
                res.matched_series.push(series_plan.series_folder.clone());
                res.downloaded_series
                    .push(series_plan.series_folder.clone());
                any_success = true;
                true
            } else if failures < results.len() {
                res.matched_series.push(series_plan.series_folder.clone());
                res.downloaded_series
                    .push(series_plan.series_folder.clone());
                res.reason.push(format!(
                    "{} failed out of {} instances for {}",
                    failures,
                    results.len(),
                    series_plan.series_folder
                ));
                any_success = true;
                true
            } else {
                res.failed_series.push(series_plan.series_folder.clone());
                res.reason.push(format!(
                    "All instances failed for {}",
                    series_plan.series_folder
                ));
                false
            };

            // Perform conversion if enabled and download succeeded
            if convert_enabled && dcm2niix_available && series_download_success {
                let conv_result = convert_series_to_nifti(
                    &series_dir,
                    &niix_study_dir,
                    &series_plan.series_folder,
                    conversion_config.get_dcm2niix_path(),
                    &conversion_config.get_dcm2niix_args(),
                )
                .await;

                match conv_result {
                    Ok(result) if result.success => {
                        res.converted_series.push(series_plan.series_folder.clone());
                        // Optionally delete DICOM files after successful conversion
                        if conversion_config.should_delete_dicom() {
                            if let Err(e) = delete_dicom_files(&series_dir).await {
                                res.reason.push(format!(
                                    "Failed to delete DICOM files for {}: {}",
                                    series_plan.series_folder, e
                                ));
                            }
                        }
                    }
                    Ok(result) => {
                        // Conversion ran but produced no NIfTI files (e.g., SR DICOM)
                        res.conversion_failed
                            .push(series_plan.series_folder.clone());
                        if let Some(err) = result.error {
                            res.reason.push(format!(
                                "Conversion produced no output for {}: {}",
                                series_plan.series_folder, err
                            ));
                        }
                    }
                    Err(e) => {
                        res.conversion_failed
                            .push(series_plan.series_folder.clone());
                        res.reason.push(format!(
                            "Conversion failed for {}: {}",
                            series_plan.series_folder, e
                        ));
                    }
                }
            }
        }
    }

    res.status = summarize_status(&res.downloaded_series, &res.reason);
    if !any_success && res.status == "Success" {
        res.status = "Failed".into();
    }
    res
}
