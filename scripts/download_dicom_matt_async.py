"""
@author: sean Ho
Improved version with progress tracking and better error handling
Compatible with Python 3.7+
"""
import pathlib
import traceback
import asyncio
from dataclasses import dataclass
from typing import Optional
import aiofiles
from pyorthanc import AsyncOrthanc
from code_ai import load_dotenv
import time

load_dotenv()

# 調整 semaphore 數量,避免過多並發
semaphore = asyncio.Semaphore(16)  # 從 32 降到 16

@dataclass
class DownloadDicom:
    async_client: AsyncOrthanc
    output_directory: pathlib.Path
    study_uid: str
    series_uid: str
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
        download_dicom_data.study_uid,
        download_dicom_data.series_uid,
        f'{download_dicom_data.instances_uid}.dcm'
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
    study_uid: str,
    series_uid: str,
    progress: ProgressTracker
) -> list[tuple[bool, str]]:
    """下載一個 series 的所有 instances"""
    try:
        series = await async_client.get_series_id(series_uid)
        instances_uid_list = series['Instances']

        print(f"\n下載 Series {series_uid}: {len(instances_uid_list)} 個檔案")

        tasks = []
        for instances_uid in instances_uid_list:
            download_dicom_data = DownloadDicom(
                async_client=async_client,
                output_directory=output_directory,
                study_uid=study_uid,
                series_uid=series_uid,
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
        print(f"Series {series_uid} 處理失敗: {str(e)}")
        traceback.print_exc()
        return []

async def main():
    print("=== 開始下載 DICOM 檔案 ===\n")

    # 設定
    UPLOAD_DATA_DICOM_SEG_URL = "http://10.103.51.1:8042"
    output_directory = pathlib.Path("/home/david/ai-inference-dicom-file")

    # 創建客戶端
    async_client = AsyncOrthanc(
        UPLOAD_DATA_DICOM_SEG_URL,
        username="radaxaiAdmin",
        password="radaxaiAdmin666",
        timeout=60.0  # 降低 timeout
    )

    study_uid_list = [
        "2f7f4996-7143fcff-47d700ec-a5b31fac-fb0b65be",
    ]

    series_uid_list = [
        # T1
        ("38dc9f10-b62b878f-762b3d23-9b7fe801-f78744b5", "T1"),
        # T2 FLAIR
        # ("a4e02ca3-f65e3730-4666e4da-992694b1-5a349090", "T2 FLAIR"),
        # SWAN
        ("639bcc76-d9215eea-2719c014-7be36d31-a4c4eb78", "SWAN"),
        # DWI
        # ("063e719-26189a06-6e7e1453-6f5fe3ef-972620f6", "DWI"),
        # # ADC
        # ("38acec51-987b6c78-9dce8ee1-0f1379e3-57d61fe7", "ADC")
        ("eae888be-bedbb4e9-1d0c5185-2ad1709d-1e1f3212","MRA_BRAIN")
    ]

    try:
        # 計算總檔案數
        total_files = 0
        series_info = []

        for study_uid in study_uid_list:
            for series_uid, series_name in series_uid_list:
                try:
                    series = await async_client.get_series_id(series_uid)
                    instances_count = len(series['Instances'])
                    total_files += instances_count
                    series_info.append((study_uid, series_uid, series_name, instances_count))
                    print(f"{series_name}: {instances_count} 個檔案")
                except Exception as e:
                    print(f"取得 Series {series_name} 資訊失敗: {str(e)}")

        print(f"\n總共需要下載: {total_files} 個檔案\n")

        # 創建進度追蹤器
        progress = ProgressTracker(total_files)

        # 逐個下載 series
        all_results = []
        for study_uid, series_uid, series_name, instances_count in series_info:
            print(f"\n{'='*60}")
            print(f"開始處理: {series_name}")
            print(f"{'='*60}")

            results = await download_series(
                async_client,
                output_directory,
                study_uid,
                series_uid,
                progress
            )
            all_results.extend(results)

        # 輸出最終統計
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
        # 確保關閉連線
        try:
            await async_client.close()
        except:
            pass

if __name__ == "__main__":
    # 使用 asyncio.run 運行
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        print("\n\n使用者中斷程式")
    except Exception as e:
        print(f"\n\n程式異常結束: {str(e)}")
        traceback.print_exc()

"""
# CMB
{
  "requests": [
    {
      "study_uid": "58e16e38-2e7f9a4a-9c26a415-f55b1725-ffa1288d",
      "series_uids": ["fb9ec858-3790e92e-ddf5d73b-8cfcea20-63d5499f", "2c4bc8a9-2cd5576d-776c485f-180b3212-5c708eb1"],
      "model_id": "48c0cfa2-347b-4d32-aa74-a7b1e20dd2e6"
    }
  ]
}


("2c4bc8a9-2cd5576d-776c485f-180b3212-5c708eb1", "T1"),
# SWAN
("fb9ec858-3790e92e-ddf5d73b-8cfcea20-63d5499f", "SWAN"),

{
  "requests": [
    {
      "study_uid": "ee5f44b1-e1f0dc1c-8825e04b-d5fb7bae-0373ba30",
      "series_uids": ["767b7a86-3609ad08-41f1e8bd-c2ffcd1e-d52c404f"],
      "model_id": "3fa85f64-5717-4562-b3fc-2c963f66afa6"
    },
    {
      "study_uid": "ee5f44b1-e1f0dc1c-8825e04b-d5fb7bae-0373ba30",
      "series_uids": ["308454c5-d2ff7ec1-74ce99a4-4281ba1b-a3a1d20b","86364c14-51867d5e-4ecffab6-36054e99-ad1ff077"],
      "model_id": "97abe75d-34de-4e91-80c2-ce74b6c70438"
    },
    {
      "study_uid": "ee5f44b1-e1f0dc1c-8825e04b-d5fb7bae-0373ba30",
      "series_uids": ["fc340cd5-37fe175d-ed6feb62-bab88fdf-9850e140"],
      "model_id": "7e94d381-3f5d-46b6-b440-e5d44ebc48d2"
    },
    {
      "study_uid": "58e16e38-2e7f9a4a-9c26a415-f55b1725-ffa1288d",
      "series_uids": ["2c4bc8a9-2cd5576d-776c485f-180b3212-5c708eb1", "fb9ec858-3790e92e-ddf5d73b-8cfcea20-63d5499f"],
      "model_id": "48c0cfa2-347b-4d32-aa74-a7b1e20dd2e6"
    }
  ]
}

"""