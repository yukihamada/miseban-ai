<div align="center">

# ミセバンAI

**既存カメラが、AI店長に変わる。**

Your existing camera becomes an AI store manager.

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.86.0-orange.svg)](https://www.rust-lang.org/)
[![Deploy](https://img.shields.io/badge/Fly.io-Deployed-purple.svg)](https://miseban-ai.fly.dev)

[Website](https://miseban-ai.fly.dev) | [Blog](https://miseban-ai.fly.dev/blog/) | [対応カメラ](https://miseban-ai.fly.dev/cameras.html)

</div>

---

## What is MisebanAI? / ミセバンAIとは？

**日本語**: ミセバンAIは、既存の防犯カメラやスマートフォンをAI店長に変えるソリューションです。高価な専用機器を購入する必要はありません。今あるカメラの映像をクラウドAIが分析し、来客数カウント・属性推定・ヒートマップ・防犯アラート・AI経営アドバイスをリアルタイムで提供します。結果はLINEやSlackに通知され、ダッシュボードからいつでも確認できます。

**English**: MisebanAI transforms your existing security cameras and smartphones into an AI-powered store manager. No expensive proprietary hardware required. Cloud AI analyzes your camera feed in real time, providing people counting, demographic estimation, heatmaps, security alerts, and AI-driven business advice. Results are delivered via LINE and Slack notifications, and are always accessible from the dashboard.

---

## Features / 機能一覧

| Feature | Description |
|---------|-------------|
| **People Counting / 来客数カウント** | リアルタイムで入店・退店人数を自動計測。時間帯別の来客傾向を把握 |
| **Demographics / 属性推定** | 年齢層・性別の推定により、顧客層を可視化 |
| **Heatmap / ヒートマップ** | 店舗内の動線を可視化。どこに人が滞留しているかを一目で把握 |
| **Security Alerts / 防犯アラート** | 不審行動や異常を検知し、即座に通知 |
| **AI Advice / AI経営アドバイス** | 蓄積データをもとに、レイアウト改善や人員配置の提案を自動生成 |
| **LINE/Slack Notifications / 通知連携** | 重要なイベントやレポートをLINE・Slackにリアルタイム配信 |

---

## Architecture / アーキテクチャ

```
┌─────────────┐     RTSP/MJPEG     ┌─────────────────┐
│   Camera     │ ──────────────────>│   MisebanAI     │
│  (既存カメラ)  │                    │   Agent         │
└─────────────┘                    │  (PC/RPi/Phone) │
                                   └────────┬────────┘
                                            │ HTTPS
                                            v
                                   ┌─────────────────┐
                                   │   Cloud AI       │
                                   │  (Fly.io)        │
                                   │                  │
                                   │  - Analysis API  │
                                   │  - Dashboard     │
                                   │  - Data Store    │
                                   └──┬──────────┬───┘
                                      │          │
                              ┌───────┘          └───────┐
                              v                          v
                     ┌──────────────┐          ┌──────────────┐
                     │  Dashboard   │          │ LINE / Slack │
                     │  (Web UI)    │          │  通知          │
                     └──────────────┘          └──────────────┘
```

---

## 3 Ways to Set Up / 3つのセットアップ方法

### 1. PC (Docker)

最も簡単な方法。Docker対応のPC（Windows/Mac/Linux）で動作します。

```bash
docker run -d miseban/agent \
  --camera "rtsp://admin:pass@192.168.1.100/stream1" \
  --token "your_api_token"
```

### 2. Smartphone / スマートフォン

スマートフォンのカメラをそのまま利用。専用アプリで簡単セットアップ。

1. アプリをインストール
2. QRコードでトークンを読み取り
3. カメラを設置位置に固定

### 3. Raspberry Pi Zero 2 W

超小型・低コストの専用デバイス。常時稼働に最適。

```bash
# Raspberry Pi OS Lite にセットアップ
curl -sSL https://miseban-ai.fly.dev/install.sh | bash
miseban-agent --camera /dev/video0 --token "your_api_token"
```

---

## Quick Start / クイックスタート

```bash
# 1. Docker でエージェントを起動
docker run -d --name miseban-agent \
  miseban/agent \
  --camera "rtsp://admin:pass@192.168.1.100/stream1" \
  --token "your_api_token"

# 2. ダッシュボードにアクセス
open https://miseban-ai.fly.dev

# 3. LINE通知を設定（ダッシュボードから）
```

---

## Supported Cameras / 対応カメラ

20以上のブランド、数百モデルに対応しています。RTSP/ONVIF対応カメラであれば基本的に接続可能です。

| Brand | 対応状況 |
|-------|----------|
| Hikvision | 対応済 |
| Dahua | 対応済 |
| Axis | 対応済 |
| TP-Link (TAPO) | 対応済 |
| Reolink | 対応済 |
| Amcrest | 対応済 |
| Ubiquiti (UniFi) | 対応済 |
| EZVIZ | 対応済 |
| Panasonic i-PRO | 対応済 |
| Sony | 対応済 |
| Hanwha (Samsung) | 対応済 |
| Vivotek | 対応済 |
| Bosch | 対応済 |
| Milestone | 対応済 |
| FLIR / Lorex | 対応済 |
| Wyze | 対応済 |
| Eufy | 対応済 |
| Google Nest | 対応予定 |
| Ring | 対応予定 |
| Arlo | 対応予定 |

全対応カメラの詳細は [対応カメラページ](https://miseban-ai.fly.dev/cameras.html) をご覧ください。

---

## Security / セキュリティ

ミセバンAIはセキュリティを最重要事項として設計しています。

| Tool | Purpose |
|------|---------|
| **cargo-deny** | 依存クレートのライセンス・脆弱性チェック |
| **gitleaks** | シークレット・認証情報の漏洩防止 |
| **trivy** | コンテナイメージの脆弱性スキャン |
| **cosign** | コンテナイメージの署名・検証 |
| **SBOM** | ソフトウェア部品表の自動生成 |

映像データはエッジ（Agent）側で処理され、クラウドには分析結果のメタデータのみが送信されます。生映像がクラウドに保存されることはありません。

---

## Development / 開発

### Prerequisites / 前提条件

- Rust 1.86.0+
- Docker (オプション)
- Make

### Commands / コマンド

```bash
make check      # lint + test
make security   # security audit (cargo-deny, gitleaks, trivy)
make build      # release build
make deploy     # deploy to Fly.io
```

### Project Structure / プロジェクト構成

```
miseban-ai/
├── crates/           # Rust ワークスペースクレート
├── web/
│   └── landing/      # ランディングページ (HTML/CSS/JS)
├── scripts/          # ビルド・デプロイスクリプト
├── docs/             # ドキュメント
├── Cargo.toml        # ワークスペース定義
├── Makefile          # ビルドコマンド
├── Dockerfile.web    # Webコンテナ
├── fly.web.toml      # Fly.io設定
└── deny.toml         # cargo-deny設定
```

---

## Roadmap / ロードマップ

| Phase | Period | Goals |
|-------|--------|-------|
| **Phase 1: MVP** | 2026 Q1 | 来客カウント・基本ダッシュボード・LINE通知 |
| **Phase 2: Analytics** | 2026 Q2 | 属性推定・ヒートマップ・AI経営アドバイス |
| **Phase 3: Scale** | 2026 Q3 | マルチ店舗対応・チェーン向け管理画面・API公開 |
| **Phase 4: Ecosystem** | 2026 Q4 | POS連携・在庫管理AI・サードパーティ連携 |

---

## Contributing / コントリビューション

コントリビューション大歓迎です！ Contributions are welcome!

- **Bug Reports**: [GitHub Issues](https://github.com/yukihamada/miseban-ai/issues) にバグ報告をお寄せください
- **Feature Requests**: 新機能のアイデアも Issues でお待ちしています
- **Pull Requests**: コードの改善・機能追加のPRをお送りください

開発に参加いただく際は、既存のコードスタイルとアーキテクチャに準拠してください。

---

## License / ライセンス

[MIT License](LICENSE) - Copyright 2026 MisebanAI
