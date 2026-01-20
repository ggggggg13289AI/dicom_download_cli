#!/usr/bin/env python3
"""
Orthanc DICOM Download Script (Final Configurable Version)
修正項目：
1. 修正 400 Missing tag 0020,000d 錯誤。
2. C-MOVE Payload 階層修正。
3. 支援 Username/Password 認證。
4. 修正 Target AET 寫死問題。
5. [New] 修正 Analyze URL 寫死問題 (改為參數)。
6. [New] 將特殊 Series 關鍵字提取為設定。
"""

import requests
import json
import time
import sys
import argparse
from typing import Optional, Dict, Any, List, Set
from urllib3.exceptions import InsecureRequestWarning

# 禁用 SSL 警告
requests.packages.urllib3.disable_warnings(InsecureRequestWarning)

class OrthancClient:
    
    # [Config] 分析後決定要下載的類型 (白名單)
    SERIES_WHITELIST = {
        "ADC", "DWI", "DWI0", "DWI1000", "SWAN", 
        "MRA_BRAIN", "T1FLAIR_AXI", "T1BRAVO_AXI", "T2FLAIR_AXI",
        "ASLSEQ","ASLSEQATT","ASLSEQATT_COLOR","ASLSEQCBF","ASLSEQCBF_COLOR",
        "ASLSEQPW","ASLPROD","ASLPRODCBF","ASLPRODCBF_COLOR","DSC","DSCCBF_COLOR","DSCCBV_COLOR","DSCMTT_COLOR"
    }
    
    # [Config] 不經過分析，直接強制下載的 Series 描述關鍵字
    DIRECT_DOWNLOAD_KEYWORDS = {
        "MRA_BRAIN"
    }

    def __init__(self, base_url: str, analyze_url: str, username: str = None, password: str = None, verify_ssl: bool = False):
        self.base_url = base_url.rstrip('/')
        self.analyze_url = analyze_url # [Fix] 改為從參數讀取
        
        self.session = requests.Session()
        
        # 設定認證 (若有提供帳密)
        if username and password:
            self.session.auth = (username, password)
            
        self.session.verify = verify_ssl
        self.default_timeout = (10.0, 60.0)
    
    def _request(self, method: str, endpoint: str, **kwargs) -> Any:
        url = f"{self.base_url}/{endpoint.lstrip('/')}"
        if 'timeout' not in kwargs: kwargs['timeout'] = self.default_timeout
        
        try:
            if 'files' in kwargs and 'headers' in kwargs and 'Content-Type' in kwargs['headers']:
                del kwargs['headers']['Content-Type']
            
            response = self.session.request(method, url, **kwargs)
            response.raise_for_status()
            return response.json()
        except Exception as e:
            if hasattr(e, 'response') and e.response is not None:
                print(f"   [API Error] Status: {e.response.status_code}")
                try: print(f"   [API Error] Detail: {e.response.json()}")
                except: print(f"   [API Error] Text: {e.response.text}")
            raise

    def get_local_series_uids(self, study_instance_uid: str) -> Set[str]:
        try:
            payload = {"Level": "Study", "Query": {"StudyInstanceUID": study_instance_uid}}
            studies = self._request("POST", "tools/find", json=payload)
            if not studies: return set()
            series_list = self._request("GET", f"studies/{studies[0]}/series")
            local_uids = set()
            for series in series_list:
                if "MainDicomTags" in series:
                    local_uids.add(series["MainDicomTags"].get("SeriesInstanceUID"))
            return local_uids
        except: return set()

    def query_remote_series(self, modality: str, study_uid: str) -> List[Dict]:
        print(f"[Query] 查詢遠端 Series (UID: {study_uid})...")
        payload = {"Level": "Series", "Query": {"StudyInstanceUID": study_uid}, "Normalize": True}
        resp = self._request("POST", f"modalities/{modality}/query", json=payload)
        q_id = resp.get('ID')
        answers = self._request("GET", f"queries/{q_id}/answers")
        series_list = []
        for idx in answers:
            content = self._request("GET", f"queries/{q_id}/answers/{idx}/content")
            series_list.append({
                "SeriesInstanceUID": content.get("0020,000e", {}).get("Value"),
                "SeriesDescription": content.get("0008,103e", {}).get("Value", ""),
            })
        print(f"✓ 遠端共有 {len(series_list)} 個 Series")
        return series_list

    def get_remote_sample_uid(self, modality: str, series_uid: str) -> Optional[str]:
        payload = {"Level": "Instance", "Query": {"SeriesInstanceUID": series_uid}, "Limit": 1}
        resp = self._request("POST", f"modalities/{modality}/query", json=payload)
        q_id = resp.get("ID")
        answers = self._request("GET", f"queries/{q_id}/answers")
        if answers:
            content = self._request("GET", f"queries/{q_id}/answers/{answers[0]}/content")
            return content.get("0008,0018", {}).get("Value")
        return None
        
    def find_local_uuid(self, sop_uid: str) -> Optional[str]:
        try:
            res = self._request("POST", "tools/find", json={"Level": "Instance", "Query": {"SOPInstanceUID": sop_uid}})
            return res[0] if res else None
        except: return None

    def analyze_dicom(self, dicom_bytes: bytes) -> Optional[str]:
        try:
            files = {'dicom_file_list': ('IM0', dicom_bytes, 'application/dicom')}
            # [Fix] 使用動態傳入的 analyze_url
            # 如果 Analyze API 也需要驗證，請改用 self.session.post
            resp = requests.post(self.analyze_url, files=files, timeout=30)
            if resp.status_code == 200 and resp.json():
                return resp.json()[0].get("series_type")
        except Exception as e:
            print(f"   [API Error] Analyze Failed: {e}")
        return None
    
    def monitor_job(self, job_id: str, poll_interval: int = 2, max_attempts: int = 300):
        print(f"   [Job] 監控 Job ID: {job_id} ...")
        attempt = 0
        while attempt < max_attempts:
            try:
                job_info = self._request("GET", f"jobs/{job_id}")
                state = job_info.get('State', 'Unknown')
                progress = job_info.get('Progress', 0)
                print(f"\r   [Job] 進度: {progress}% (狀態: {state})    ", end="", flush=True)
                if state == "Success":
                    print("\n   [Job] ✓ 完成")
                    return True
                elif state == "Failure":
                    print(f"\n   [Job] ✗ 失敗: {job_info.get('ErrorDetails')}")
                    return False
                time.sleep(poll_interval)
                attempt += 1
            except Exception as e:
                return False
        return False

    def move_resource(self, modality: str, level: str, 
                      study_uid: str,          
                      series_uid: str = None,  
                      sop_uid: str = None,     
                      target_aet: str = "ORTHANC", 
                      async_mode: bool = True):
        
        resource_identifier = {
            "StudyInstanceUID": study_uid 
        }
        
        if series_uid:
            resource_identifier["SeriesInstanceUID"] = series_uid 
        
        if sop_uid:
            resource_identifier["SOPInstanceUID"] = sop_uid 

        payload = {
            "Level": level,
            "Resources": [resource_identifier],
            "TargetAet": target_aet,
            "Synchronous": not async_mode
        }
        
        headers = {"Asynchronous": "true"} if async_mode else {}
        req_timeout = (5.0, 15.0) if not async_mode else (10.0, 60.0)

        resp = self._request(
            "POST", 
            f"modalities/{modality}/move", 
            json=payload, 
            headers=headers,
            timeout=req_timeout
        )
        
        if async_mode:
            return resp.get("ID")
        return None

    def process(self, modality, accession, target_aet):
        print(f"--- 開始處理 Accession: {accession} ---")
        
        q_payload = {"Level": "Study", "Query": {"AccessionNumber": accession}}
        try:
            q_resp = self._request("POST", f"modalities/{modality}/query", json=q_payload)
        except Exception as e:
            print(f"✗ 查詢失敗，請檢查連線或帳密。")
            return

        q_id = q_resp.get("ID")
        answers = self._request("GET", f"queries/{q_id}/answers")
        
        if not answers:
            print("✗ 找不到 Accession Number")
            return
        
        content = self._request("GET", f"queries/{q_id}/answers/0/content")
        study_uid = content.get("0020,000d", {}).get("Value")
        
        local_uids = self.get_local_series_uids(study_uid)
        remote_series = self.query_remote_series(modality, study_uid)
        
        for s in remote_series:
            s_uid = s["SeriesInstanceUID"]
            s_desc = s["SeriesDescription"]
            
            print(f"\n[*] Series: {s_desc} ")
            
            if s_uid in local_uids:
                print("   => [Skip] 本地已存在")
                continue
                
            should_download = False
            
            # [Fix] 檢查是否在直接下載清單中 (不再寫死字串)
            if s_desc in self.DIRECT_DOWNLOAD_KEYWORDS:
                print(f"   => [Match] 關鍵字符合 ({s_desc})，強制下載")
                should_download = True
            else:
                print("   => [Sample] 抽樣分析中...")
                sample_sop = self.get_remote_sample_uid(modality, s_uid)
                if sample_sop:
                    try:
                        self.move_resource(
                            modality=modality, 
                            level="Instance", 
                            study_uid=study_uid,     
                            series_uid=s_uid,        
                            sop_uid=sample_sop,      
                            target_aet=target_aet,
                            async_mode=False
                        )
                        
                        local_uuid = self.find_local_uuid(sample_sop)
                        if local_uuid:
                            dicom_data = self.session.get(f"{self.base_url}/instances/{local_uuid}/file", timeout=30).content
                            sType = self.analyze_dicom(dicom_data)
                            print(f"   => [Analyze] 類型: {sType}")
                            if sType in self.SERIES_WHITELIST: should_download = True
                            self._request("DELETE", f"instances/{local_uuid}")
                        else:
                            print("   => [Error] 樣本下載後找不到")
                    except Exception as e:
                        print(f"   => [Error] 抽樣失敗: {e}")
            
            if should_download:
                print(f"   => [Download] 啟動 Job 下載...")
                try:
                    job_id = self.move_resource(
                        modality=modality, 
                        level="Series", 
                        study_uid=study_uid,    
                        series_uid=s_uid,       
                        target_aet=target_aet, 
                        async_mode=True
                    )
                    if job_id: self.monitor_job(job_id)
                except Exception as e:
                    print(f"   => [Error] 下載任務失敗: {e}")
            else:
                print("   => [Pass] 跳過")

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("accession")
    
    # 核心連線參數
    parser.add_argument("--url", default="http://10.103.1.193/orthanc-a", help="Orthanc Base URL")
    # [New] 新增 Analyze URL 參數
    parser.add_argument("--analyze-url", default="http://10.103.1.193:8000/api/v1/series/dicom/analyze/by-upload", help="Analysis API URL")
    
    # DICOM 參數
    parser.add_argument("--modality", default="INFINTT-SERVER", help="Source Modality Name")
    parser.add_argument("--target", default="ORTHANC", help="Destination AET (Yourself)")
    
    # 認證參數
    parser.add_argument("--username", default=None, help="Orthanc username")
    parser.add_argument("--password", default=None, help="Orthanc password")
    
    args = parser.parse_args()
    
    try:
        client = OrthancClient(
            base_url=args.url,
            analyze_url=args.analyze_url, # 傳入
            username=args.username, 
            password=args.password
        )
        client.process(args.modality, args.accession, args.target)
    except KeyboardInterrupt:
        print("\n使用者中斷")
    except Exception as e:
        print(f"\n[Fatal Error] {e}")

if __name__ == "__main__":
    main()

1.我要用rust 重新寫成CLI工具
2.幫我讓整個CLI工具是  pure function 
3.我要可以在 window linux macos 都可以跑
4. 我要一個接收CSV、JOSN，自動執行下載(異步可指定數量)，並記錄成功失敗，最後給出結果的功能。
5. 有問題請反問我，我會協助完成