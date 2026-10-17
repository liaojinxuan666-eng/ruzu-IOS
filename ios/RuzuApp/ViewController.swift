//
//  ViewController.swift
//  P1 阶段的诊断界面：把「这台设备能不能跑 ruzu」三件事查清楚。
//

import UIKit
import Metal
import QuartzCore
import UniformTypeIdentifiers

final class ViewController: UIViewController, UIDocumentPickerDelegate {

    private let previewView = MetalPreviewView()
    private let textView = UITextView()
    private let statusLabel = UILabel()

    override func viewDidLoad() {
        super.viewDidLoad()
        title = "ruzu iOS · 诊断"
        view.backgroundColor = .systemBackground
        RuzuFFI.configureLogPath()
        buildUI()
    }

    // MARK: - UI

    private func buildUI() {
        statusLabel.font = .preferredFont(forTextStyle: .headline)
        statusLabel.numberOfLines = 0
        statusLabel.text = RuzuFFI.version

        previewView.translatesAutoresizingMaskIntoConstraints = false
        previewView.backgroundColor = .secondarySystemBackground
        previewView.isHidden = true

        textView.font = UIFont.monospacedSystemFont(ofSize: 12, weight: .regular)
        textView.isEditable = false
        textView.layer.borderColor = UIColor.separator.cgColor
        textView.layer.borderWidth = 1
        textView.layer.cornerRadius = 8
        textView.text = "点下面的按钮开始诊断。"

        let safe = UIButton(type: .system)
        safe.setTitle("① 运行诊断（安全）", for: .normal)
        safe.addTarget(self, action: #selector(runSafeDiagnostics), for: .touchUpInside)

        let metal = UIButton(type: .system)
        metal.setTitle("② Metal 自检（官方渲染后端）", for: .normal)
        metal.addTarget(self, action: #selector(runMetalSelfTest), for: .touchUpInside)

        let unsigned = UIButton(type: .system)
        unsigned.setTitle("③ 映射方式对照（越狱）", for: .normal)
        unsigned.addTarget(self, action: #selector(runUnsignedProbe), for: .touchUpInside)

        let nce = UIButton(type: .system)
        nce.setTitle("④ NCE 预检（含执行测试）", for: .normal)
        nce.setTitleColor(.systemRed, for: .normal)
        nce.addTarget(self, action: #selector(runNCEPrecheck), for: .touchUpInside)

        let selfTrace = UIButton(type: .system)
        selfTrace.setTitle("⑤ 自持 JIT（越狱/老系统）", for: .normal)
        selfTrace.addTarget(self, action: #selector(runSelfTrace), for: .touchUpInside)

        let logButton = UIButton(type: .system)
        logButton.setTitle("⑥ 查看探测日志", for: .normal)
        logButton.addTarget(self, action: #selector(showProbeLog), for: .touchUpInside)

        let windowProbe = UIButton(type: .system)
        windowProbe.setTitle("⑦ 窗口接入自检（Metal + core）", for: .normal)
        windowProbe.addTarget(self, action: #selector(runWindowProbe), for: .touchUpInside)

        let loadButton = UIButton(type: .system)
        loadButton.setTitle("⑧ 选择文件并加载（阶段 1）", for: .normal)
        loadButton.addTarget(self, action: #selector(pickAndLoadGame), for: .touchUpInside)

        let buttons = UIStackView(arrangedSubviews: [safe, metal, unsigned, nce, selfTrace, logButton, windowProbe, loadButton])
        buttons.axis = .vertical
        buttons.spacing = 8

        let stack = UIStackView(arrangedSubviews: [statusLabel, buttons, previewView, textView])
        stack.axis = .vertical
        stack.spacing = 12
        stack.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(stack)

        let guide = view.safeAreaLayoutGuide
        NSLayoutConstraint.activate([
            stack.topAnchor.constraint(equalTo: guide.topAnchor, constant: 12),
            stack.leadingAnchor.constraint(equalTo: guide.leadingAnchor, constant: 16),
            stack.trailingAnchor.constraint(equalTo: guide.trailingAnchor, constant: -16),
            stack.bottomAnchor.constraint(equalTo: guide.bottomAnchor, constant: -12),
            previewView.heightAnchor.constraint(equalToConstant: 160),
            textView.heightAnchor.constraint(greaterThanOrEqualToConstant: 240),
        ])
    }

    private func append(_ text: String) {
        textView.text = (textView.text ?? "") + "\n" + text
        let end = NSRange(location: (textView.text as NSString).length, length: 0)
        textView.scrollRangeToVisible(end)
    }

    private func reset(_ header: String) {
        textView.text = "── \(header) ──"
        textView.scrollRangeToVisible(NSRange(location: 0, length: 0))
    }

    // MARK: - 动作

    @objc private func runSafeDiagnostics() {
        reset("诊断（安全）")
        // 探针里有 mmap / mprotect，放到后台线程更干净
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            let report = RuzuFFI.diagnostics
            DispatchQueue.main.async { self?.append(report) }
        }
    }

    @objc private func runUnsignedProbe() {
        reset("映射方式对照")
        // 现在这一步只"读真实权限"，只有在真实权限里确实有 X 时才执行 → 不会闪退
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            let report = RuzuFFI.jitProbeUnsigned
            DispatchQueue.main.async { self?.append(report) }
        }
    }

    @objc private func runMetalSelfTest() {
        reset("Metal 自检")
        previewView.isHidden = false
        view.layoutIfNeeded()
        append(MetalProbe.run(layer: previewView.metalLayer))
    }

    @objc private func runSelfTrace() {
        reset("自持 JIT")
        append(RuzuFFI.trySelfTrace)
        append("\n（如果成功，请重新点「① 运行诊断」，看「真实页面权限」里有没有出现 X）")
    }

    @objc private func showProbeLog() {
        reset("探测日志")
        append(RuzuFFI.log)
    }

    /// 把预览视图的 CAMetalLayer 交给 Rust 侧的 EmuWindow，
    /// 验证「Swift → core → video_core」这条数据通路。
    @objc private func runWindowProbe() {
        reset("窗口接入自检")
        previewView.isHidden = false
        view.layoutIfNeeded()
        guard previewView.bounds.width > 0 else {
            append("[FAIL] 预览视图还没有尺寸")
            return
        }
        let scale = Float(UIScreen.main.scale)
        let layer = previewView.metalLayer
        let ptr = Unmanaged.passUnretained(layer).toOpaque()
        let w = UInt32(previewView.bounds.width * CGFloat(scale))
        let h = UInt32(previewView.bounds.height * CGFloat(scale))
        append(RuzuFFI.windowProbe(layer: ptr, width: w, height: h, scale: scale))
    }

    @objc private func runNCEPrecheck() {
        let alert = UIAlertController(
            title: "NCE 可行性预检",
            message: """
            四组实验：
            · 在预留的客户机 VA 里映射可执行页（先 RW → 写入 → mprotect(RX) → 才执行）
            · 自修改代码后立刻重执行
            · 4 线程独占监视器（LDXR/STXR）压力测试
            · 信号处理器（SIGSEGV/SIGBUS 注册、备用信号栈、信号送达）

            每一步执行前都会先写进 Documents/nce_probe.log —— 万一闪退，
            重开 App 点「⑥ 查看探测日志」就能看到崩在哪一步。
            """,
            preferredStyle: .alert
        )
        alert.addAction(UIAlertAction(title: "取消", style: .cancel))
        alert.addAction(UIAlertAction(title: "开始", style: .default) { [weak self] _ in
            guard let self else { return }
            self.reset("NCE 预检")
            self.append("运行中（多线程独占监视器测试可能要几秒）…")
            DispatchQueue.global(qos: .userInitiated).async {
                let report = RuzuFFI.ncePrecheck
                DispatchQueue.main.async { self.append(report) }
            }
        })
        present(alert, animated: true)
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        if previewView.bounds.width > 0 {
            previewView.metalLayer.drawableSize = CGSize(
                width: previewView.bounds.width * UIScreen.main.scale,
                height: previewView.bounds.height * UIScreen.main.scale
            )
        }
    }
    @objc private func pickAndLoadGame() {
        let picker = UIDocumentPickerViewController(forOpeningContentTypes: [.data], asCopy: true)
        picker.delegate = self
        picker.allowsMultipleSelection = false
        present(picker, animated: true)
    }

    func documentPicker(_ controller: UIDocumentPickerViewController,
                        didPickDocumentsAt urls: [URL]) {
        guard let url = urls.first else { return }
        reset("加载测试（阶段 1）")
        let dst = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
            .appendingPathComponent(url.lastPathComponent)
        try? FileManager.default.removeItem(at: dst)
        do {
            try FileManager.default.copyItem(at: url, to: dst)
            append("已拷入沙盒：\(dst.lastPathComponent)")
            append("开始加载（客户机内存预留 + 解析，可能要几秒）…")
            DispatchQueue.global(qos: .userInitiated).async { [weak self] in
                let result = RuzuFFI.loadGame(path: dst.path)
                DispatchQueue.main.async { self?.append(result) }
            }
        } catch {
            append("[FAIL] 拷贝失败：\(error)")
        }
    }

}

/// 一个 CAMetalLayer 支撑的视图 —— 这就是后面 ruzu Metal 后端要渲染进去的宿主。
final class MetalPreviewView: UIView {
    override class var layerClass: AnyClass { CAMetalLayer.self }
    var metalLayer: CAMetalLayer { layer as! CAMetalLayer }
}
