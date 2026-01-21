"""
@author: sean Ho
Improved version with progress tracking and better error handling
Compatible with Python 3.7+
"""
import argparse
import csv
import json
import pathlib
import traceback
import asyncio
from dataclasses import dataclass
from typing import Optional
from io import BytesIO
import aiofiles
from pyorthanc import AsyncOrthanc
import pydicom
import httpx
import time



# 調整 semaphore 數量,避免過多並發 (稍後會依參數重設)
semaphore = asyncio.Semaphore(16)  # 從 32 降到 16

ACCESSION_KEYS = {"accessionnumber", "accession", "acc"}
INVALID_PATH_CHARS = set('<>:"/\\|?*')


def sanitize_segment(text: str) -> str:
    cleaned = "".join("_" if c in INVALID_PATH_CHARS else c for c in text.strip())
    return cleaned or "unknown"


def normalize_key(name: str) -> str:
    return name.strip().lower().replace("_", "")


def deduplicate_preserve_order(values: list[str]) -> list[str]:
    seen = set()
    ordered = []
    for v in values:
        if v in seen:
            continue
        seen.add(v)
        ordered.append(v)
    return ordered


def extract_accession_from_mapping(row: dict) -> Optional[str]:
    for key, value in row.items():
        if normalize_key(key) in ACCESSION_KEYS and isinstance(value, str):
            candidate = value.strip()
            if candidate:
                return candidate
    return None


def parse_accessions_from_csv(path: pathlib.Path) -> list[str]:
    if not path.exists():
        raise FileNotFoundError(f"找不到檔案: {path}")

    with path.open("r", encoding="utf-8-sig", newline="") as f:
        reader = list(csv.reader(f))

    if not reader:
        return []

    header = [normalize_key(col) for col in reader[0]]
    has_header = any(col in ACCESSION_KEYS for col in header)
    start_idx = 1 if has_header else 0

    col_idx = 0
    if has_header:
        for idx, col in enumerate(header):
            if col in ACCESSION_KEYS:
                col_idx = idx
                break

    accessions: list[str] = []
    for row in reader[start_idx:]:
        if col_idx < len(row):
            value = row[col_idx].strip()
            if value:
                accessions.append(value)
    return deduplicate_preserve_order(accessions)


def parse_accessions_from_json(path: pathlib.Path) -> list[str]:
    if not path.exists():
        raise FileNotFoundError(f"找不到檔案: {path}")

    with path.open("r", encoding="utf-8") as f:
        data = json.load(f)

    accessions: list[str] = []
    if isinstance(data, list):
        for item in data:
            if isinstance(item, str):
                value = item.strip()
                if value:
                    accessions.append(value)
            elif isinstance(item, dict):
                acc = extract_accession_from_mapping(item)
                if acc:
                    accessions.append(acc)
    else:
        raise ValueError("JSON 格式需為字串陣列或物件陣列")

    return deduplicate_preserve_order(accessions)


def load_accessions(accession_args: list[str], input_path: Optional[str]) -> list[str]:
    collected: list[str] = []
    if accession_args:
        collected.extend([a.strip() for a in accession_args if a.strip()])

    if input_path:
        source = pathlib.Path(input_path)
        suffix = source.suffix.lower()
        if suffix == ".csv":
            collected.extend(parse_accessions_from_csv(source))
        elif suffix == ".json":
            collected.extend(parse_accessions_from_json(source))
        else:
            raise ValueError("僅支援 CSV 或 JSON")

    return deduplicate_preserve_order([a for a in collected if a])


def get_study_folder_name_from_bytes(dicom_bytes: bytes) -> Optional[str]:
    """
    從 DICOM bytes 產生標準化的研究資料夾名稱
    
    Returns:
        標準化的研究資料夾名稱，格式為 `{patient_id}_{study_date}_{modality}_{accession_number}`
        如果 Study Date 標籤缺失，則返回 None
    """
    try:
        dicom_ds = pydicom.dcmread(BytesIO(dicom_bytes), stop_before_pixels=True)
        
        modality = str(dicom_ds[0x08, 0x60].value).strip()
        patient_id = str(dicom_ds[0x10, 0x20].value).strip()
        accession_number = str(dicom_ds[0x08, 0x50].value).strip()
        study_date = dicom_ds.get((0x08, 0x20), None)
        
        if study_date is None:
            return None
        else:
            study_date = str(study_date.value).strip()
        
        return f'{patient_id}_{study_date}_{modality}_{accession_number}'
    except Exception as e:
        print(f"解析 DICOM 標籤失敗: {e}")
        return None


async def analyze_single_instance(
    async_client: AsyncOrthanc,
    instance_uid: str,
    analyze_url: Optional[str],
    username: Optional[str],
    password: Optional[str]
) -> tuple[Optional[str], Optional[bytes]]:
    """
    下載單一 instance 並送至 Analyze API
    
    Returns:
        (series_type, dicom_bytes) - series_type 為分析結果，dicom_bytes 為 DICOM 檔案內容
        若 analyze_url 為 None 或分析失敗，series_type 為 None
    """
    try:
        # 下載 instance
        dicom_bytes = await async_client.get_instances_id_file(instance_uid)
        
        # 若未提供 analyze_url，直接返回
        if not analyze_url:
            return None, dicom_bytes
        
        # 呼叫 Analyze API
        try:
            files = {'dicom_file_list': ('IM0', dicom_bytes, 'application/dicom')}
            auth = httpx.BasicAuth(username, password) if username and password else None
            
            async with httpx.AsyncClient(auth=auth, timeout=30.0) as client:
                resp = await client.post(analyze_url, files=files)
                
                if resp.status_code == 200:
                    result = resp.json()
                    if result and len(result) > 0:
                        series_type = result[0].get("series_type")
                        # 若 series_type 為 "unknown" 或空字串，視為 None
                        if series_type and series_type.lower() != "unknown":
                            return series_type, dicom_bytes
        except Exception as e:
            print(f"Analyze API 呼叫失敗: {e}")
        
        return None, dicom_bytes
        
    except Exception as e:
        print(f"下載 instance {instance_uid} 失敗: {e}")
        return None, None

@dataclass
class DownloadDicom:
    async_client: AsyncOrthanc
    output_directory: pathlib.Path
    study_folder: str
    series_folder: str
    instances_uid: str

class ProgressTracker:
    """進度追蹤器"""
    def __init__(self, total: int):
        self.total = total
        self.completed = 0
        self.failed = 0
        self.skipped = 0
        self.lock = asyncio.Lock()
        self.start_time = time.time()

    async def increment(self, status: str):
        async with self.lock:
            if status == "completed":
                self.completed += 1
            elif status == "failed":
                self.failed += 1
            elif status == "skipped":
                self.skipped += 1

            processed = self.completed + self.failed + self.skipped
            elapsed = time.time() - self.start_time

            if processed % 10 == 0 or processed == self.total:
                speed = processed / elapsed if elapsed > 0 else 0
                eta = (self.total - processed) / speed if speed > 0 else 0

                print(f"\n進度: {processed}/{self.total} "
                      f"(完成:{self.completed} 失敗:{self.failed} 跳過:{self.skipped}) "
                      f"速度:{speed:.2f}/s ETA:{eta:.0f}s")

async def write_file(file_path: pathlib.Path, content: bytes) -> bool:
    """寫入檔案"""
    try:
        async with semaphore:
            async with aiofiles.open(file_path, "wb") as f:
                await f.write(content)
        return True
    except Exception as e:
        print(f"寫入檔案失敗 {file_path}: {str(e)}")
        return False

async def download_single_instance(
    async_client: AsyncOrthanc,
    instances_uid: str,
    output_path: pathlib.Path
) -> bool:
    """下載單個 instance (帶 timeout)"""
    async with semaphore:
        instances_response = await async_client.get_instances_id_file(instances_uid)

    # 寫入檔案
    return await write_file(output_path, instances_response)

async def download_dicom(
    download_dicom_data: DownloadDicom,
    progress: ProgressTracker,
    retry_count: int = 3,
    timeout_seconds: float = 60.0
) -> tuple[bool, str]:
    """
    下載 DICOM 檔案

    Returns:
        (success, status) - status 可以是 "completed", "failed", "skipped"
    """
    output_path = download_dicom_data.output_directory.joinpath(
        sanitize_segment(download_dicom_data.study_folder),
        sanitize_segment(download_dicom_data.series_folder),
        f"{download_dicom_data.instances_uid}.dcm"
    )

    # 如果檔案已存在,跳過
    if output_path.exists():
        await progress.increment("skipped")
        return True, "skipped"

    # 確保輸出目錄存在
    output_path.parent.mkdir(parents=True, exist_ok=True)

    async_client = download_dicom_data.async_client

    # 重試機制
    for attempt in range(retry_count):
        try:
            # 使用 wait_for 實現 timeout (兼容 Python 3.7+)
            success = await asyncio.wait_for(
                download_single_instance(
                    async_client,
                    download_dicom_data.instances_uid,
                    output_path
                ),
                timeout=timeout_seconds
            )

            if success:
                await progress.increment("completed")
                return True, "completed"
            else:
                if attempt < retry_count - 1:
                    await asyncio.sleep(1 * (attempt + 1))  # 遞增延遲
                    continue
                else:
                    await progress.increment("failed")
                    return False, "failed"

        except asyncio.TimeoutError:
            print(f"下載超時 (嘗試 {attempt + 1}/{retry_count}): "
                  f"{download_dicom_data.instances_uid}")
            if attempt < retry_count - 1:
                await asyncio.sleep(2 * (attempt + 1))
                continue
            else:
                await progress.increment("failed")
                return False, "failed"

        except Exception as e:
            print(f"下載錯誤 (嘗試 {attempt + 1}/{retry_count}): "
                  f"{download_dicom_data.instances_uid}: {str(e)}")
            traceback.print_exc()

            if attempt < retry_count - 1:
                await asyncio.sleep(2 * (attempt + 1))
                continue
            else:
                await progress.increment("failed")
                return False, "failed"

    await progress.increment("failed")
    return False, "failed"

async def download_series(
    async_client: AsyncOrthanc,
    output_directory: pathlib.Path,
    study_folder: str,
    series_folder: str,
    progress: ProgressTracker,
    instances_uid_list: list[str]
) -> list[tuple[bool, str]]:
    """下載一個 series 的所有 instances"""
    try:
        if not instances_uid_list:
            print(f"Series {series_folder} 無實例可下載，跳過")
            return []

        print(f"\n下載 Series {series_folder}: {len(instances_uid_list)} 個檔案")

        tasks = []
        for instances_uid in instances_uid_list:
            download_dicom_data = DownloadDicom(
                async_client=async_client,
                output_directory=output_directory,
                study_folder=study_folder,
                series_folder=series_folder,
                instances_uid=instances_uid
            )
            task = asyncio.create_task(
                download_dicom(download_dicom_data, progress)
            )
            tasks.append(task)

        # 使用 gather 並處理異常
        results = await asyncio.gather(*tasks, return_exceptions=True)

        # 處理異常結果
        processed_results = []
        for result in results:
            if isinstance(result, Exception):
                print(f"Task 異常: {result}")
                processed_results.append((False, "failed"))
            else:
                processed_results.append(result)

        return processed_results

    except Exception as e:
        print(f"Series {series_folder} 處理失敗: {str(e)}")
        traceback.print_exc()
        return []


async def find_studies_by_accession(
    async_client: AsyncOrthanc,
    accession: str
) -> list[str]:
    payload = {
        "Level": "Study",
        "Query": {"AccessionNumber": accession},
    }
    try:
        result = await async_client.post_tools_find(json=payload)
        if isinstance(result, list):
            return result
    except Exception as e:
        print(f"查詢 Accession {accession} 失敗: {e}")
    return []


async def build_download_plan(
    async_client: AsyncOrthanc,
    accessions: list[str],
    analyze_url: Optional[str],
    analyze_username: Optional[str],
    analyze_password: Optional[str]
) -> tuple[list[dict], int]:
    """
    建立下載計畫，包含 analyze 與目錄命名
    
    Returns:
        (plan_list, total_files)
        plan_list: [
            {
                "study_folder": "patient_date_modality_accession",
                "series": [
                    {
                        "series_folder": "ADC" or "DWI0_003",
                        "instances": ["inst1", "inst2", ...]
                    },
                    ...
                ]
            },
            ...
        ]
    """
    plan: list[dict] = []
    total_files = 0

    for accession in accessions:
        study_ids = await find_studies_by_accession(async_client, accession)
        if not study_ids:
            print(f"找不到 AccessionNumber: {accession}")
            continue

        for study_uid in study_ids:
            try:
                series_list = await async_client.get_studies_id_series(study_uid)
            except Exception as e:
                print(f"取得 Study {study_uid} 的 series 失敗: {e}")
                continue

            # 收集所有 series 資訊
            series_info = []
            study_folder_name = None
            
            for series_data in series_list:
                try:
                    if isinstance(series_data, dict):
                        series_uid = series_data.get("ID")
                        instances = series_data.get("Instances", [])
                        main_dicom_tags = series_data.get("MainDicomTags", {})
                        series_description = main_dicom_tags.get("SeriesDescription", "Unknown")
                        series_number = main_dicom_tags.get("SeriesNumber", "")
                    else:
                        series_uid = str(series_data)
                        series = await async_client.get_series_id(series_uid)
                        instances = series.get("Instances", [])
                        main_dicom_tags = series.get("MainDicomTags", {})
                        series_description = main_dicom_tags.get("SeriesDescription", "Unknown")
                        series_number = main_dicom_tags.get("SeriesNumber", "")

                    if not instances:
                        print(f"Series {series_uid} 無實例，跳過")
                        continue
                    
                    # 取第一個 instance 進行 analyze
                    first_instance = instances[0]
                    series_type, dicom_bytes = await analyze_single_instance(
                        async_client, 
                        first_instance,
                        analyze_url,
                        analyze_username,
                        analyze_password
                    )
                    
                    # 若 analyze 失敗，使用 SeriesDescription
                    if series_type is None:
                        series_type = series_description
                    
                    # 若這是第一個 series，從 DICOM 標籤取得 study folder 名稱
                    if study_folder_name is None and dicom_bytes:
                        study_folder_name = get_study_folder_name_from_bytes(dicom_bytes)
                        if study_folder_name is None:
                            # Fallback: 使用 accession + study_uid
                            study_folder_name = f"{accession}_{study_uid[:8]}"
                    
                    series_info.append({
                        "series_uid": series_uid,
                        "series_type": series_type,
                        "series_number": series_number,
                        "instances": instances
                    })
                    
                except Exception as e:
                    sid = series_data.get("ID") if isinstance(series_data, dict) else str(series_data)
                    print(f"處理 Series {sid} 失敗: {e}")
                    traceback.print_exc()
                    continue
            
            # 處理 DWI 命名衝突
            series_type_count = {}
            for info in series_info:
                st = info["series_type"]
                series_type_count[st] = series_type_count.get(st, 0) + 1
            
            # 為 series 產生最終的 series_folder 名稱
            series_folders = []
            series_type_index = {}
            
            for info in series_info:
                series_type = info["series_type"]
                series_number = info["series_number"]
                
                # 若該 series_type 有多個（特別是 DWI0/DWI1000），加上 SeriesNumber
                if series_type_count[series_type] > 1 and series_type in ("DWI0", "DWI1000"):
                    series_folder = f"{series_type}_{series_number.zfill(3)}"
                else:
                    series_folder = series_type
                
                series_folders.append({
                    "series_folder": series_folder,
                    "instances": info["instances"]
                })
                
                total_files += len(info["instances"])
            
            plan.append({
                "study_folder": study_folder_name or f"{accession}_unknown",
                "series": series_folders
            })

    return plan, total_files

def parse_args():
    parser = argparse.ArgumentParser(description="依 AccessionNumber 下載 Orthanc DICOM")
    parser.add_argument(
        "--url",
        default="http://10.103.51.1:8042",
        help="Orthanc Base URL，例如 http://host:port",
    )
    parser.add_argument("--username", default=None, help="Orthanc 使用者名稱")
    parser.add_argument("--password", default=None, help="Orthanc 密碼")
    parser.add_argument(
        "--output",
        default="./dicom_downloads",
        help="下載輸出目錄，預設 ./dicom_downloads",
    )
    parser.add_argument(
        "--concurrency",
        type=int,
        default=16,
        help="同時下載/寫檔的最大併發數，預設 16",
    )
    parser.add_argument(
        "--accession",
        action="append",
        dest="accessions",
        help="單筆 AccessionNumber，可重複指定多筆",
    )
    parser.add_argument(
        "--input",
        help="CSV 或 JSON 檔案路徑，內含 AccessionNumber 清單",
    )
    parser.add_argument(
        "--analyze-url",
        default=None,
        help="Analyze API URL，若未指定則使用 SeriesDescription",
    )
    parser.add_argument(
        "--analyze-username",
        default=None,
        help="Analyze API 使用者名稱",
    )
    parser.add_argument(
        "--analyze-password",
        default=None,
        help="Analyze API 密碼",
    )
    return parser.parse_args()


async def main():
    args = parse_args()

    accessions = load_accessions(args.accessions or [], args.input)
    if not accessions:
        print("請提供至少一筆 AccessionNumber (--accession 或 --input)")
        return

    output_directory = pathlib.Path(args.output).expanduser().resolve()
    output_directory.mkdir(parents=True, exist_ok=True)

    global semaphore
    semaphore = asyncio.Semaphore(max(args.concurrency, 1))

    print("=== 開始下載 DICOM 檔案 ===\n")
    print(f"目標 Orthanc: {args.url}")
    print(f"輸出路徑: {output_directory}")
    print(f"併發數: {args.concurrency}")
    print(f"Analyze URL: {args.analyze_url or '未啟用'}")
    print(f"Accession 清單: {', '.join(accessions)}\n")

    async_client = AsyncOrthanc(
        args.url,
        username=args.username,
        password=args.password,
        timeout=60.0
    )

    try:
        plan, total_files = await build_download_plan(
            async_client, 
            accessions,
            args.analyze_url,
            args.analyze_username,
            args.analyze_password
        )
        
        if total_files == 0:
            print("查無可下載的實例，結束")
            return

        progress = ProgressTracker(total_files)

        for study_plan in plan:
            study_folder = study_plan["study_folder"]
            
            print(f"\n{'=' * 60}")
            print(f"開始處理 Study: {study_folder}")
            print(f"{'=' * 60}")

            for series_item in study_plan["series"]:
                await download_series(
                    async_client,
                    output_directory,
                    study_folder,
                    series_item["series_folder"],
                    progress,
                    series_item["instances"]
                )

        print(f"\n{'='*60}")
        print("下載完成!")
        print(f"{'='*60}")
        print(f"總檔案數: {total_files}")
        print(f"成功: {progress.completed}")
        print(f"跳過: {progress.skipped}")
        print(f"失敗: {progress.failed}")
        print(f"總耗時: {time.time() - progress.start_time:.2f} 秒")

    except Exception as e:
        print(f"\n主程式錯誤: {str(e)}")
        traceback.print_exc()

    finally:
        try:
            await async_client.close()
        except Exception:
            pass


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        print("\n\n使用者中斷程式")
    except Exception as e:
        print(f"\n\n程式異常結束: {str(e)}")
        traceback.print_exc()