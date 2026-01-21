# dicom_download 設計規範

## 目標
- 以 Rust 實作跨平台 CLI（Windows/Linux/macOS）。
- 核心邏輯為 pure function，IO 與副作用集中於命令式外殼。
- 支援 CSV/JSON 批次輸入，依 AccessionNumber 自動下載 DICOM。
- 下載可設定併發數量（concurrency），記錄成功/失敗並輸出報告。

## 依據
- 以 `dicom_download.py` 的現有流程與行為為準。
- Analyze API 回傳為 Array，取第 1 筆的 `series_type`。

## 架構原則（Functional Core, Imperative Shell）
- Pure Core：
  - 決策流程、過濾規則、狀態彙整、報告產生。
  - 不直接存取網路、檔案或時間。
- IO Shell：
  - HTTP 呼叫（Orthanc/Analyze）。
  - 讀取 CSV/JSON、寫入報告檔、輸出 Terminal。
  - 併發執行與重試策略。

## CLI 介面（規格）
### 子命令
- `dicom_download_cli remote ...`：C-MOVE 流程（對應舊 `dicom_download.py`），推送到目標 AET。
- `dicom_download_cli download ...`：直接拉檔寫本機（對應 `download_dicom_matt_async.py`），需指定輸出目錄。

### 共同參數
- `-i, --input`：CSV/JSON 路徑（支援報表 CSV，再用 `AccessionNumber/acc/accession` 欄位取值）。
- `--url`：Orthanc Base URL，預設 `http://10.103.1.193/orthanc-a`
- `--username` / `--password`：Orthanc 認證（選填）
- `--concurrency`：同時處理的 accession/實例併發，預設 `5`
- `--report-csv` / `--report-json`：輸出報告路徑，預設 `report.csv` / `report.json`
- `--config`：TOML 供預設值覆寫。

### remote 專屬參數
- `--analyze-url`：Analyze API URL，預設 `http://10.103.1.193:8000/api/v1/series/dicom/analyze/by-upload`
- `--modality`：來源 Modality 名稱，預設 `INFINTT-SERVER`
- `--target`：目的 AET，預設 `ORTHANC`

### download 專屬參數
- `--output <DIR>`：必填，下載檔案的根資料夾。

## 輸入格式
### CSV
- 具或不具 header 均可。
- 優先尋找欄位（不分大小寫）：`AccessionNumber` / `accession` / `acc`。
- 若未找到上述欄位，退回使用第 1 欄。

### JSON（兩種格式）
1. 字串陣列：
   - `[
       "A0001",
       "A0002"
     ]`
2. 物件陣列：
   - `[
       {"AccessionNumber": "A0001"},
       {"AccessionNumber": "A0002"}
     ]`

## 輸出報告
- **Terminal**：即時顯示進度與結果摘要。
- **CSV 檔案**：記錄每筆 Accession 的最終結果。

### CSV 欄位（建議）
- `AccessionNumber`
- `Status`：`Success` / `Failure` / `Skipped`
- `Reason`：失敗或略過原因摘要
- `DownloadedSeriesCount`
- `MatchedSeriesCount`
- `FailedSeriesCount`
- `Timestamp`（由 IO Shell 注入）

## 核心流程（依 dicom_download.py）
針對每一筆 Accession：
1. 查詢 Study：
   - 以 `AccessionNumber` 做查詢，取得 `StudyInstanceUID`。
2. 取得本地已存在 Series UID：
   - 若 series 已存在於本地 Orthanc，則略過該 series。
3. 查詢遠端所有 Series（同 Study）：
4. 逐一處理 series：
   - 若 `SeriesDescription` 符合 **直接下載關鍵字** -> 直接下載。
   - 否則抽樣下載 1 筆 Instance：
     - 將樣本送至 Analyze API。
     - 若 `series_type` 在 **白名單** -> 下載整個 series。
     - 否則跳過。
   - 抽樣完成後刪除樣本 Instance（清理本地）。
5. 異步下載採用 Orthanc Move Job，成功後監控狀態。

## 直接下載關鍵字（DIRECT_DOWNLOAD_KEYWORDS）
- `MRA_BRAIN`

## Series 白名單（SERIES_WHITELIST）
- `ADC`, `DWI`, `DWI0`, `DWI1000`, `SWAN`
- `MRA_BRAIN`, `T1FLAIR_AXI`, `T1BRAVO_AXI`, `T2FLAIR_AXI`
- `ASLSEQ`, `ASLSEQATT`, `ASLSEQATT_COLOR`, `ASLSEQCBF`, `ASLSEQCBF_COLOR`
- `ASLSEQPW`, `ASLPROD`, `ASLPRODCBF`, `ASLPRODCBF_COLOR`
- `DSC`, `DSCCBF_COLOR`, `DSCCBV_COLOR`, `DSCMTT_COLOR`

## Analyze API 規格
- Request：`multipart/form-data`，欄位 `dicom_file_list`。
- Response：Array，取第 1 筆的 `series_type`。

## 併發與工作佇列
- `concurrency` 代表同時處理的 Accession 數量。
- 下載任務以 `buffer_unordered(concurrency)` 控制併發。

## 錯誤與狀態定義（建議）
- `StudyNotFound`：查無 Accession。
- `QueryFailed`：Orthanc Query 失敗或認證失敗。
- `AnalyzeFailed`：Analyze API 失敗或無法解析回應。
- `MoveFailed`：Orthanc Move 失敗。
- `JobFailed`：下載 Job 失敗。
- `SampleNotFound`：樣本 Instance 下載後無法定位。

## 純函數核心建議模型
- `parse_inputs(...) -> Vec<AccessionInput>`
- `decide_series_actions(...) -> Vec<SeriesAction>`
- `merge_results(...) -> Report`
- `render_report_csv(...) -> String`

## IO Shell 建議職責
- 讀寫檔案、HTTP 呼叫、併發管理、時間戳注入、Terminal 輸出。
