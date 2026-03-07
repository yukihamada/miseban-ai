#!/usr/bin/env python3
"""
Tapo C220 動作確認スクリプト
Usage:
  python3 scripts/test_camera.py --ip 192.168.1.xxx --user admin --pass yourpass --token YOUR_API_TOKEN
"""
import argparse, base64, sys, time, json, ssl
import urllib.request, urllib.error
import cv2

_ssl_ctx = ssl._create_unverified_context()

API_BASE = "https://api.misebanai.com"
CAMERA_ID = "tapo-c220-test"
INTERVAL_SEC = 5   # フレーム送信間隔


def capture_frame(rtsp_url: str) -> bytes | None:
    cap = cv2.VideoCapture(rtsp_url)
    cap.set(cv2.CAP_PROP_BUFFERSIZE, 1)
    ok, frame = cap.read()
    cap.release()
    if not ok:
        return None
    _, buf = cv2.imencode(".jpg", frame, [cv2.IMWRITE_JPEG_QUALITY, 80])
    return buf.tobytes()


def send_frame(jpeg: bytes, token: str) -> dict:
    w, h = 640, 480
    body = json.dumps({
        "camera_id": CAMERA_ID,
        "timestamp": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "jpeg_bytes": base64.b64encode(jpeg).decode(),
        "resolution": {"width": w, "height": h},
    }).encode()
    req = urllib.request.Request(
        f"{API_BASE}/api/v1/frames",
        data=body,
        headers={"Content-Type": "application/json", "Authorization": f"Bearer {token}"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=15, context=_ssl_ctx) as resp:
            return json.loads(resp.read())
    except urllib.error.HTTPError as e:
        return {"error": e.code, "body": e.read().decode()}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ip",    required=True, help="カメラIPアドレス (例: 192.168.1.100)")
    ap.add_argument("--user",  default="admin")
    ap.add_argument("--pass",  dest="password", required=True, help="Tapoアカウントのパスワード")
    ap.add_argument("--token", required=True, help="ミセバンAIのAPIトークン")
    ap.add_argument("--count", type=int, default=5, help="送信フレーム数 (default: 5)")
    ap.add_argument("--stream", default="stream1", help="stream1(高画質) or stream2(低画質)")
    ap.add_argument("--local",  action="store_true", help="ローカルAPIサーバーに送信 (http://localhost:3000)")
    args = ap.parse_args()

    global API_BASE
    if args.local:
        API_BASE = "http://localhost:3000"

    rtsp = f"rtsp://{args.user}:{args.password}@{args.ip}/{args.stream}"
    print(f"RTSP: rtsp://{args.user}:***@{args.ip}/{args.stream}")
    print(f"API:  {API_BASE}")
    print(f"送信: {args.count}フレーム × {INTERVAL_SEC}秒間隔\n")

    # 接続テスト
    print("[1/3] RTSP接続確認...")
    cap = cv2.VideoCapture(rtsp)
    if not cap.isOpened():
        print("ERROR: RTSPに接続できません。IPアドレス/ユーザー名/パスワードを確認してください。")
        print("Tapoアプリ → カメラ設定 → 詳細設定 → カメラアカウント でRTSP有効化が必要です。")
        sys.exit(1)
    cap.release()
    print("OK\n")

    print("[2/3] APIトークン確認...")
    req = urllib.request.Request(
        f"{API_BASE}/api/v1/stores/me/stats",
        headers={"Authorization": f"Bearer {args.token}"},
    )
    try:
        with urllib.request.urlopen(req, timeout=10, context=_ssl_ctx) as r:
            store = json.loads(r.read())
            store_id = store.get('store_id', store.get('id', '?'))
            print(f"OK: store_id={store_id}\n")
    except Exception as e:
        print(f"ERROR: APIトークンが無効です ({e})")
        sys.exit(1)

    print("[3/3] フレーム送信テスト...")
    for i in range(args.count):
        print(f"  フレーム {i+1}/{args.count} 取得中...", end=" ", flush=True)
        jpeg = capture_frame(rtsp)
        if jpeg is None:
            print("ERROR: フレーム取得失敗")
            continue
        print(f"{len(jpeg):,} bytes → API送信...", end=" ", flush=True)

        result = send_frame(jpeg, args.token)
        if "error" in result:
            print(f"ERROR {result['error']}: {result.get('body','')}")
        else:
            pc = result.get("people_count", "?")
            dwell = result.get("avg_dwell_secs", 0)
            uniq = result.get("unique_visitors", 0)
            demo = result.get("demographics", [])
            genders = [f"{d.get('gender','?')}({d.get('age_group','?')})" for d in demo[:3]]
            print(f"OK | 人数: {pc}人 | 滞在: {dwell:.0f}s | 累計: {uniq}人 | {', '.join(genders) or '-'}")

        if i < args.count - 1:
            time.sleep(INTERVAL_SEC)

    print("\n完了。ダッシュボードで確認: https://misebanai.com/dashboard")


if __name__ == "__main__":
    main()
