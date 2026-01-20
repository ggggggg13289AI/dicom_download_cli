use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::fs::{self, File};
use std::path::PathBuf;

/// Default runtime configuration file path.
pub const DEFAULT_CONFIG_PATH: &str = "config/dicom_download_cli.toml";
/// Default Orthanc modality AET that the CLI queries.
pub const DEFAULT_MODALITY: &str = "INFINTT-SERVER";
/// Default destination AET that receives downloaded series.
pub const DEFAULT_TARGET: &str = "RADAX";
/// Default Orthanc base URL used if no override is supplied.
pub const DEFAULT_URL: &str = "http://10.103.51.1:8042/";
/// Default analysis service URL that classifies downloaded DICOM samples.
pub const DEFAULT_ANALYZE_URL: &str =
    "http://10.103.51.1:8000/api/v1/series/dicom/analyze/by-upload";
/// Default CSV path for the summary report.
pub const DEFAULT_REPORT_CSV: &str = "report.csv";
/// Default JSON path for the summary report.
pub const DEFAULT_REPORT_JSON: &str = "report.json";
/// Default number of simultaneous accession workers.
pub const DEFAULT_CONCURRENCY: usize = 5;

/// Determines which series should be downloaded by the CLI.
pub struct AnalysisConfig {
    pub series_whitelist: HashSet<String>,
    pub direct_download_keywords: HashSet<String>,
    pub enable_whitelist: bool,
    pub enable_direct_keywords: bool,
    pub download_all: bool,
}

impl AnalysisConfig {
    /// Returns the CLI's hard-coded defaults for whitelists and keyword matching.
    pub fn default() -> Self {
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

    /// Loads an analysis config file if it exists, falling back to defaults otherwise.
    ///
    /// When `path` is `None` or the file is missing, the defaults from `AnalysisConfig::default`
    /// are returned.
    pub fn load(path: Option<&PathBuf>) -> Result<Self> {
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

    /// Parses the TOML analysis config and sanitizes each collection.
    ///
    /// Empty strings from the file are trimmed and dropped.
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
/// Helper that mirrors the TOML schema for the analysis config file.
struct AnalysisConfigFile {
    enable_whitelist: Option<bool>,
    enable_direct_keywords: Option<bool>,
    download_all: Option<bool>,
    series_whitelist: Option<Vec<String>>,
    direct_download_keywords: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
/// Runtime overrides loaded from the TOML config referenced by `main`.
pub struct RuntimeConfigFile {
    pub url: Option<String>,
    pub analyze_url: Option<String>,
    pub modality: Option<String>,
    pub target: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub concurrency: Option<usize>,
    pub report_csv: Option<PathBuf>,
    pub report_json: Option<PathBuf>,
}

/// Final configuration used throughout the download workflow.
pub struct EffectiveConfig {
    pub url: String,
    pub analyze_url: String,
    pub modality: String,
    pub target: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub concurrency: usize,
    pub report_csv: PathBuf,
    pub report_json: PathBuf,
}

impl EffectiveConfig {
    /// Returns the crate-level defaults before CLI/runtime overrides are merged.
    pub fn defaults() -> Self {
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

/// Attempts to read the runtime config file and deserialize CLI overrides.
///
/// Returns `Ok(None)` when the file is missing so defaults are preserved.
pub fn load_runtime_config(path: Option<&PathBuf>) -> Result<Option<RuntimeConfigFile>> {
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

/// Trims whitespace and drops empty strings when parsing sensitive CLI overrides.
pub fn sanitize_optional_string(value: Option<String>) -> Option<String> {
    value.and_then(|s| {
        let trimmed = s.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

/// Decides if a series should be downloaded based on config flags and analysis tags.
///
/// The priority is: download-all override, direct keyword match, and finally
/// whitelist match against the analysis service result when available.
pub fn should_download(
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

/// Reads accession numbers from a CSV (first column) or JSON array (strings or objects).
///
/// JSON objects may supply `accession`, `AccessionNumber`, or `acc` keys, and empty values are
/// filtered out.
pub fn parse_input_file(path: &PathBuf) -> Result<Vec<String>> {
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
