import SwiftUI
import AVFoundation

struct ContentView: View {
    @StateObject private var agent = AgentViewModel()

    var body: some View {
        NavigationStack {
            VStack(spacing: 24) {
                // Status card
                StatusCard(agent: agent)

                // Camera preview
                if agent.isRunning {
                    CameraPreviewView(session: agent.session)
                        .frame(maxWidth: .infinity)
                        .frame(height: 220)
                        .clipShape(RoundedRectangle(cornerRadius: 12))
                } else {
                    SetupCard(agent: agent)
                }

                // People count
                if agent.isRunning {
                    PeopleCountCard(count: agent.lastCount)
                }

                Spacer()
            }
            .padding()
            .navigationTitle("MisebanAI")
            .navigationBarTitleDisplayMode(.large)
        }
        .onAppear { agent.checkPermissions() }
    }
}

// MARK: - Status Card
struct StatusCard: View {
    @ObservedObject var agent: AgentViewModel

    var body: some View {
        HStack {
            Circle()
                .fill(agent.isRunning ? .green : .gray)
                .frame(width: 10, height: 10)
            Text(agent.isRunning ? "送信中" : "停止中")
                .font(.subheadline)
                .foregroundStyle(.secondary)
            Spacer()
            Button(agent.isRunning ? "停止" : "開始") {
                agent.isRunning ? agent.stop() : agent.start()
            }
            .buttonStyle(.borderedProminent)
            .tint(agent.isRunning ? .red : .blue)
        }
        .padding()
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
    }
}

// MARK: - Setup Card
struct SetupCard: View {
    @ObservedObject var agent: AgentViewModel
    @State private var token = ""
    @State private var apiBase = "https://api.misebanai.com"

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("セットアップ")
                .font(.headline)

            TextField("APIトークン", text: $token)
                .textFieldStyle(.roundedBorder)
                .autocorrectionDisabled()
                .textInputAutocapitalization(.never)

            TextField("APIエンドポイント", text: $apiBase)
                .textFieldStyle(.roundedBorder)
                .autocorrectionDisabled()
                .textInputAutocapitalization(.never)

            Button("保存して開始") {
                agent.configure(token: token, apiBase: apiBase)
                agent.start()
            }
            .buttonStyle(.borderedProminent)
            .disabled(token.isEmpty)
            .frame(maxWidth: .infinity)
        }
        .padding()
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
        .onAppear {
            token = agent.token
            apiBase = agent.apiBase
        }
    }
}

// MARK: - People Count Card
struct PeopleCountCard: View {
    let count: Int

    var body: some View {
        VStack(spacing: 8) {
            Text("\(count)")
                .font(.system(size: 72, weight: .bold, design: .rounded))
                .foregroundStyle(.blue)
            Text("現在の来客数")
                .font(.subheadline)
                .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity)
        .padding()
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
    }
}

// MARK: - Camera Preview
struct CameraPreviewView: UIViewRepresentable {
    let session: AVCaptureSession

    func makeUIView(context: Context) -> PreviewUIView {
        let view = PreviewUIView()
        view.session = session
        return view
    }

    func updateUIView(_ uiView: PreviewUIView, context: Context) {}
}

class PreviewUIView: UIView {
    var session: AVCaptureSession? {
        didSet {
            guard let session else { return }
            (layer as? AVCaptureVideoPreviewLayer)?.session = session
        }
    }

    override class var layerClass: AnyClass { AVCaptureVideoPreviewLayer.self }

    override init(frame: CGRect) {
        super.init(frame: frame)
        (layer as? AVCaptureVideoPreviewLayer)?.videoGravity = .resizeAspectFill
    }

    required init?(coder: NSCoder) { fatalError() }
}
