# Rust CLI Download 邏輯對齊 Python `download_dicom_async.py` 分析

> "Bad programmers worry about the code. Good programmers worry about data structures and their relationships."
> — Linus Torvalds

---

## 1. 核心問題定義

**目標**：將 Rust CLI 的 `download` 子命令邏輯對齊 Python `scripts/download_dicom_async.py`。

**現狀差異摘要**：

| 面向 | Python (`download_dicom_async.py`) | Rust CLI (`download` subcommand) |
|------|-------------------------------------|----------------------------------|
| **目錄命名** | 人類可讀：`{patient_id}_{study_date}_{modality}_{accession}` | UUID-based：`{study_uid}/{series_uid}/` |
| **Series 命名** | Analyze API 結果或 SeriesDescription（DWI0_003 處理衝突） | SeriesDescription 或 series_uid |
| **Analyze 整合** | ✅ 呼叫 Analyze API 決定 series_type | ❌ 不使用 Analyze |
| **重試機制** | ✅ 3 次重試 + 60s timeout + 線性退避 | ❌ 單次嘗試 |
| **進度追蹤** | ProgressTracker（completed/failed/skipped 計數） | 無進度輸出 |
| **DICOM 解析** | ✅ pydicom 取得 patient_id/study_date/modality | ❌ 不解析 DICOM 標籤 |

---

## 2. 資料結構優先（Linus 第二原則）

在動手寫程式碼之前，先定義正確的資料結構。

### 2.1 目前 Rust 資料結構

```rust
// client.rs
pub struct StudyMeta {
    pub study_uid: Option<String>,
}

pub struct SeriesMeta {
    pub series_uid: Option<String>,
    pub description: Option<String>,
    pub instances: Vec<String>,
}
```

**問題**：缺少人類可讀命名所需的欄位。

### 2.2 對齊所需的資料結構

```rust
/// Study 資訊，包含 DICOM 標籤供目錄命名
pub struct StudyMeta {
    pub study_uid: Option<String>,
    pub patient_id: Option<String>,      // NEW: (0010,0020)
    pub study_date: Option<String>,      // NEW: (0008,0020)
    pub modality: Option<String>,        // NEW: (0008,0060)
    pub accession_number: Option<String>,// NEW: (0008,0050)
}

/// Series 資訊，含 Analyze 結果
pub struct SeriesMeta {
    pub series_uid: Option<String>,
    pub description: Option<String>,
    pub series_number: Option<String>,   // NEW: (0020,0011) for DWI 衝突處理
    pub series_type: Option<String>,     // NEW: Analyze API 結果
    pub instances: Vec<String>,
}

/// 下載計畫：圍繞資料設計程式碼
pub struct DownloadPlan {
    pub study_folder: String,            // 人類可讀目錄名
    pub series: Vec<SeriesDownloadPlan>,
}

pub struct SeriesDownloadPlan {
    pub series_folder: String,           // ADC, DWI0_003 等
    pub instances: Vec<String>,          // Orthanc instance UUIDs
}
```

**為什麼這樣設計**：
- `DownloadPlan` 讓下載流程與命名邏輯分離
- 先建立計畫（分析階段），再執行下載（執行階段）
- 與 Python 的 `build_download_plan()` → `download_series()` 模式一致

---

## 3. Good Taste：消除特殊情況（Linus 第三原則）

### 3.1 Python 的 DWI 命名衝突處理

```python
# 計算每個 series_type 的出現次數
series_type_count = {}
for info in series_info:
    st = info["series_type"]
    series_type_count[st] = series_type_count.get(st, 0) + 1

# 若 DWI0/DWI1000 有多個，加上 SeriesNumber
if series_type_count[series_type] > 1 and series_type in ("DWI0", "DWI1000"):
    series_folder = f"{series_type}_{series_number.zfill(3)}"
else:
    series_folder = series_type
```

**問題**：這是「特殊情況」處理 DWI。

### 3.2 Linus 風格：統一處理模式

```rust
/// 產生 series 資料夾名稱
fn generate_series_folder_name(
    series_type: &str,
    series_number: Option<&str>,
    type_counts: &HashMap<String, usize>,
) -> String {
    let count = *type_counts.get(series_type).unwrap_or(&1);

    // 統一模式：只要同類型有多個，就加編號
    // 不再特殊判斷 DWI0/DWI1000
    if count > 1 {
        let num = series_number
            .map(|n| format!("{:03}", n.parse::<u32>().unwrap_or(0)))
            .unwrap_or_else(|| "000".to_string());
        format!("{}_{}", series_type, num)
    } else {
        series_type.to_string()
    }
}
```

**為什麼更好**：
- 消除 `if series_type in ("DWI0", "DWI1000")` 的特殊判斷
- 任何重複的 series_type 都用相同規則處理
- 程式碼更通用，未來新增其他類型不需修改

---

## 4. 實作計畫（務實優先）

### Phase 1: 擴充資料結構

**檔案**: `client.rs`

1. 修改 `StudyMeta` 加入 DICOM 標籤欄位
2. 修改 `SeriesMeta` 加入 `series_number`
3. 更新 `get_study_meta()` 解析更多標籤
4. 更新 `get_series_meta()` 解析 SeriesNumber

### Phase 2: 加入 DICOM 解析

**新增依賴**: `dicom` crate 或從 DICOM bytes 解析

**選項比較**：

| 方案 | 優點 | 缺點 |
|------|------|------|
| A. 使用 `dicom` crate | 完整解析、類型安全 | 新增依賴、編譯時間增加 |
| B. 從 Orthanc API 取得 | 無新依賴、已有實作 | 需額外 API 呼叫 |
| C. 解析第一個 instance bytes | 與 Python 一致 | 需下載後解析 |

**建議**：採用 **方案 B**，因為：
- Orthanc 的 `/studies/{id}` 和 `/instances/{id}/simplified-tags` 已提供所需標籤
- 不需新增依賴
- 網路開銷可接受（只取一次 metadata）

### Phase 3: 建立下載計畫

**新增函數**: `build_download_plan()`

```rust
async fn build_download_plan(
    client: &OrthancClient,
    accession: &str,
    analyze_url: Option<&str>,
) -> Result<Vec<DownloadPlan>> {
    // 1. 查詢 Study IDs
    // 2. 對每個 Study:
    //    a. 取得 StudyMeta（含 DICOM 標籤）
    //    b. 列出所有 Series
    //    c. 對每個 Series 的第一個 instance 呼叫 Analyze API（若啟用）
    //    d. 計算 series_type 出現次數
    //    e. 產生 series_folder 名稱
    // 3. 組裝 DownloadPlan
}
```

### Phase 4: 重試機制

**新增函數**: `download_with_retry()`

```rust
async fn download_with_retry(
    client: &OrthancClient,
    instance_id: &str,
    dest_path: &Path,
    max_retries: usize,
    timeout: Duration,
) -> Result<DownloadStatus> {
    for attempt in 0..max_retries {
        match tokio::time::timeout(
            timeout,
            client.download_instance_file(instance_id)
        ).await {
            Ok(Ok(data)) => {
                fs::write(dest_path, data).await?;
                return Ok(DownloadStatus::Completed);
            }
            Ok(Err(e)) => {
                if attempt < max_retries - 1 {
                    tokio::time::sleep(Duration::from_secs((attempt + 1) as u64)).await;
                    continue;
                }
                return Ok(DownloadStatus::Failed(e.to_string()));
            }
            Err(_timeout) => {
                if attempt < max_retries - 1 {
                    tokio::time::sleep(Duration::from_secs((attempt + 1) * 2)).await;
                    continue;
                }
                return Ok(DownloadStatus::Failed("timeout".into()));
            }
        }
    }
    unreachable!()
}
```

### Phase 5: 進度追蹤

**新增結構**: `ProgressTracker`

```rust
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct ProgressTracker {
    total: usize,
    completed: AtomicUsize,
    failed: AtomicUsize,
    skipped: AtomicUsize,
    start_time: Instant,
}

impl ProgressTracker {
    pub fn increment(&self, status: DownloadStatus) {
        match status {
            DownloadStatus::Completed => self.completed.fetch_add(1, Ordering::Relaxed),
            DownloadStatus::Failed(_) => self.failed.fetch_add(1, Ordering::Relaxed),
            DownloadStatus::Skipped => self.skipped.fetch_add(1, Ordering::Relaxed),
        };
        self.maybe_print_progress();
    }

    fn maybe_print_progress(&self) {
        let processed = self.completed.load(Ordering::Relaxed)
            + self.failed.load(Ordering::Relaxed)
            + self.skipped.load(Ordering::Relaxed);

        if processed % 10 == 0 || processed == self.total {
            let elapsed = self.start_time.elapsed().as_secs_f64();
            let speed = processed as f64 / elapsed;
            let eta = (self.total - processed) as f64 / speed;
            println!(
                "進度: {}/{} (完成:{} 失敗:{} 跳過:{}) 速度:{:.2}/s ETA:{:.0}s",
                processed, self.total,
                self.completed.load(Ordering::Relaxed),
                self.failed.load(Ordering::Relaxed),
                self.skipped.load(Ordering::Relaxed),
                speed, eta
            );
        }
    }
}
```

---

## 5. 修改清單

### 檔案變更

| 檔案 | 變更類型 | 說明 |
|------|----------|------|
| `client.rs` | 修改 | 擴充 `StudyMeta`/`SeriesMeta`、新增 `analyze_dicom_for_download()` |
| `main.rs` | 修改 | 重構 `download_accession()` 使用 `DownloadPlan` |
| `processor.rs` | 新增函數 | `ProgressTracker`、`DownloadStatus` enum |
| `config.rs` | 修改 | 新增 `--analyze-url`、`--analyze-username`、`--analyze-password` 給 download 子命令 |

### 新增 CLI 參數 (`DownloadArgs`)

```rust
#[derive(Args, Clone)]
struct DownloadArgs {
    #[command(flatten)]
    shared: SharedArgs,

    /// Directory to write downloaded DICOM files.
    #[arg(long, value_name = "DIR")]
    output: PathBuf,

    /// Retry count per instance (default: 3)
    #[arg(long, default_value = "3")]
    retry_count: usize,

    /// Timeout per instance in seconds (default: 60)
    #[arg(long, default_value = "60")]
    timeout: u64,
}
```

---

## 6. 實作優先序

遵循 Linus「從小開始」原則：

1. **先做最核心的**：目錄命名邏輯（Phase 1-2）
2. **再加 Analyze 整合**：讓命名更精準（Phase 3）
3. **最後加重試與進度**：提升使用體驗（Phase 4-5）

每個 Phase 完成後都應該可以正常運作，只是功能逐步完善。

---

## 7. 驗證計畫

### 測試案例

1. **單一 Accession 下載**
   - 輸入：`--accession TEST001`
   - 預期：產生 `{patient_id}_{study_date}_{modality}_{accession}/` 目錄結構

2. **DWI 衝突處理**
   - 輸入：含有多個 DWI0 series 的 Study
   - 預期：產生 `DWI0_001/`、`DWI0_002/` 等子目錄

3. **重試機制**
   - 模擬：間歇性網路失敗
   - 預期：3 次重試後成功或最終標記 failed

4. **進度輸出**
   - 輸入：大量檔案下載
   - 預期：每 10 筆輸出一次進度、速度、ETA

### 對比驗證

```bash
# Python 版本
python scripts/download_dicom_async.py --accession TEST001 --output /tmp/py_out

# Rust 版本
dicom_download_cli download --input test.csv --output /tmp/rs_out

# 比較目錄結構
diff -r /tmp/py_out /tmp/rs_out
```

---

## 8. 最終決定

1. **Analyze API 整合**：✅ 整合 Analyze API
   - 新增 `--analyze-url`、`--analyze-username`、`--analyze-password` 參數
   - 若未提供 analyze-url，fallback 使用 SeriesDescription

2. **DICOM 標籤來源**：✅ 下載後解析 DICOM bytes
   - 新增 `dicom` crate 依賴
   - 與 Python 版本使用 pydicom 的方式一致

3. **進度輸出格式**：✅ 使用 `indicatif` 進度條
   - 與 remote 子命令風格一致
   - 每個 series 一條進度條，顯示 ETA

---

## 參考資料

- Python 原始碼：`scripts/download_dicom_async.py`
- 現有 Rust 實作：`dicom_download_cli/src/main.rs`
- Linus 哲學指引：`.cursor/rules/linus-torvalds.mdc`

---

*分析版本：1.0*
*核心精神：資料結構優先、消除特殊情況、務實漸進*
