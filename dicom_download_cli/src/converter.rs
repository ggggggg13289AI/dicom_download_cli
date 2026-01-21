//! dcm2niix integration for DICOM to NIfTI conversion.
//!
//! This module provides functions to convert downloaded DICOM series to NIfTI format
//! using the external dcm2niix tool. NIfTI files are output to a separate directory
//! from the DICOM source files.

#![allow(dead_code)] // TODO: 整合至 download subcommand 時移除

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

/// Result of a dcm2niix conversion operation.
#[derive(Debug, Clone)]
pub struct ConversionResult {
    /// Whether the conversion succeeded.
    pub success: bool,
    /// Paths to generated NIfTI files.
    pub nifti_files: Vec<PathBuf>,
    /// Paths to generated JSON sidecar files.
    pub json_files: Vec<PathBuf>,
    /// Error message if conversion failed.
    pub error: Option<String>,
    /// Time taken in milliseconds.
    pub elapsed_ms: u64,
}

/// Check if dcm2niix is available at the specified path.
///
/// Returns `true` if dcm2niix is found and executable, `false` otherwise.
pub fn check_dcm2niix_available(path: &str) -> bool {
    std::process::Command::new(path)
        .arg("-h")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Convert a series directory from DICOM to NIfTI using dcm2niix.
///
/// The NIfTI files are written to a separate output directory with the specified
/// series name as the filename.
///
/// # Arguments
/// * `dicom_dir` - Directory containing DICOM files for a single series
/// * `output_dir` - Directory where NIfTI files will be written
/// * `series_name` - Name to use for output files (without extension)
/// * `dcm2niix_path` - Path to dcm2niix executable
/// * `extra_args` - Additional arguments to pass to dcm2niix (e.g., ["-z", "y", "-b", "y"])
///
/// # Returns
/// A `ConversionResult` indicating success/failure and listing generated files.
///
/// # Example
/// ```ignore
/// let result = convert_series_to_nifti(
///     Path::new("./dicom/study/T1"),
///     Path::new("./niix/study"),
///     "T1",
///     "dcm2niix",
///     &["-z".into(), "y".into(), "-b".into(), "y".into()],
/// ).await?;
/// // Generates: ./niix/study/T1.nii.gz and ./niix/study/T1.json
/// ```
pub async fn convert_series_to_nifti(
    dicom_dir: &Path,
    output_dir: &Path,
    series_name: &str,
    dcm2niix_path: &str,
    extra_args: &[String],
) -> Result<ConversionResult> {
    let start = std::time::Instant::now();

    // Ensure output directory exists
    tokio::fs::create_dir_all(output_dir).await?;

    // Build command: dcm2niix [extra_args] -f <series_name> -o <output_dir> <dicom_dir>
    let output = Command::new(dcm2niix_path)
        .args(extra_args)
        .arg("-f")
        .arg(series_name)
        .arg("-o")
        .arg(output_dir)
        .arg(dicom_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    let elapsed_ms = start.elapsed().as_millis() as u64;

    // dcm2niix returns 0 even when no images are converted (e.g., for SR DICOM)
    // Check if any NIfTI files were actually created
    let (nifti_files, json_files) = find_output_files(output_dir, series_name).await?;

    if output.status.success() {
        Ok(ConversionResult {
            success: !nifti_files.is_empty(),
            nifti_files,
            json_files,
            error: None,
            elapsed_ms,
        })
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let error_msg = if stderr.is_empty() {
            stdout.to_string()
        } else {
            stderr.to_string()
        };
        Ok(ConversionResult {
            success: false,
            nifti_files: vec![],
            json_files: vec![],
            error: Some(error_msg),
            elapsed_ms,
        })
    }
}

/// Find NIfTI and JSON files matching the series name pattern in output directory.
///
/// dcm2niix may append suffixes like `_e1`, `_ph` for multi-echo or phase images,
/// so we search for files starting with the series name.
async fn find_output_files(dir: &Path, series_name: &str) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let mut nifti_files = Vec::new();
    let mut json_files = Vec::new();
    let mut entries = tokio::fs::read_dir(dir).await?;

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let filename = path.file_name().unwrap_or_default().to_string_lossy();

        // Check if filename starts with series_name
        if filename.starts_with(series_name) {
            if filename.ends_with(".nii.gz") || filename.ends_with(".nii") {
                nifti_files.push(path);
            } else if filename.ends_with(".json") {
                json_files.push(path);
            }
        }
    }

    Ok((nifti_files, json_files))
}

/// Find all NIfTI files (.nii, .nii.gz) in a directory.
pub async fn find_nifti_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut nifti_files = Vec::new();
    let mut entries = tokio::fs::read_dir(dir).await?;

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let filename = path.file_name().unwrap_or_default().to_string_lossy();

        // Match .nii.gz or .nii files
        if filename.ends_with(".nii.gz") || filename.ends_with(".nii") {
            nifti_files.push(path);
        }
    }

    Ok(nifti_files)
}

/// Delete all DICOM files (.dcm) in a directory after successful conversion.
pub async fn delete_dicom_files(dir: &Path) -> Result<usize> {
    let mut deleted_count = 0;
    let mut entries = tokio::fs::read_dir(dir).await?;

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if let Some(ext) = path.extension() {
            if ext.to_string_lossy().to_lowercase() == "dcm" {
                if let Err(e) = tokio::fs::remove_file(&path).await {
                    eprintln!("Warning: Failed to delete {}: {}", path.display(), e);
                } else {
                    deleted_count += 1;
                }
            }
        }
    }

    Ok(deleted_count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_dcm2niix_not_found() {
        // Test with a non-existent path
        assert!(!check_dcm2niix_available("nonexistent_dcm2niix_binary_xyz"));
    }
}
