use crate::client::OrthancClient;
use crate::config::{should_download, AnalysisConfig};
use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use colored::*;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use serde::Serialize;
use serde_json::json;
use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Serialize, Default)]
pub struct ProcessResult {
    pub accession: String,
    pub status: String,
    pub reason: Vec<String>,
    pub downloaded_series: Vec<String>,
    pub matched_series: Vec<String>,
    pub failed_series: Vec<String>,
    pub timestamp: DateTime<Utc>,
}

pub async fn process_single_accession(
    client: Arc<OrthancClient>,
    acc: String,
    modality: String,
    mp: Arc<MultiProgress>,
    config: Arc<AnalysisConfig>,
) -> ProcessResult {
    let pb = setup_progress_bar(&mp, &acc);
    let mut res = ProcessResult {
        accession: acc.clone(),
        timestamp: Utc::now(),
        ..Default::default()
    };

    let study_uid = match client.find_study_by_accession(&acc, &modality).await {
        Ok(uid) => uid,
        Err(e) => return finish_with_error(pb, &mut res, format!("Study query failed: {}", e)),
    };

    let remote_series = match client.get_remote_series(&modality, &study_uid).await {
        Ok(s) => s,
        Err(e) => return finish_with_error(pb, &mut res, format!("Series query failed: {}", e)),
    };

    let local_uids = client.get_local_series(&study_uid).await.unwrap_or_default();
    
    for (idx, series_json) in remote_series.into_iter().enumerate() {
        let (uid, desc) = client.extract_series_info(&series_json);
        if local_uids.contains(&uid) {
            continue;
        }

        pb.set_message(format!(" [{}/{}] {}", idx + 1, res.matched_series.len() + 1, desc));
        
        if let Err(e) = process_series(&client, &modality, &study_uid, &uid, &desc, &config, &pb, &mut res).await {
            res.reason.push(e.to_string());
        }
    }

    pb.finish_with_message(format!("{} Done", "✓".green()));
    res.status = summarize_status(&res.downloaded_series, &res.reason);
    res
}

async fn process_series(
    client: &OrthancClient,
    modality: &str,
    study_uid: &str,
    series_uid: &str,
    desc: &str,
    config: &AnalysisConfig,
    pb: &ProgressBar,
    res: &mut ProcessResult,
) -> Result<()> {
    let should_dl = if config.download_all || should_download(desc, None, config) {
        true
    } else {
        match client.sample_series_type(modality, study_uid, series_uid).await? {
            Some(t) => should_download(desc, Some(&t), config),
            None => false,
        }
    };

    if !should_dl {
        return Ok(());
    }

    res.matched_series.push(desc.to_string());
    pb.set_message(format!("Downloading {}...", desc));

    let move_payload = json!({ "SeriesInstanceUID": series_uid, "StudyInstanceUID": study_uid });
    match client.c_move(modality, "Series", move_payload, true).await? {
        Some(job_id) => {
            client.wait_for_job(&job_id, pb).await?;
            res.downloaded_series.push(desc.to_string());
        }
        None => {
            res.failed_series.push(desc.to_string());
            return Err(anyhow!("Sync move not supported for {}", desc));
        }
    }
    Ok(())
}

fn setup_progress_bar(mp: &MultiProgress, prefix: &str) -> ProgressBar {
    let pb = mp.add(ProgressBar::new_spinner());
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.green} [{prefix}] {msg}")
            .unwrap(),
    );
    pb.set_prefix(prefix.to_string());
    pb.enable_steady_tick(Duration::from_millis(100));
    pb
}

fn finish_with_error(pb: ProgressBar, res: &mut ProcessResult, err: String) -> ProcessResult {
    pb.finish_with_message(format!("{} {}", "✗".red(), err));
    res.status = "Failed".into();
    res.reason.push(err);
    std::mem::take(res)
}

pub fn summarize_status(downloaded: &[String], reasons: &[String]) -> String {
    if reasons.is_empty() { "Success".into() }
    else if !downloaded.is_empty() { "Partial".into() }
    else { "Failed".into() }
}

pub fn write_reports(csv_path: &PathBuf, json_path: &PathBuf, results: &[ProcessResult]) -> Result<()> {
    write_csv_report(csv_path, results)?;
    write_json_report(json_path, results)?;
    Ok(())
}

fn write_json_report(path: &PathBuf, results: &[ProcessResult]) -> Result<()> {
    let file = File::create(path)?;
    serde_json::to_writer_pretty(file, results)?;
    Ok(())
}

fn write_csv_report(path: &PathBuf, results: &[ProcessResult]) -> Result<()> {
    let mut wtr = csv::Writer::from_path(path)?;
    wtr.write_record(&["AccessionNumber", "Status", "Reason", "DownloadedCount", "MatchedCount", "FailedCount", "Timestamp"])?;
    for r in results {
        wtr.write_record(&[
            &r.accession,
            &r.status,
            &r.reason.join("; "),
            &r.downloaded_series.len().to_string(),
            &r.matched_series.len().to_string(),
            &r.failed_series.len().to_string(),
            &r.timestamp.to_rfc3339(),
        ])?;
    }
    wtr.flush()?;
    Ok(())
}
