# dcm2niix 整合說明文件

> 將 dcm2niix (DICOM 轉 NIfTI 轉換器) 整合至 Rust CLI 的 download 工作流程中。

---

## 1. 概述

本功能將 [dcm2niix](https://github.com/rordenlab/dcm2niix) 工具整合至 `dicom_download_cli`，
實現 DICOM 檔案下載後自動轉換為 NIfTI 格式（`.nii.gz`）並產生 BIDS 相容的 JSON sidecar。

### 1.1 功能特點

- **可選啟用**：透過 `--convert` 旗標或設定檔啟用
- **Pipeline 模式**：每個 series 下載完成後立即轉換
- **優雅降級**：轉換失敗不會中斷整體下載流程
- **完整報告**：轉換結果納入 CSV/JSON 報告

---

## 2. 設計原則 (Linus Torvalds Style)

1. **KISS (Keep It Simple, Stupid)**
   - 單一目的函數，非通用後處理框架
   - 只做 dcm2niix 整合，需要時再泛化

2. **分離關注點**
   - 下載歸下載，轉換是可選附加功能
   - 現有下載邏輯完全不受影響

3. **優雅降級**
   - 轉換失敗記錄於日誌，繼續處理下一個 series
   - 不會因單一 series 轉換失敗而中斷整個流程

4. **YAGNI (You Aren't Gonna Need It)**
   - 不預先建立通用後處理框架
   - 專注解決當前需求

---

## 3. 使用方式

### 3.1 CLI 使用

```bash
# 下載並轉換（啟用 dcm2niix）
cargo run -- download -i input.csv --output ./dicom --convert

# 僅下載（現有行為，不轉換）
cargo run -- download -i input.csv --output ./dicom
```

### 3.2 設定檔方式

編輯 `config/dicom_download_cli.toml`：

```toml
[conversion]
# 啟用 dcm2niix 轉換（可被 --convert 旗標覆蓋）
enabled = true

# dcm2niix 執行檔路徑（預設假設在 PATH 中）
dcm2niix_path = "dcm2niix"

# dcm2niix 參數
# -z y = gzip 壓縮
# -f %p_%s = 檔名格式（protocol_series）
# -b y = 產生 BIDS JSON sidecar
dcm2niix_args = ["-z", "y", "-f", "%p_%s", "-b", "y"]

# 轉換成功後是否刪除 DICOM 檔案（預設 false）
delete_dicom_after_conversion = false
```

### 3.3 優先順序

CLI 旗標 > 設定檔 > 程式碼預設值

---

## 4. 輸出結構

### 4.1 目錄結構

執行 `--convert` 後的輸出（DICOM 與 NIfTI 分離）：

```
{output}/
├── dicom/                                              # DICOM 檔案根目錄
│   └── {PatientID}_{StudyDate}_{Modality}_{AccessionNumber}/
│       ├── {SeriesDescription1}/                       # Series 資料夾
│       │   ├── {instance_uuid_1}.dcm                   # 原始 DICOM 檔案
│       │   ├── {instance_uuid_2}.dcm
│       │   └── ...
│       ├── {SeriesDescription2}/
│       │   └── *.dcm
│       └── ...
│
└── niix/                                               # NIfTI 檔案根目錄
    └── {PatientID}_{StudyDate}_{Modality}_{AccessionNumber}/
        ├── {SeriesDescription1}.nii.gz                 # 轉換後 NIfTI (gzip)
        ├── {SeriesDescription1}.json                   # BIDS sidecar
        ├── {SeriesDescription2}.nii.gz
        ├── {SeriesDescription2}.json
        └── ...
```

### 4.2 範例

```
./output/
├── dicom/
│   └── P001_20240115_MR_ACC123/
│       ├── T1_MPRAGE/
│       │   ├── 1.2.840.xxx.1.dcm
│       │   ├── 1.2.840.xxx.2.dcm
│       │   └── ...
│       ├── T2_FLAIR/
│       │   └── *.dcm
│       └── DWI/
│           └── *.dcm
│
└── niix/
    └── P001_20240115_MR_ACC123/
        ├── T1_MPRAGE.nii.gz
        ├── T1_MPRAGE.json
        ├── T2_FLAIR.nii.gz
        ├── T2_FLAIR.json
        ├── DWI.nii.gz
        └── DWI.json
```

### 4.3 檔案說明

| 檔案類型 | 副檔名 | 位置 | 說明 |
|---------|--------|------|------|
| DICOM | `.dcm` | `dicom/` | 原始醫療影像（預設保留） |
| NIfTI | `.nii.gz` | `niix/` | 轉換後的 3D/4D 影像（gzip 壓縮） |
| BIDS sidecar | `.json` | `niix/` | 影像 metadata（TR, TE, 方向等） |

### 4.4 設計優點

- **分離關注點**：DICOM 原始檔與 NIfTI 衍生檔分開存放
- **平行結構**：`niix/` 鏡像 `dicom/` 的 study 資料夾結構
- **易於管理**：可獨立刪除 DICOM 或 NIfTI 檔案
- **清晰命名**：NIfTI 檔案以 series 名稱命名，便於識別

---

## 5. dcm2niix 參數說明

### 5.1 預設參數

| 參數 | 值 | 說明 |
|------|-----|------|
| `-z` | `y` | 輸出 gzip 壓縮的 `.nii.gz` |
| `-f` | `%p_%s` | 檔名格式：`{protocol}_{series}` |
| `-b` | `y` | 產生 BIDS JSON sidecar |

### 5.2 常用參數

| 參數 | 說明 | 範例 |
|------|------|------|
| `-o` | 輸出目錄 | `-o /output/path` |
| `-f` | 檔名格式 | `-f %p_%s_%t` (protocol_series_datetime) |
| `-z` | 壓縮選項 | `y`=gzip, `n`=不壓縮, `i`=internal |
| `-b` | BIDS sidecar | `y`=是, `n`=否, `o`=僅 sidecar |
| `-m` | 合併 2D 切片 | `y`=合併, `n`=分開 |

### 5.3 檔名格式代碼

| 代碼 | 說明 |
|------|------|
| `%p` | Protocol name |
| `%s` | Series number |
| `%t` | Acquisition time |
| `%d` | Series description |
| `%i` | Patient ID |
| `%n` | Patient name |

---

## 6. 前置需求

### 6.1 安裝 dcm2niix

**Windows (使用 winget):**
```powershell
winget install -e --id rordenlab.dcm2niix
```

**Windows (手動安裝):**
1. 從 [GitHub Releases](https://github.com/rordenlab/dcm2niix/releases) 下載
2. 解壓縮並加入 PATH

**macOS:**
```bash
brew install dcm2niix
```

**Linux (Ubuntu/Debian):**
```bash
sudo apt-get install dcm2niix
```

### 6.2 驗證安裝

```bash
dcm2niix -h
```

---

## 7. 錯誤處理

### 7.1 轉換失敗處理

- 失敗的 series 記錄於 `conversion_failed` 欄位
- 錯誤訊息記錄於日誌
- 不影響其他 series 的下載與轉換

### 7.2 常見錯誤

| 錯誤 | 可能原因 | 解決方案 |
|------|----------|----------|
| `dcm2niix not found` | 未安裝或不在 PATH | 安裝 dcm2niix 或設定完整路徑 |
| `Conversion failed` | DICOM 損壞或不支援 | 檢查 DICOM 檔案完整性 |
| `No NIfTI generated` | 非影像 DICOM（如 SR） | 正常現象，跳過即可 |

---

## 8. 報告輸出

### 8.1 CSV 報告新增欄位

| 欄位 | 說明 |
|------|------|
| `ConvertedCount` | 成功轉換的 series 數量 |
| `ConversionFailed` | 轉換失敗的 series 數量 |

### 8.2 JSON 報告新增欄位

```json
{
  "accession": "ACC123",
  "status": "Success",
  "converted_series": ["T1", "T2", "DWI"],
  "conversion_failed": ["SR_Report"]
}
```

---

## 9. 技術實作

### 9.1 模組結構

```
dicom_download_cli/src/
├── main.rs          # CLI 入口、download 流程整合、--convert 旗標處理
├── config.rs        # ConversionConfig 設定解析、RuntimeConfigFile
├── converter.rs     # dcm2niix 子程序處理、轉換結果追蹤
├── processor.rs     # ProcessResult（含 converted_series, conversion_failed）
└── client.rs        # Orthanc HTTP 客戶端
```

### 9.2 資料流程

```
1. 解析 CLI 參數 (--convert, --output)
2. 載入設定檔 ([conversion] 區段)
3. 建立目錄結構:
   - {output}/dicom/  (DICOM 檔案)
   - {output}/niix/   (NIfTI 檔案，僅當 convert enabled)
4. 檢查 dcm2niix 可用性（若啟用轉換）
5. 對每個 accession:
   a. 建立 study 資料夾: dicom/{study_folder}/
   b. 對每個 series:
      i.   下載所有 DICOM instances 到 dicom/{study_folder}/{series}/
      ii.  [if convert enabled] 建立 niix/{study_folder}/
      iii. [if convert enabled] 呼叫 dcm2niix 轉換到 niix/{study_folder}/{series}.nii.gz
      iv.  [if delete_dicom_after_conversion] 刪除 DICOM 檔案
      v.   記錄轉換結果
6. 輸出報告（含轉換統計：ConvertedCount, ConversionFailedCount）
```

### 9.3 關鍵函數

```rust
// converter.rs
pub async fn convert_series_to_nifti(
    dicom_dir: &Path,      // 輸入：包含 DICOM 檔案的 series 目錄
    output_dir: &Path,     // 輸出：NIfTI 檔案的 study 目錄
    series_name: &str,     // NIfTI 檔案名稱（不含副檔名）
    dcm2niix_path: &str,
    extra_args: &[String],
) -> Result<ConversionResult>

pub fn check_dcm2niix_available(path: &str) -> bool

pub async fn delete_dicom_files(dir: &Path) -> Result<usize>

// config.rs
pub struct ConversionConfig {
    pub enabled: Option<bool>,
    pub dcm2niix_path: Option<String>,
    pub dcm2niix_args: Option<Vec<String>>,
    pub delete_dicom_after_conversion: Option<bool>,
}
```

---

## 10. 效能考量

### 10.1 轉換時機

- **Pipeline 模式**：每個 series 下載完成後立即轉換
- 優點：及早發現問題、減少記憶體佔用
- 缺點：無法平行化轉換（但 dcm2niix 本身已高度最佳化）

### 10.2 建議

1. 安裝 `pigz` 以加速 gzip 壓縮
2. 使用 SSD 儲存以提升 I/O 效能
3. 大量轉換時考慮批次處理

---

## 11. 參考資料

- [dcm2niix GitHub](https://github.com/rordenlab/dcm2niix)
- [dcm2niix Wiki](https://github.com/rordenlab/dcm2niix/wiki)
- [BIDS Specification](https://bids-specification.readthedocs.io/)
- [NIfTI Format](https://nifti.nimh.nih.gov/)

---

*文件版本：1.0*
*最後更新：2025-01*
