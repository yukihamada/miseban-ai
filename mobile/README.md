# MisebanAI Mobile Apps

スマートフォンをカメラとして使い、MisebanAI APIにフレームを送信するアプリです。

## iOS App

### 要件
- iOS 16+
- Xcode 15+

### ビルド手順
1. Xcodeで新規プロジェクト作成: `File → New → Project → iOS App`
   - Product Name: `MisebanAgent`
   - Organization Identifier: `com.misebanai`
   - Interface: `SwiftUI`
   - Language: `Swift`
2. `ContentView.swift` と `AgentViewModel.swift` を追加
3. `Info.plist` にカメラ使用許可を追加:
   ```xml
   <key>NSCameraUsageDescription</key>
   <string>店舗の来客数を計測するためにカメラを使用します</string>
   ```
4. シミュレーターまたは実機でビルド・実行

### 機能
- 背面カメラで5秒ごとにJPEGをキャプチャ
- MisebanAI APIにフレームを送信
- リアルタイムの来客数を表示

---

## Android App

### 要件
- Android 8.0+ (API 26+)
- Android Studio Hedgehog+

### ビルド手順
1. Android Studioで新規プロジェクト作成:
   - Template: `Empty Activity (Compose)`
   - Package name: `com.misebanai.agent`
   - Minimum SDK: API 26
2. `app/build.gradle` に依存関係を追加:
   ```groovy
   implementation "androidx.camera:camera-core:1.3.1"
   implementation "androidx.camera:camera-camera2:1.3.1"
   implementation "androidx.camera:camera-lifecycle:1.3.1"
   implementation "androidx.camera:camera-view:1.3.1"
   ```
3. `MainActivity.kt` を配置
4. `AndroidManifest.xml` にカメラ権限を追加:
   ```xml
   <uses-permission android:name="android.permission.CAMERA" />
   <uses-permission android:name="android.permission.INTERNET" />
   ```
5. ビルド・インストール

### 機能
- 背面カメラで5秒ごとにJPEGをキャプチャ
- MisebanAI APIにフレームを送信
- リアルタイムの来客数を表示

---

## 設定

両アプリ共通の設定項目:

| 項目 | 説明 | デフォルト |
|------|------|----------|
| APIトークン | ダッシュボードで発行したトークン | - |
| APIエンドポイント | APIサーバーURL | `https://api.misebanai.com` |

設定はアプリ内で保存されます。

---

## 将来の計画

- [ ] バックグラウンド実行 (iOSはBackground Modes、AndroidはForeground Service)
- [ ] 複数カメラ切り替え (フロント/バック)
- [ ] ローカルストレージへのフレーム保存
- [ ] プッシュ通知 (来客数異常検知)
- [ ] App Store / Google Play 公開
