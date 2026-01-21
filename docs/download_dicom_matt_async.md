# download_dicom_matt_async.py 流程說明

對應檔案：`scripts/download_dicom_matt_async.py`  
用途：直接從 Orthanc 以 HTTP/REST 拉取 Instances，寫入本機資料夾，附帶重試、超時與並發控制。

## 輸入/設定
- `UPLOAD_DATA_DICOM_SEG_URL`：Orthanc 伺服器網址。
- `output_directory`：下載輸出根目錄。
- `study_uid_list`：要處理的 Study ID（Orthanc 內部 UUID）。
- `series_uid_list`：要處理的 Series ID（Orthanc 內部 UUID）與可讀名稱。
- 認證：`username`/`password`（在 `AsyncOrthanc` 初始化時提供）。
- 並發：全域 `semaphore = asyncio.Semaphore(16)` 限制同時下載/寫入。
- 重試/超時：每個 instance 最多重試 3 次，單次超時 60 秒。

## 主要流程
1. **計算總檔案數**：對每個 series 呼叫 `get_series_id(series_uid)` 取得 `Instances` 長度，累加 total。
2. **初始化進度**：`ProgressTracker(total_files)` 以鎖保護已完成/失敗/跳過計數，定期列印速度與 ETA。
3. **逐 series 處理**：
   - 列出該 series 的 `Instances` 清單。
   - 為每個 instance 建立下載任務 `download_dicom`：
     - 若檔案已存在則標記 skipped。
     - 呼叫 `get_instances_id_file(instances_uid)` 取得檔案 bytes。
     - 寫入 `output_directory/<study_uid>/<series_uid>/<instances_uid>.dcm`。
     - 失敗時依重試次數退避重試；超時或例外會計入 failed。
4. **等待所有任務**：`asyncio.gather(..., return_exceptions=True)` 收集結果，將例外轉為 failed。
5. **輸出摘要**：顯示成功/跳過/失敗數與總耗時。

## 錯誤處理與重試
- 單檔案下載使用 `asyncio.wait_for` 套用超時。
- 下載/寫檔失敗會逐次延遲重試（線性退避）。
- 例外被捕捉並記錄，進度計數同步更新。

## 目錄結構與命名
- 下載路徑：`<output_directory>/<study_uid>/<series_uid>/<instance_uid>.dcm`
- `study_uid`/`series_uid`/`instance_uid` 皆為 Orthanc 內部 UUID，而非 DICOM UID。

## 併發策略
- 透過 `asyncio.Semaphore(16)` 控制同時下載/寫檔數量，避免壓爆伺服器。
- Series 內的 instances 以 `create_task` 全部排入，靠 semaphore 節流。

## 與現有 Rust CLI 對應
- 新的 `download` 子命令需提供等效能力：直接拉檔寫本機（非 C-MOVE），並支援輸入為 accession 列表（CSV/JSON/report_csv）。  
- 需新增輸出目錄參數（必填），並保持並發/重試/跳過既有檔案的行為。*** End Patch
