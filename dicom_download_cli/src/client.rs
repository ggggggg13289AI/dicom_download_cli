use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose, Engine as _};
use indicatif::ProgressBar;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::time::Duration;

#[derive(Clone)]
/// HTTP client that orchestrates Orthanc queries, moves, and analysis calls.
pub struct OrthancClient {
    client: Client,
    pub base_url: String,
    pub analyze_url: String,
    pub target_aet: String,
}

pub struct StudyMeta {
    pub study_uid: Option<String>,
}

pub struct SeriesMeta {
    pub series_uid: Option<String>,
    pub description: Option<String>,
    pub instances: Vec<String>,
}

impl OrthancClient {
    /// Builds a reqwest client configured for Orthanc + analysis endpoints and optional auth.
    ///
    /// Accepts invalid TLS certs, sets request timeout, and applies Basic auth headers when
    /// credentials are provided.
    pub fn new(
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

    /// Uses Orthanc's modality query to turn an accession number into a StudyInstanceUID.
    pub async fn find_study_by_accession(&self, accession: &str, modality: &str) -> Result<String> {
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

    /// Performs a generic Orthanc modality query and collects all returned answer contents.
    pub async fn execute_modality_query(&self, modality: &str, payload: Value) -> Result<Vec<Value>> {
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

    /// Queries Orthanc for all series metadata belonging to a study using `Normalize: true`.
    pub async fn get_remote_series(&self, modality: &str, study_uid: &str) -> Result<Vec<Value>> {
        let payload = json!({
            "Level": "Series",
            "Query": { "StudyInstanceUID": study_uid },
            "Normalize": true,
        });
        self.execute_modality_query(modality, payload).await
    }

    /// Extracts the SeriesInstanceUID and description tags from a normalized response.
    pub fn extract_series_info(&self, series_json: &Value) -> (String, String) {
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

    /// Lists already stored series UUIDs on the local Orthanc for a study.
    pub async fn get_local_series(&self, study_uid: &str) -> Result<HashSet<String>> {
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

    /// Issues an Orthanc C-MOVE request to transfer a study/series/instance to the target AET.
    ///
    /// Returns the job ID when running in async mode so callers can poll its status.
    pub async fn c_move(
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

    /// Finds a single SOPInstanceUID for the series to pull a sample instance.
    pub async fn find_instance_sop(&self, modality: &str, series_uid: &str) -> Result<Option<String>> {
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

    /// Resolves the Orthanc instance UUID for a given SOP instance UID.
    pub async fn find_instance_uuid(&self, sop_uid: &str) -> Result<Option<String>> {
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

    /// Downloads the raw DICOM file bytes of a stored instance in Orthanc.
    pub async fn download_instance_file(&self, uuid: &str) -> Result<Vec<u8>> {
        let bytes = self
            .client
            .get(format!("{}/instances/{}/file", self.base_url, uuid))
            .send()
            .await?
            .bytes()
            .await?;
        Ok(bytes.to_vec())
    }

    pub async fn delete_instance(&self, uuid: &str) -> Result<()> {
        self.client
            .delete(format!("{}/instances/{}", self.base_url, uuid))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn sample_series_type(
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

    pub async fn analyze_dicom_data(&self, dicom_data: Vec<u8>) -> Result<Option<String>> {
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

    pub async fn wait_for_job(&self, job_id: &str, pb: &ProgressBar) -> Result<()> {
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

    /// Queries local Orthanc by AccessionNumber and returns study IDs (Orthanc UUIDs).
    pub async fn find_study_ids_by_accession(&self, accession: &str) -> Result<Vec<String>> {
        let payload = json!({
            "Level": "Study",
            "Query": { "AccessionNumber": accession },
        });
        let resp = self
            .client
            .post(format!("{}/tools/find", self.base_url))
            .json(&payload)
            .send()
            .await?
            .error_for_status()?;
        
        // Support both ["id1", "id2"] and [{"ID": "id1"}, ...]
        let items: Vec<Value> = resp.json().await?;
        let mut ids = Vec::new();
        for item in items {
            if let Some(s) = item.as_str() {
                ids.push(s.to_string());
            } else if let Some(obj) = item.as_object() {
                if let Some(s) = obj.get("ID").and_then(|v| v.as_str()) {
                    ids.push(s.to_string());
                }
            }
        }
        Ok(ids)
    }

    /// Fetches StudyInstanceUID and tags for a local Orthanc study UUID.
    pub async fn get_study_meta(&self, study_id: &str) -> Result<StudyMeta> {
        let resp = self
            .client
            .get(format!("{}/studies/{}", self.base_url, study_id))
            .send()
            .await?
            .error_for_status()?;
        let body: Value = resp.json().await?;
        let study_uid = body
            .get("MainDicomTags")
            .and_then(|t| t.get("StudyInstanceUID"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        Ok(StudyMeta { study_uid })
    }

    /// Returns Orthanc series UUIDs under a study UUID.
    pub async fn list_series_ids(&self, study_id: &str) -> Result<Vec<String>> {
        let resp = self
            .client
            .get(format!("{}/studies/{}/series", self.base_url, study_id))
            .send()
            .await?
            .error_for_status()?;
        
        // Support both ["id1", "id2"] and [{"ID": "id1"}, ...]
        let items: Vec<Value> = resp.json().await?;
        let mut ids = Vec::new();
        for item in items {
            if let Some(s) = item.as_str() {
                ids.push(s.to_string());
            } else if let Some(obj) = item.as_object() {
                if let Some(s) = obj.get("ID").and_then(|v| v.as_str()) {
                    ids.push(s.to_string());
                }
            }
        }
        Ok(ids)
    }

    /// Returns series metadata plus instance IDs for a series UUID.
    pub async fn get_series_meta(&self, series_id: &str) -> Result<SeriesMeta> {
        let resp = self
            .client
            .get(format!("{}/series/{}", self.base_url, series_id))
            .send()
            .await?
            .error_for_status()?;
        let body: Value = resp.json().await?;
        let series_uid = body
            .get("MainDicomTags")
            .and_then(|t| t.get("SeriesInstanceUID"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let description = body
            .get("MainDicomTags")
            .and_then(|t| t.get("SeriesDescription"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let instances: Vec<String> = body
            .get("Instances")
            .and_then(|arr| arr.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        Ok(SeriesMeta {
            series_uid,
            description,
            instances,
        })
    }
}
