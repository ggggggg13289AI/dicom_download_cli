//! DICOM file structure checker and fixer.
//!
//! This module provides functionality to check and fix common DICOM file organization issues:
//! - DWI series: Files misplaced between DWI0 and DWI1000 folders based on b-value
//! - ADC series: Duplicate ADC folders that should be removed

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use dicom_object::{open_file, Tag};
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::fs;

// ============================================================================
// Data Structures
// ============================================================================

/// Type of action to perform on a file
#[derive(Debug, Clone, Serialize, PartialEq)]
pub enum ActionType {
    Move,
    Delete,
}

/// Type of check performed
#[derive(Debug, Clone, Serialize)]
pub enum CheckType {
    DWI,
    ADC,
}

/// A single file action (move or delete)
#[derive(Debug, Clone, Serialize)]
pub struct FileAction {
    pub source_path: PathBuf,
    pub action_type: ActionType,
    pub target_path: Option<PathBuf>,
    pub reason: String,
}

/// Result of checking a single series
#[derive(Debug, Clone, Serialize)]
pub struct SeriesCheckResult {
    pub series_folder: String,
    pub check_type: CheckType,
    pub files_checked: usize,
    pub actions: Vec<FileAction>,
}

/// Result of checking a single study
#[derive(Debug, Clone, Serialize)]
pub struct StudyCheckResult {
    pub study_folder: String,
    pub series_results: Vec<SeriesCheckResult>,
    pub total_moves: usize,
    pub total_deletes: usize,
}

/// Summary statistics for the check operation
#[derive(Debug, Clone, Serialize, Default)]
pub struct CheckSummary {
    pub total_studies: usize,
    pub total_series_checked: usize,
    pub total_files_checked: usize,
    pub total_moves: usize,
    pub total_deletes: usize,
    pub dwi_fixes: usize,
    pub adc_duplicates_removed: usize,
}

/// Complete check report
#[derive(Debug, Clone, Serialize)]
pub struct CheckReport {
    pub input_path: PathBuf,
    pub timestamp: DateTime<Utc>,
    pub dry_run: bool,
    pub studies: Vec<StudyCheckResult>,
    pub summary: CheckSummary,
}

// ============================================================================
// DICOM Tag Reading
// ============================================================================

/// Read the Diffusion b-value from a DICOM file.
/// Checks multiple locations where b-value might be stored:
/// 1. (0018,9087) DiffusionBValue - direct tag
/// 2. (0018,9117) MRDiffusionSequence → (0018,9087)
/// 3. (5200,9229) SharedFunctionalGroupsSequence → MRDiffusionSequence → DiffusionBValue
/// 4. (0043,1039) Private GE tag for b-value
/// 5. (0019,100c) Siemens private tag for b-value
///
/// Returns None if b-value is not found or is 0.
/// Returns Some(value) for positive b-values.
fn read_bvalue(path: &Path) -> Result<Option<u32>> {
    let obj = open_file(path).context("Failed to open DICOM file")?;

    // Helper macro to convert element to u32
    macro_rules! elem_to_u32 {
        ($elem:expr) => {{
            let elem = $elem;
            let mut result: Option<u32> = None;
            if let Ok(val) = elem.to_float32() {
                result = Some(val.round() as u32);
            } else if let Ok(val) = elem.to_int::<i32>() {
                result = Some(val.unsigned_abs());
            } else if let Ok(val_str) = elem.to_str() {
                if let Ok(val) = val_str.trim().parse::<f32>() {
                    result = Some(val.round() as u32);
                }
            }
            result
        }};
    }

    // Method 1: Try primary tag (0018,9087) DiffusionBValue
    if let Ok(elem) = obj.element_by_name("DiffusionBValue") {
        if let Some(bval) = elem_to_u32!(elem) {
            return Ok(if bval == 0 { None } else { Some(bval) });
        }
    }

    // Method 2: Try MR Diffusion Sequence (0018,9117) → DiffusionBValue
    if let Ok(seq) = obj.element_by_name("MRDiffusionSequence") {
        if let Some(items) = seq.items() {
            if let Some(first_item) = items.first() {
                if let Ok(bval_elem) = first_item.element_by_name("DiffusionBValue") {
                    if let Some(bval) = elem_to_u32!(bval_elem) {
                        return Ok(if bval == 0 { None } else { Some(bval) });
                    }
                }
            }
        }
    }

    // Method 3: Try Shared Functional Groups Sequence (5200,9229)
    if let Ok(shared_seq) = obj.element(Tag(0x5200, 0x9229)) {
        if let Some(items) = shared_seq.items() {
            for item in items {
                if let Ok(diff_seq) = item.element_by_name("MRDiffusionSequence") {
                    if let Some(diff_items) = diff_seq.items() {
                        if let Some(diff_item) = diff_items.first() {
                            if let Ok(bval_elem) = diff_item.element_by_name("DiffusionBValue") {
                                if let Some(bval) = elem_to_u32!(bval_elem) {
                                    return Ok(if bval == 0 { None } else { Some(bval) });
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Method 4: Try Per-frame Functional Groups Sequence (5200,9230)
    if let Ok(perframe_seq) = obj.element(Tag(0x5200, 0x9230)) {
        if let Some(items) = perframe_seq.items() {
            if let Some(first_frame) = items.first() {
                if let Ok(diff_seq) = first_frame.element_by_name("MRDiffusionSequence") {
                    if let Some(diff_items) = diff_seq.items() {
                        if let Some(diff_item) = diff_items.first() {
                            if let Ok(bval_elem) = diff_item.element_by_name("DiffusionBValue") {
                                if let Some(bval) = elem_to_u32!(bval_elem) {
                                    return Ok(if bval == 0 { None } else { Some(bval) });
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Method 5: Try GE private tag (0043,1039)
    if let Ok(elem) = obj.element(Tag(0x0043, 0x1039)) {
        if let Ok(val_str) = elem.to_str() {
            // Parse first number from string like "1000\0\0"
            if let Some(first_part) = val_str.split(['\\', '/', ' ']).next() {
                if let Ok(bval) = first_part.trim().parse::<u32>() {
                    return Ok(if bval == 0 { None } else { Some(bval) });
                }
            }
        }
        if let Some(bval) = elem_to_u32!(elem) {
            return Ok(if bval == 0 { None } else { Some(bval) });
        }
    }

    // Method 6: Try Siemens private tag (0019,100c)
    if let Ok(elem) = obj.element(Tag(0x0019, 0x100c)) {
        if let Some(bval) = elem_to_u32!(elem) {
            return Ok(if bval == 0 { None } else { Some(bval) });
        }
    }

    // b-value not found - treat as b=0 (DWI0)
    // This is the expected behavior when the tag is missing or null
    Ok(None)
}

/// Read the SOP Instance UID (0008,0018) from a DICOM file.
fn read_sop_instance_uid(path: &Path) -> Result<String> {
    let obj = open_file(path).context("Failed to open DICOM file")?;
    let elem = obj
        .element_by_name("SOPInstanceUID")
        .context("SOPInstanceUID not found")?;
    Ok(elem.to_str()?.trim().to_string())
}

// ============================================================================
// File System Helpers
// ============================================================================

/// List all .dcm files in a directory (non-recursive).
async fn list_dcm_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut entries = fs::read_dir(dir).await?;

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.is_file()
            && path
                .extension()
                .map(|e| e.to_ascii_lowercase() == "dcm")
                .unwrap_or(false)
        {
            files.push(path);
        }
    }

    Ok(files)
}

/// Find all DWI-related folders in a study directory.
/// Matches folders named exactly "DWI0" or "DWI1000".
async fn find_dwi_folders(study_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut folders = Vec::new();
    let mut entries = fs::read_dir(study_dir).await?;

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.is_dir() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name == "DWI0" || name == "DWI1000" {
                    folders.push(path);
                }
            }
        }
    }

    Ok(folders)
}

/// Find all ADC-related folders in a study directory.
/// Matches folders named "ADC" or starting with "ADC_".
async fn find_adc_folders(study_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut folders = Vec::new();
    let mut entries = fs::read_dir(study_dir).await?;

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.is_dir() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name == "ADC" || name.starts_with("ADC_") {
                    folders.push(path);
                }
            }
        }
    }

    Ok(folders)
}

/// Check if a directory is empty.
async fn is_dir_empty(dir: &Path) -> Result<bool> {
    let mut entries = fs::read_dir(dir).await?;
    Ok(entries.next_entry().await?.is_none())
}

/// Remove a directory if it's empty.
async fn remove_if_empty(dir: &Path) -> Result<bool> {
    if is_dir_empty(dir).await? {
        fs::remove_dir(dir).await?;
        Ok(true)
    } else {
        Ok(false)
    }
}

// ============================================================================
// DWI Check Logic
// ============================================================================

/// Check DWI series for misplaced files based on b-value.
///
/// Rules:
/// - b-value is None or 0 → should be in DWI0
/// - b-value == 1000 → should be in DWI1000
///
/// If only DWI0 exists but contains b=1000 files, they will be moved to a new DWI1000 folder.
/// If only DWI1000 exists but contains b=0 files, they will be moved to a new DWI0 folder.
pub async fn check_dwi_series(study_dir: &Path) -> Result<Vec<SeriesCheckResult>> {
    let dwi_folders = find_dwi_folders(study_dir).await?;

    // Need at least one DWI folder to check
    if dwi_folders.is_empty() {
        return Ok(vec![]);
    }

    let mut results = Vec::new();

    for folder in &dwi_folders {
        let folder_name = folder
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        let is_dwi0_folder = folder_name == "DWI0";

        let dcm_files = list_dcm_files(folder).await?;
        let mut actions = Vec::new();
        let mut files_checked = 0;

        for dcm_file in &dcm_files {
            files_checked += 1;
            match read_bvalue(dcm_file) {
                Ok(bvalue) => {
                    // Determine where this file should be
                    let should_be_in_dwi0 = bvalue.is_none() || bvalue == Some(0);
                    let should_be_in_dwi1000 = bvalue == Some(1000);

                    let needs_move = if is_dwi0_folder {
                        // File is in DWI0 but should be in DWI1000
                        should_be_in_dwi1000
                    } else {
                        // File is in DWI1000 but should be in DWI0
                        should_be_in_dwi0
                    };

                    if needs_move {
                        let target_folder_name = if should_be_in_dwi0 { "DWI0" } else { "DWI1000" };
                        let target_folder = study_dir.join(target_folder_name);
                        let target_path = target_folder.join(dcm_file.file_name().unwrap());

                        actions.push(FileAction {
                            source_path: dcm_file.clone(),
                            action_type: ActionType::Move,
                            target_path: Some(target_path),
                            reason: format!(
                                "b-value={} should be in {}",
                                bvalue.map(|v| v.to_string()).unwrap_or("0/None".to_string()),
                                target_folder_name
                            ),
                        });
                    }
                }
                Err(e) => {
                    // This only happens if the DICOM file itself cannot be opened/parsed
                    eprintln!(
                        "Warning: Failed to read DICOM file {}: {}",
                        dcm_file.file_name().unwrap_or_default().to_string_lossy(),
                        e
                    );
                }
            }
        }

        // Always report results if we checked files, even if no actions needed
        results.push(SeriesCheckResult {
            series_folder: folder_name.to_string(),
            check_type: CheckType::DWI,
            files_checked,
            actions,
        });
    }

    Ok(results)
}

// ============================================================================
// ADC Check Logic
// ============================================================================

/// Collect SOP Instance UIDs from all DICOM files in a directory.
async fn collect_sop_instance_uids(dir: &Path) -> Result<HashSet<String>> {
    let mut uids = HashSet::new();
    let dcm_files = list_dcm_files(dir).await?;

    for file in dcm_files {
        match read_sop_instance_uid(&file) {
            Ok(uid) => {
                uids.insert(uid);
            }
            Err(e) => {
                eprintln!(
                    "Warning: Failed to read SOP Instance UID from {}: {}",
                    file.display(),
                    e
                );
            }
        }
    }

    Ok(uids)
}

/// Check ADC series for duplicates.
///
/// Rules:
/// - If only one ADC folder exists, no check needed
/// - If multiple ADC folders exist (ADC, ADC_3, ADC_350, etc.):
///   - Check if "ADC" folder's SOP Instance UIDs are all contained in numbered ADC folders
///   - If yes, "ADC" is a duplicate and should be deleted
pub async fn check_adc_series(study_dir: &Path) -> Result<Vec<SeriesCheckResult>> {
    let adc_folders = find_adc_folders(study_dir).await?;

    if adc_folders.len() <= 1 {
        // Only one or no ADC folder, no check needed
        return Ok(vec![]);
    }

    // Separate "pure ADC" from "numbered ADC" folders
    let (pure_adc, numbered_adc): (Vec<_>, Vec<_>) = adc_folders.iter().partition(|f| {
        f.file_name()
            .and_then(|n| n.to_str())
            .map(|n| n == "ADC")
            .unwrap_or(false)
    });

    if pure_adc.is_empty() || numbered_adc.is_empty() {
        // No pure ADC or no numbered ADC folders, no check needed
        return Ok(vec![]);
    }

    let pure_adc_folder = &pure_adc[0];

    // Collect UIDs from pure ADC folder
    let pure_adc_uids = collect_sop_instance_uids(pure_adc_folder).await?;

    if pure_adc_uids.is_empty() {
        // Empty ADC folder
        return Ok(vec![]);
    }

    // Collect UIDs from all numbered ADC folders
    let mut all_numbered_uids = HashSet::new();
    for folder in &numbered_adc {
        let uids = collect_sop_instance_uids(folder).await?;
        all_numbered_uids.extend(uids);
    }

    // Check if all pure ADC UIDs exist in numbered ADC folders
    let is_duplicate = pure_adc_uids
        .iter()
        .all(|uid| all_numbered_uids.contains(uid));

    let mut results = Vec::new();

    if is_duplicate {
        let dcm_files = list_dcm_files(pure_adc_folder).await?;
        let mut actions = Vec::new();

        for dcm_file in &dcm_files {
            actions.push(FileAction {
                source_path: dcm_file.clone(),
                action_type: ActionType::Delete,
                target_path: None,
                reason: format!(
                    "Duplicate: all {} UIDs exist in numbered ADC folders ({:?})",
                    pure_adc_uids.len(),
                    numbered_adc
                        .iter()
                        .filter_map(|f| f.file_name().and_then(|n| n.to_str()))
                        .collect::<Vec<_>>()
                ),
            });
        }

        results.push(SeriesCheckResult {
            series_folder: "ADC".to_string(),
            check_type: CheckType::ADC,
            files_checked: dcm_files.len(),
            actions,
        });
    }

    Ok(results)
}

// ============================================================================
// Execution Logic
// ============================================================================

/// Execute file actions (move or delete).
/// Returns the number of successful operations.
pub async fn execute_actions(actions: &[FileAction], dry_run: bool) -> Result<(usize, usize)> {
    let mut moves = 0;
    let mut deletes = 0;

    // Track folders that might become empty
    let mut folders_to_check: HashSet<PathBuf> = HashSet::new();

    for action in actions {
        match action.action_type {
            ActionType::Move => {
                if let Some(target_path) = &action.target_path {
                    if dry_run {
                        println!(
                            "[DRY-RUN] Would move: {} -> {}",
                            action.source_path.display(),
                            target_path.display()
                        );
                    } else {
                        // Ensure target directory exists
                        if let Some(parent) = target_path.parent() {
                            fs::create_dir_all(parent).await?;
                        }

                        // Move file
                        fs::rename(&action.source_path, target_path)
                            .await
                            .with_context(|| {
                                format!(
                                    "Failed to move {} to {}",
                                    action.source_path.display(),
                                    target_path.display()
                                )
                            })?;

                        // Track source folder for cleanup
                        if let Some(parent) = action.source_path.parent() {
                            folders_to_check.insert(parent.to_path_buf());
                        }

                        println!(
                            "Moved: {} -> {}",
                            action.source_path.display(),
                            target_path.display()
                        );
                    }
                    moves += 1;
                }
            }
            ActionType::Delete => {
                if dry_run {
                    println!("[DRY-RUN] Would delete: {}", action.source_path.display());
                } else {
                    fs::remove_file(&action.source_path)
                        .await
                        .with_context(|| {
                            format!("Failed to delete {}", action.source_path.display())
                        })?;

                    // Track source folder for cleanup
                    if let Some(parent) = action.source_path.parent() {
                        folders_to_check.insert(parent.to_path_buf());
                    }

                    println!("Deleted: {}", action.source_path.display());
                }
                deletes += 1;
            }
        }
    }

    // Clean up empty folders
    if !dry_run {
        for folder in folders_to_check {
            if folder.exists() {
                match remove_if_empty(&folder).await {
                    Ok(true) => println!("Removed empty folder: {}", folder.display()),
                    Ok(false) => {}
                    Err(e) => eprintln!(
                        "Warning: Failed to check/remove folder {}: {}",
                        folder.display(),
                        e
                    ),
                }
            }
        }
    }

    Ok((moves, deletes))
}

// ============================================================================
// Main Check Function
// ============================================================================

/// Run the complete check on a directory structure.
///
/// Expected structure:
/// ```
/// input_dir/
/// └── dicom/
///     └── PatientID_StudyDate_Modality_Accession/
///         ├── DWI0/
///         ├── DWI1000/
///         ├── ADC/
///         └── ADC_3/
/// ```
pub async fn run_check(input_dir: &Path, dry_run: bool) -> Result<CheckReport> {
    let dicom_dir = input_dir.join("dicom");

    if !dicom_dir.exists() {
        // Try input_dir directly if no dicom/ subdirectory
        return run_check_on_dir(input_dir, dry_run).await;
    }

    run_check_on_dir(&dicom_dir, dry_run).await
}

async fn run_check_on_dir(base_dir: &Path, dry_run: bool) -> Result<CheckReport> {
    let mut studies = Vec::new();
    let mut summary = CheckSummary::default();

    // Iterate over study directories
    let mut entries = fs::read_dir(base_dir).await?;

    while let Some(entry) = entries.next_entry().await? {
        let study_dir = entry.path();
        if !study_dir.is_dir() {
            continue;
        }

        let study_folder = study_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        println!("\nChecking study: {}", study_folder);

        let mut series_results = Vec::new();
        let mut study_moves = 0;
        let mut study_deletes = 0;

        // Check DWI series
        match check_dwi_series(&study_dir).await {
            Ok(dwi_results) => {
                for result in dwi_results {
                    summary.total_files_checked += result.files_checked;
                    summary.total_series_checked += 1;

                    if !result.actions.is_empty() {
                        // Execute actions
                        let (moves, _deletes) = execute_actions(&result.actions, dry_run).await?;
                        study_moves += moves;
                        summary.dwi_fixes += moves;

                        series_results.push(result);
                    } else {
                        println!("  {} - {} files checked, no issues found", result.series_folder, result.files_checked);
                    }
                }
            }
            Err(e) => {
                eprintln!("Warning: DWI check failed for {}: {}", study_folder, e);
            }
        }

        // Check ADC series
        match check_adc_series(&study_dir).await {
            Ok(adc_results) => {
                for result in adc_results {
                    summary.total_files_checked += result.files_checked;

                    if !result.actions.is_empty() {
                        // Execute actions
                        let (_moves, deletes) = execute_actions(&result.actions, dry_run).await?;
                        study_deletes += deletes;
                        summary.adc_duplicates_removed += deletes;

                        series_results.push(result);
                        summary.total_series_checked += 1;
                    }
                }
            }
            Err(e) => {
                eprintln!("Warning: ADC check failed for {}: {}", study_folder, e);
            }
        }

        if !series_results.is_empty() {
            studies.push(StudyCheckResult {
                study_folder,
                series_results,
                total_moves: study_moves,
                total_deletes: study_deletes,
            });

            summary.total_moves += study_moves;
            summary.total_deletes += study_deletes;
        }

        summary.total_studies += 1;
    }

    Ok(CheckReport {
        input_path: base_dir.to_path_buf(),
        timestamp: Utc::now(),
        dry_run,
        studies,
        summary,
    })
}

// ============================================================================
// Report Writing
// ============================================================================

/// Write check report to CSV file.
pub fn write_csv_report(report: &CheckReport, path: &Path) -> Result<()> {
    let mut wtr = csv::Writer::from_path(path)?;

    // Write header
    wtr.write_record([
        "study_folder",
        "series_folder",
        "check_type",
        "action",
        "source_path",
        "target_path",
        "reason",
    ])?;

    // Write data
    for study in &report.studies {
        for series in &study.series_results {
            let check_type = match series.check_type {
                CheckType::DWI => "DWI",
                CheckType::ADC => "ADC",
            };

            for action in &series.actions {
                let action_type = match action.action_type {
                    ActionType::Move => "Move",
                    ActionType::Delete => "Delete",
                };

                wtr.write_record([
                    &study.study_folder,
                    &series.series_folder,
                    check_type,
                    action_type,
                    &action.source_path.to_string_lossy(),
                    &action
                        .target_path
                        .as_ref()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    &action.reason,
                ])?;
            }
        }
    }

    wtr.flush()?;
    println!("CSV report written to: {}", path.display());
    Ok(())
}

/// Write check report to JSON file.
pub fn write_json_report(report: &CheckReport, path: &Path) -> Result<()> {
    let json = serde_json::to_string_pretty(report)?;
    std::fs::write(path, json)?;
    println!("JSON report written to: {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_action_type_serialization() {
        assert_eq!(
            serde_json::to_string(&ActionType::Move).unwrap(),
            "\"Move\""
        );
        assert_eq!(
            serde_json::to_string(&ActionType::Delete).unwrap(),
            "\"Delete\""
        );
    }
}
