import Foundation
import AVFoundation
import UIKit
import Combine

@MainActor
class AgentViewModel: NSObject, ObservableObject {
    // MARK: Published state
    @Published var isRunning = false
    @Published var lastCount = 0
    @Published var statusMessage = "カメラアクセスを確認中..."

    // MARK: Config (persisted in UserDefaults)
    @Published var token: String = UserDefaults.standard.string(forKey: "apiToken") ?? ""
    @Published var apiBase: String = UserDefaults.standard.string(forKey: "apiBase") ?? "https://api.misebanai.com"

    let session = AVCaptureSession()
    private var photoOutput = AVCapturePhotoOutput()
    private var captureTimer: Timer?
    private let intervalSeconds: TimeInterval = 5

    func configure(token: String, apiBase: String) {
        self.token = token
        self.apiBase = apiBase
        UserDefaults.standard.set(token, forKey: "apiToken")
        UserDefaults.standard.set(apiBase, forKey: "apiBase")
    }

    func checkPermissions() {
        switch AVCaptureDevice.authorizationStatus(for: .video) {
        case .authorized: setupCamera()
        case .notDetermined:
            AVCaptureDevice.requestAccess(for: .video) { [weak self] granted in
                Task { @MainActor in
                    if granted { self?.setupCamera() }
                }
            }
        default:
            statusMessage = "カメラのアクセスを許可してください"
        }
    }

    private func setupCamera() {
        session.beginConfiguration()
        session.sessionPreset = .hd1280x720

        guard let device = AVCaptureDevice.default(.builtInWideAngleCamera, for: .video, position: .back),
              let input = try? AVCaptureDeviceInput(device: device) else {
            statusMessage = "カメラを初期化できません"
            return
        }

        if session.canAddInput(input) { session.addInput(input) }
        if session.canAddOutput(photoOutput) { session.addOutput(photoOutput) }

        session.commitConfiguration()
        statusMessage = "カメラ準備完了"
    }

    func start() {
        guard !token.isEmpty else { statusMessage = "APIトークンを入力してください"; return }

        Task.detached { [weak self] in
            self?.session.startRunning()
        }

        captureTimer = Timer.scheduledTimer(withTimeInterval: intervalSeconds, repeats: true) { [weak self] _ in
            self?.captureAndSend()
        }
        captureTimer?.fire()
        isRunning = true
    }

    func stop() {
        captureTimer?.invalidate()
        captureTimer = nil
        Task.detached { [weak self] in
            self?.session.stopRunning()
        }
        isRunning = false
    }

    private func captureAndSend() {
        let settings = AVCapturePhotoSettings()
        settings.flashMode = .off
        photoOutput.capturePhoto(with: settings, delegate: self)
    }

    private func sendFrame(jpeg: Data) {
        let url = URL(string: "\(apiBase)/api/v1/frames")!
        var req = URLRequest(url: url)
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")

        let body: [String: Any] = [
            "camera_id": "ios-\(UIDevice.current.identifierForVendor?.uuidString ?? "unknown")",
            "timestamp": ISO8601DateFormatter().string(from: Date()),
            "jpeg_bytes": jpeg.base64EncodedString(),
            "resolution": ["width": 1280, "height": 720]
        ]

        req.httpBody = try? JSONSerialization.data(withJSONObject: body)

        URLSession.shared.dataTask(with: req) { [weak self] data, _, error in
            guard let data, error == nil,
                  let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return }

            Task { @MainActor [weak self] in
                self?.lastCount = json["people_count"] as? Int ?? 0
            }
        }.resume()
    }
}

// MARK: - AVCapturePhotoCaptureDelegate
extension AgentViewModel: AVCapturePhotoCaptureDelegate {
    func photoOutput(_ output: AVCapturePhotoOutput,
                     didFinishProcessingPhoto photo: AVCapturePhoto,
                     error: Error?) {
        guard error == nil, let jpeg = photo.fileDataRepresentation() else { return }
        sendFrame(jpeg: jpeg)
    }
}
