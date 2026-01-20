use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose, Engine as _};
use chrono::{DateTime, Utc};
use clap::Parser;
use colored::*;
use futures::stream::{self, StreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs::{self, File};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

// =================================================================================
// 1. Pure Domain Logic & Configuration (Functional Core)
// =================================================================================

const DEFAULT_CONFIG_PATH: &str = "config/dicom_download_cli.toml";
const DEFAULT_MODALITY: &str = "INFINTT-SERVER";
const DEFAULT_TARGET: &str = "RADAX";
const DEFAULT_URL: &str = "http://10.103.51.1:8042/";
const DEFAULT_ANALYZE_URL: &str =
    "http://10.103.51.1:8000/api/v1/series/dicom/analyze/by-upload";
const DEFAULT_REPORT_CSV: &str = "report.csv";
const DEFAULT_REPORT_JSON: &str = "report.json";
const DEFAULT_CONCURRENCY: usize = 5;

/// 控制白名單與直接下載關鍵字
struct AnalysisConfig {
    series_whitelist: HashSet<String>,
    direct_download_keywords: HashSet<String>,
    enable_whitelist: bool,
    enable_direct_keywords: bool,
    download_all: bool,
}

impl AnalysisConfig {
    fn default() -> Self {
        Self {
            series_whitelist: HashSet::from([
                "ADC".into(),
                "DWI".into(),
                "DWI0".into(),
                "DWI1000".into(),
                "SWAN".into(),
                "MRA_BRAIN".into(),
                "T1FLAIR_AXI".into(),
                "T1BRAVO_AXI".into(),
                "T2FLAIR_AXI".into(),
                "ASLSEQ".into(),
                "ASLSEQATT".into(),
                "ASLSEQATT_COLOR".into(),
                "ASLSEQCBF".into(),
                "ASLSEQCBF_COLOR".into(),
                "ASLSEQPW".into(),
                "ASLPROD".into(),
                "ASLPRODCBF".into(),
                "ASLPRODCBF_COLOR".into(),
                "DSC".into(),
                "DSCCBF_COLOR".into(),
                "DSCCBV_COLOR".into(),
                "DSCMTT_COLOR".into(),
            ]),
            direct_download_keywords: HashSet::from(["MRA_BRAIN".into()]),
            enable_whitelist: true,
            enable_direct_keywords: true,
            download_all: false,
        }
    }

    fn load(path: Option<&PathBuf>) -> Result<Self> {
        if let Some(path) = path {
            if path.exists() {
                Self::from_file(path)
            } else {
                Ok(Self::default())
            }
        } else {
            Ok(Self::default())
        }
    }

    fn from_file(path: &PathBuf) -> Result<Self> {
        let content = fs::read_to_string(path).context("Failed to read analysis config")?;
        let parsed: AnalysisConfigFile =
            toml::from_str(&content).context("Failed to parse analysis config")?;
        let mut config = Self::default();

        if let Some(enable) = parsed.enable_whitelist {
            config.enable_whitelist = enable;
        }
        if let Some(enable) = parsed.enable_direct_keywords {
            config.enable_direct_keywords = enable;
        }
        if let Some(enable) = parsed.download_all {
            config.download_all = enable;
        }
        if let Some(series) = parsed.series_whitelist {
            config.series_whitelist = series
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
        if let Some(keywords) = parsed.direct_download_keywords {
            config.direct_download_keywords = keywords
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }

        Ok(config)
    }
}

#[derive(Deserialize)]
struct AnalysisConfigFile {
    enable_whitelist: Option<bool>,
    enable_direct_keywords: Option<bool>,
    download_all: Option<bool>,
    series_whitelist: Option<Vec<String>>,
    direct_download_keywords: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
struct RuntimeConfigFile {
    url: Option<String>,
    analyze_url: Option<String>,
    modality: Option<String>,
    target: Option<String>,
    username: Option<String>,
    password: Option<String>,
    concurrency: Option<usize>,
    report_csv: Option<PathBuf>,
    report_json: Option<PathBuf>,
}

struct EffectiveConfig {
    url: String,
    analyze_url: String,
    modality: String,
    target: String,
    username: Option<String>,
    password: Option<String>,
    concurrency: usize,
    report_csv: PathBuf,
    report_json: PathBuf,
}

impl EffectiveConfig {
    fn defaults() -> Self {
        Self {
            url: DEFAULT_URL.to_string(),
            analyze_url: DEFAULT_ANALYZE_URL.to_string(),
            modality: DEFAULT_MODALITY.to_string(),
            target: DEFAULT_TARGET.to_string(),
            username: None,
            password: None,
            concurrency: DEFAULT_CONCURRENCY,
            report_csv: PathBuf::from(DEFAULT_REPORT_CSV),
            report_json: PathBuf::from(DEFAULT_REPORT_JSON),
        }
    }
}

fn load_runtime_config(path: Option<&PathBuf>) -> Result<Option<RuntimeConfigFile>> {
    let path = match path {
        Some(path) => path.clone(),
        None => PathBuf::from(DEFAULT_CONFIG_PATH),
    };

    if !path.exists() {
        return Ok(None);
    }

    let content = fs::read_to_string(&path).context("Failed to read runtime config")?;
    let parsed: RuntimeConfigFile =
        toml::from_str(&content).context("Failed to parse runtime config")?;
    Ok(Some(parsed))
}

fn sanitize_optional_string(value: Option<String>) -> Option<String> {
    value.and_then(|s| {
        let trimmed = s.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

fn merge_runtime_config(cli: &Cli, toml_config: Option<RuntimeConfigFile>) -> EffectiveConfig {
    let mut config = EffectiveConfig::defaults();

    if let Some(file) = toml_config {
        if let Some(url) = file.url {
            config.url = url;
        }
        if let Some(analyze_url) = file.analyze_url {
            config.analyze_url = analyze_url;
        }
        if let Some(modality) = file.modality {
            config.modality = modality;
        }
        if let Some(target) = file.target {
            config.target = target;
        }
        if let Some(concurrency) = file.concurrency {
            config.concurrency = concurrency;
        }
        if let Some(report_csv) = file.report_csv {
            config.report_csv = report_csv;
        }
        if let Some(report_json) = file.report_json {
            config.report_json = report_json;
        }
        if let Some(username) = sanitize_optional_string(file.username) {
            config.username = Some(username);
        }
        if let Some(password) = sanitize_optional_string(file.password) {
            config.password = Some(password);
        }
    }

    if let Some(url) = cli.url.as_ref() {
        config.url = url.clone();
    }
    if let Some(analyze_url) = cli.analyze_url.as_ref() {
        config.analyze_url = analyze_url.clone();
    }
    if let Some(modality) = cli.modality.as_ref() {
        config.modality = modality.clone();
    }
    if let Some(target) = cli.target.as_ref() {
        config.target = target.clone();
    }
    if let Some(concurrency) = cli.concurrency {
        config.concurrency = concurrency;
    }
    if let Some(report_csv) = cli.report_csv.as_ref() {
        config.report_csv = report_csv.clone();
    }
    if let Some(report_json) = cli.report_json.as_ref() {
        config.report_json = report_json.clone();
    }
    if let Some(username) = sanitize_optional_string(cli.username.clone()) {
        config.username = Some(username);
    }
    if let Some(password) = sanitize_optional_string(cli.password.clone()) {
        config.password = Some(password);
    }

    config
}

fn should_download(
    series_desc: &str,
    analysis_type: Option<&str>,
    config: &AnalysisConfig,
) -> bool {
    if config.download_all {
        return true;
    }

    if config.enable_direct_keywords && config.direct_download_keywords.contains(series_desc) {
        return true;
    }

    if !config.enable_whitelist {
        return false;
    }

    match analysis_type {
        Some(t) => config.series_whitelist.contains(t),
        None => false,
    }
}

fn parse_input_file(path: &PathBuf) -> Result<Vec<String>> {
    let extension = path.extension().and_then(|s| s.to_str()).unwrap_or("");

    match extension.to_lowercase().as_str() {
        "csv" => {
            let file = File::open(path)?;
            let mut rdr = csv::Reader::from_reader(file);
            let mut accessions = Vec::new();
            for result in rdr.records() {
                let record = result?;
                if let Some(acc) = record.get(0) {
                    if !acc.trim().is_empty() {
                        accessions.push(acc.trim().to_string());
                    }
                }
            }
            Ok(accessions)
        }
        "json" => {
            let file = File::open(path)?;
            let json_value: Value = serde_json::from_reader(file)?;
            if let Some(arr) = json_value.as_array() {
                let accessions: Vec<String> = arr
                    .iter()
                    .filter_map(|v| {
                        if let Some(s) = v.as_str() {
                            return Some(s.to_string());
                        }
                        if let Some(obj) = v.as_object() {
                            for key in ["accession", "AccessionNumber", "acc"] {
                                if let Some(val) = obj.get(key).and_then(|v| v.as_str()) {
                                    return Some(val.to_string());
                                }
                            }
                        }
                        None
                    })
                    .collect();
                Ok(accessions)
            } else {
                Err(anyhow!("JSON root must be an array"))
            }
        }
        _ => Err(anyhow!("Unsupported file extension. Use .csv or .json")),
    }
}

// =================================================================================
// 2. API Interaction (Imperative Shell / Side Effects)
// =================================================================================

#[derive(Clone)]
struct OrthancClient {
    client: Client,
    base_url: String,
    analyze_url: String,
    target_aet: String,
}

impl OrthancClient {
    fn new(
        base_url: &str,
        analyze_url: &str,
        target_aet: &str,
        username: Option<String>,
        password: Option<String>,
    ) -> Result<Self> {
        let mut builder = Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(60));

        if let (Some(u), Some(p)) = (username, password) {
            let credentials = format!("{}:{}", u, p);
            let token = general_purpose::STANDARD.encode(credentials);
            let mut headers = HeaderMap::new();
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Basic {}", token))
                    .context("Invalid Authorization header")?,
            );
            builder = builder.default_headers(headers);
        }

        Ok(Self {
            client: builder.build().unwrap(),
            base_url: base_url.trim_end_matches('/').to_string(),
            analyze_url: analyze_url.to_string(),
            target_aet: target_aet.to_string(),
        })
    }

    async fn find_study_by_accession(&self, accession: &str, modality: &str) -> Result<String> {
        let payload = json!({
            "Level": "Study",
            "Query": { "AccessionNumber": accession },
        });

        let resp = self
            .client
            .post(format!("{}/modalities/{}/query", self.base_url, modality))
            .json(&payload)
            .send()
            .await
            .context("Failed to query study by accession")?;

        if !resp.status().is_success() {
            return Err(anyhow!("C-FIND failed: {}", resp.status()));
        }

        let query_resp: Value = resp.json().await?;
        let query_id = query_resp["ID"]
            .as_str()
            .ok_or(anyhow!("No Query ID returned"))?;

        let answers: Vec<String> = self
            .client
            .get(format!("{}/queries/{}/answers", self.base_url, query_id))
            .send()
            .await?
            .json()
            .await?;

        if answers.is_empty() {
            return Err(anyhow!("No study found for Accession: {}", accession));
        }

        let content: Value = self
            .client
            .get(format!(
                "{}/queries/{}/answers/{}/content",
                self.base_url, query_id, answers[0]
            ))
            .send()
            .await?
            .json()
            .await?;

        content
            .get("0020,000d")
            .and_then(|v| v.get("Value").and_then(|s| s.as_str()))
            .map(|s| s.to_string())
            .ok_or(anyhow!("Missing StudyInstanceUID (0020,000d) in response"))
    }

    async fn execute_modality_query(&self, modality: &str, payload: Value) -> Result<Vec<Value>> {
        let resp = self
            .client
            .post(format!("{}/modalities/{}/query", self.base_url, modality))
            .json(&payload)
            .send()
            .await
            .context("Failed to run modality query")?;

        let query_resp: Value = resp.json().await?;
        let query_id = query_resp["ID"]
            .as_str()
            .ok_or(anyhow!("No Query ID returned"))?;

        let answers: Vec<String> = self
            .client
            .get(format!("{}/queries/{}/answers", self.base_url, query_id))
            .send()
            .await?
            .json()
            .await?;

        let mut series_list = Vec::new();
        for ans in answers {
            let content: Value = self
                .client
                .get(format!(
                    "{}/queries/{}/answers/{}/content",
                    self.base_url, query_id, ans
                ))
                .send()
                .await?
                .json()
                .await?;
            series_list.push(content);
        }

        Ok(series_list)
    }

    async fn get_remote_series(&self, modality: &str, study_uid: &str) -> Result<Vec<Value>> {
        let payload = json!({
            "Level": "Series",
            "Query": { "StudyInstanceUID": study_uid },
            "Normalize": true,
        });
        self.execute_modality_query(modality, payload).await
    }

    fn extract_series_info(&self, series_json: &Value) -> (String, String) {
        let uid = series_json
            .get("0020,000e")
            .and_then(|x| x.get("Value"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let desc = series_json
            .get("0008,103e")
            .and_then(|x| x.get("Value"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        (uid, desc)
    }

    async fn get_local_series(&self, study_uid: &str) -> Result<HashSet<String>> {
        let payload = json!({
            "Level": "Study",
            "Query": { "StudyInstanceUID": study_uid },
        });
        let studies: Vec<String> = self
            .client
            .post(format!("{}/tools/find", self.base_url))
            .json(&payload)
            .send()
            .await?
            .json()
            .await?;

        if studies.is_empty() {
            return Ok(HashSet::new());
        }

        let series_arr: Vec<Value> = self
            .client
            .get(format!("{}/studies/{}/series", self.base_url, studies[0]))
            .send()
            .await?
            .json()
            .await?;

        let mut uids = HashSet::new();
        for series in series_arr {
            if let Some(uid) = series
                .get("MainDicomTags")
                .and_then(|t| t.get("SeriesInstanceUID"))
                .and_then(|v| v.as_str())
            {
                uids.insert(uid.to_string());
            }
        }
        Ok(uids)
    }

    async fn c_move(
        &self,
        modality: &str,
        level: &str,
        identifier: Value,
        async_mode: bool,
    ) -> Result<Option<String>> {
        let payload = json!({
            "Level": level,
            "Resources": [identifier],
            "TargetAet": self.target_aet,
            "Synchronous": !async_mode,
        });

        let mut req = self
            .client
            .post(format!("{}/modalities/{}/move", self.base_url, modality))
            .json(&payload);

        if async_mode {
            req = req.header("Asynchronous", "true");
        }

        let resp = req.send().await?;
        if !resp.status().is_success() {
            return Err(anyhow!("C-MOVE failed: {}", resp.status()));
        }

        if async_mode {
            let json_body: Value = resp.json().await?;
            Ok(json_body
                .get("ID")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string()))
        } else {
            Ok(None)
        }
    }

    async fn find_instance_sop(&self, modality: &str, series_uid: &str) -> Result<Option<String>> {
        let payload = json!({
            "Level": "Instance",
            "Query": { "SeriesInstanceUID": series_uid },
            "Limit": 1,
        });
        let answers = self.execute_modality_query(modality, payload).await?;
        if let Some(content) = answers.into_iter().next() {
            let sop = content
                .get("0008,0018")
                .and_then(|v| v.get("Value"))
                .and_then(|s| s.as_str())
                .map(|s| s.to_string());
            return Ok(sop);
        }
        Ok(None)
    }

    async fn find_instance_uuid(&self, sop_uid: &str) -> Result<Option<String>> {
        let payload = json!({
            "Level": "Instance",
            "Query": { "SOPInstanceUID": sop_uid },
        });
        let resp = self
            .client
            .post(format!("{}/tools/find", self.base_url))
            .json(&payload)
            .send()
            .await?;
        let ids = resp.json::<Vec<String>>().await?;
        Ok(ids.into_iter().next())
    }

    async fn download_instance_file(&self, uuid: &str) -> Result<Vec<u8>> {
        let bytes = self
            .client
            .get(format!("{}/instances/{}/file", self.base_url, uuid))
            .send()
            .await?
            .bytes()
            .await?;
        Ok(bytes.to_vec())
    }

    async fn delete_instance(&self, uuid: &str) -> Result<()> {
        self.client
            .delete(format!("{}/instances/{}", self.base_url, uuid))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn sample_series_type(
        &self,
        modality: &str,
        study_uid: &str,
        series_uid: &str,
    ) -> Result<Option<String>> {
        if let Some(sop) = self.find_instance_sop(modality, series_uid).await? {
            let identifier = json!({
                "SOPInstanceUID": sop,
                "SeriesInstanceUID": series_uid,
                "StudyInstanceUID": study_uid,
            });
            self.c_move(modality, "Instance", identifier, false).await?;
            if let Some(local_uuid) = self.find_instance_uuid(&sop).await? {
                let dicom_data = self.download_instance_file(&local_uuid).await?;
                let analysis = self.analyze_dicom_data(dicom_data).await;
                let _ = self.delete_instance(&local_uuid).await;
                return analysis;
            }
            return Err(anyhow!("Sample moved but local instance UUID missing"));
        }
        Ok(None)
    }

    async fn analyze_dicom_data(&self, dicom_data: Vec<u8>) -> Result<Option<String>> {
        let part = reqwest::multipart::Part::bytes(dicom_data)
            .file_name("sample.dcm")
            .mime_str("application/dicom")?;
        let form = reqwest::multipart::Form::new().part("dicom_file_list", part);
        let resp = self
            .client
            .post(&self.analyze_url)
            .multipart(form)
            .send()
            .await?;
        if resp.status().is_success() {
            let json_body: Value = resp.json().await?;
            if let Some(arr) = json_body.as_array() {
                if let Some(first) = arr.get(0) {
                    return Ok(first
                        .get("series_type")
                        .and_then(|s| s.as_str())
                        .map(|s| s.to_string()));
                }
            }
        }
        Ok(None)
    }

    async fn wait_for_job(&self, job_id: &str, pb: &ProgressBar) -> Result<()> {
        let mut attempt = 0;
        loop {
            if attempt > 300 {
                return Err(anyhow!("Job timeout"));
            }
            let info: Value = self
                .client
                .get(format!("{}/jobs/{}", self.base_url, job_id))
                .send()
                .await?
                .json()
                .await?;
            let state = info["State"].as_str().unwrap_or("Unknown");
            let progress = info["Progress"].as_i64().unwrap_or(0);
            pb.set_message(format!("Job {}%: {}", progress, state));
            if state == "Success" {
                return Ok(());
            }
            if state == "Failure" {
                return Err(anyhow!("Job failed: {}", info));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            attempt += 1;
        }
    }
}

#[derive(Serialize)]
struct ProcessResult {
    accession: String,
    status: String,
    reason: Vec<String>,
    downloaded_series: Vec<String>,
    matched_series: Vec<String>,
    failed_series: Vec<String>,
    timestamp: DateTime<Utc>,
}

fn summarize_status(downloaded: &[String], reasons: &[String]) -> String {
    if reasons.is_empty() {
        "Success".into()
    } else if !downloaded.is_empty() {
        "Partial".into()
    } else {
        "Failed".into()
    }
}

fn reason_summary(reason: &[String]) -> String {
    reason.join("; ")
}

async fn process_single_accession(
    client: Arc<OrthancClient>,
    accession: String,
    modality: String,
    mp: Arc<MultiProgress>,
    analysis_config: Arc<AnalysisConfig>,
) -> ProcessResult {
    let pb = mp.add(ProgressBar::new_spinner());
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.green} [{prefix}] {msg}")
            .unwrap(),
    );
    pb.set_prefix(accession.clone());
    pb.enable_steady_tick(Duration::from_millis(100));

    let config = &*analysis_config;
    let mut downloaded_series = Vec::new();
    let mut matched_series = Vec::new();
    let mut failed_series = Vec::new();
    let mut reasons = Vec::new();

    pb.set_message("Querying Study...");
    let study_uid = match client.find_study_by_accession(&accession, &modality).await {
        Ok(uid) => uid,
        Err(e) => {
            pb.finish_with_message(format!("{} Query Failed", "✗".red()));
            return ProcessResult {
                accession,
                status: "Failed".into(),
                reason: vec![format!("Study query failed: {}", e)],
                downloaded_series: Vec::new(),
                matched_series: Vec::new(),
                failed_series: Vec::new(),
                timestamp: Utc::now(),
            };
        }
    };

    pb.set_message("Querying Series...");
    let remote_series = match client.get_remote_series(&modality, &study_uid).await {
        Ok(s) => s,
        Err(e) => {
            pb.finish_with_message(format!("{} Series Query Failed", "✗".red()));
            return ProcessResult {
                accession,
                status: "Failed".into(),
                reason: vec![format!("Series query failed: {}", e)],
                downloaded_series: Vec::new(),
                matched_series: Vec::new(),
                failed_series: Vec::new(),
                timestamp: Utc::now(),
            };
        }
    };

    let local_uids = client
        .get_local_series(&study_uid)
        .await
        .unwrap_or_default();

    let total_series = remote_series.len();
    for (idx, series_json) in remote_series.into_iter().enumerate() {
        let (series_uid, series_desc) = client.extract_series_info(&series_json);
        if local_uids.contains(&series_uid) {
            continue;
        }

        pb.set_message(format!(
            " {}/{} series {}",
            idx + 1,
            total_series,
            series_desc
        ));
        let mut should_download_series = false;

        if config.download_all {
            should_download_series = true;
            matched_series.push(series_desc.clone());
        } else if should_download(&series_desc, None, &config) {
            should_download_series = true;
            matched_series.push(series_desc.clone());
        } else {
            match client
                .sample_series_type(&modality, &study_uid, &series_uid)
                .await
            {
                Ok(Some(series_type)) => {
                    pb.set_message(format!("Sample result: {}", series_type));
                    if should_download(&series_desc, Some(series_type.as_str()), &config) {
                        should_download_series = true;
                        matched_series.push(series_desc.clone());
                    } else {
                        reasons.push(format!(
                            "{} analyzed as {} which is not in whitelist",
                            series_desc, series_type
                        ));
                    }
                }
                Ok(None) => {
                    reasons.push(format!("No sample available for {}", series_desc));
                }
                Err(e) => {
                    reasons.push(format!("Sample analysis failed for {}: {}", series_desc, e));
                }
            }
        }

        if !should_download_series {
            continue;
        }

        pb.set_message(format!("Downloading {}...", series_desc));
        let move_payload = json!({
            "SeriesInstanceUID": series_uid,
            "StudyInstanceUID": study_uid,
        });
        match client.c_move(&modality, "Series", move_payload, true).await {
            Ok(Some(job_id)) => {
                if let Err(e) = client.wait_for_job(&job_id, &pb).await {
                    reasons.push(format!("Download job failed for {}: {}", series_desc, e));
                    failed_series.push(series_desc.clone());
                } else {
                    downloaded_series.push(series_desc.clone());
                }
            }
            Ok(None) => {
                reasons.push(format!(
                    "Orthanc synchronous move not supported for {}",
                    series_desc
                ));
                failed_series.push(series_desc.clone());
            }
            Err(e) => {
                reasons.push(format!("Series move failed for {}: {}", series_desc, e));
                failed_series.push(series_desc.clone());
            }
        }
    }

    pb.finish_with_message(format!("{} Done", "✓".green()));

    let status = summarize_status(&downloaded_series, &reasons);

    ProcessResult {
        accession,
        status,
        reason: reasons,
        downloaded_series,
        matched_series,
        failed_series,
        timestamp: Utc::now(),
    }
}

fn write_reports(csv_path: &PathBuf, json_path: &PathBuf, results: &[ProcessResult]) -> Result<()> {
    write_csv_report(csv_path, results)?;
    write_json_report(json_path, results)?;
    Ok(())
}

fn write_json_report(path: &PathBuf, results: &[ProcessResult]) -> Result<()> {
    let file = File::create(path).context("Failed to create JSON report")?;
    serde_json::to_writer_pretty(file, results).context("Failed to write JSON report")?;
    Ok(())
}

fn write_csv_report(path: &PathBuf, results: &[ProcessResult]) -> Result<()> {
    let mut wtr = csv::Writer::from_path(path).context("Failed to create CSV report")?;
    wtr.write_record(&[
        "AccessionNumber",
        "Status",
        "Reason",
        "DownloadedSeriesCount",
        "MatchedSeriesCount",
        "FailedSeriesCount",
        "Timestamp",
    ])?;
    for result in results {
        let reason_text = reason_summary(&result.reason);
        let downloaded_count = result.downloaded_series.len().to_string();
        let matched_count = result.matched_series.len().to_string();
        let failed_count = result.failed_series.len().to_string();
        let timestamp = result.timestamp.to_rfc3339();
        wtr.write_record(&[
            result.accession.as_str(),
            result.status.as_str(),
            reason_text.as_str(),
            downloaded_count.as_str(),
            matched_count.as_str(),
            failed_count.as_str(),
            timestamp.as_str(),
        ])?;
    }
    wtr.flush()?;
    Ok(())
}

#[derive(Parser)]
#[command(name = "dicom_download_cli")]
#[command(about = "Orthanc DICOM Batch Downloader", long_about = None)]
struct Cli {
    #[arg(long, help = "INFINTT-SERVER")]
    modality: Option<String>,

    #[arg(long, help = "ORTHANC | RADAX")]
    target: Option<String>,

    #[arg(
        long,
        help = "http://10.103.1.193/orthanc-a | http://10.103.51.1:8042/"
    )]
    url: Option<String>,

    #[arg(
        long,
        help = "http://10.103.1.193:8000/api/v1/series/dicom/analyze/by-upload | http://10.103.51.1:8000/api/v1/series/dicom/analyze/by-upload"
    )]
    analyze_url: Option<String>,

    #[arg(long)]
    username: Option<String>,

    #[arg(long)]
    password: Option<String>,

    #[arg(short, long)]
    input: PathBuf,

    #[arg(
        long,
        value_name = "FILE",
        help = "TOML file to override runtime/analysis settings"
    )]
    config: Option<PathBuf>,

    #[arg(long)]
    report_csv: Option<PathBuf>,

    #[arg(long)]
    report_json: Option<PathBuf>,

    #[arg(short, long)]
    concurrency: Option<usize>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Cli::parse();

    let config_path = args
        .config
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));
    let runtime_file = load_runtime_config(Some(&config_path))?;
    let effective = merge_runtime_config(&args, runtime_file);

    let client = Arc::new(OrthancClient::new(
        &effective.url,
        &effective.analyze_url,
        &effective.target,
        effective.username.clone(),
        effective.password.clone(),
    )?);

    println!("Reading input file: {:?}", args.input);
    let accessions = parse_input_file(&args.input).context("Failed to parse input file")?;
    println!("Found {} accessions to process.", accessions.len());

    let analysis_config = Arc::new(AnalysisConfig::load(Some(&config_path))?);
    let mp = Arc::new(MultiProgress::new());

    let results: Vec<ProcessResult> = stream::iter(accessions)
        .map(|acc| {
            let client = client.clone();
            let modality = effective.modality.clone();
            let mp = mp.clone();
            let analysis_config = analysis_config.clone();
            async move {
                process_single_accession(client, acc, modality, mp, analysis_config).await
            }
        })
        .buffer_unordered(effective.concurrency)
        .collect()
        .await;

    println!("\nProcessing complete. Writing reports...");
    write_reports(&effective.report_csv, &effective.report_json, &results)?;
    let success_count = results.iter().filter(|r| r.status == "Success").count();
    println!(
        "Summary: {} Success, {} Partial/Failed. Reports -> CSV: {:?}, JSON: {:?}",
        success_count,
        results.len() - success_count,
        effective.report_csv,
        effective.report_json
    );

    Ok(())
}
