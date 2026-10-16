//
//  ViewController.swift
//  P1 阶段的诊断界面：把「这台设备能不能跑 ruzu」三件事查清楚。
//

import UIKit
import Metal
import QuartzCore

final class ViewController: UIViewController {

    private let previewView = MetalPreviewView()
    private let textView = UITextView()
    private let statusLabel = UILabel()

    override func viewDidLoad() {
        super.viewDidLoad()
        title = "ruzu iOS · 诊断"
        view.backgroundColor = .systemBackground
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
        unsigned.setTitle("③ 越狱路径探测（可能闪退）", for: .normal)
        unsigned.setTitleColor(.systemRed, for: .normal)
        unsigned.addTarget(self, action: #selector(runUnsignedProbe), for: .touchUpInside)

        let buttons = UIStackView(arrangedSubviews: [safe, metal, unsigned])
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
        let alert = UIAlertController(
            title: "越狱路径探测",
            message: """
            这会尝试执行「裸 RWX」和「RW → mprotect(RX)」这两种可执行内存。
            如果本 App 没有 JIT 权限，**它可能直接闪退** —— 闪退本身就是结论：
            说明这条路也走不通（重开 App 即可）。

            越狱环境下请点「继续」。
            """,
            preferredStyle: .alert
        )
        alert.addAction(UIAlertAction(title: "取消", style: .cancel))
        alert.addAction(UIAlertAction(title: "继续", style: .destructive) { [weak self] _ in
            self?.reset("越狱路径探测")
            self?.append(RuzuFFI.jitProbeUnsigned)
        })
        present(alert, animated: true)
    }

    @objc private func runMetalSelfTest() {
        reset("Metal 自检")
        previewView.isHidden = false
        view.layoutIfNeeded()
        append(MetalProbe.run(layer: previewView.metalLayer))
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
}

/// 一个 CAMetalLayer 支撑的视图 —— 这就是后面 ruzu Metal 后端要渲染进去的宿主。
final class MetalPreviewView: UIView {
    override class var layerClass: AnyClass { CAMetalLayer.self }
    var metalLayer: CAMetalLayer { layer as! CAMetalLayer }
}
