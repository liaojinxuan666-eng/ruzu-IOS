//
//  RuzuFFI.swift
//  Rust 门面（libr uzu_ios.a）的 Swift 友好包装。
//

import Foundation

enum RuzuFFI {

    /// 把 Rust 返回的 `char *` 取成 Swift String 并释放，避免泄漏。
    private static func take(_ ptr: UnsafeMutablePointer<CChar>?) -> String {
        guard let ptr else { return "(Rust 返回空指针)" }
        defer { ruzu_ios_free_string(ptr) }
        return String(cString: ptr)
    }

    static var version: String { take(ruzu_ios_version()) }

    static var diagnostics: String { take(ruzu_ios_diagnostics()) }

    static var jitProbe: String { take(ruzu_ios_jit_probe()) }

    /// 可能闪退，调用前必须让用户知情。
    static var jitProbeUnsigned: String { take(ruzu_ios_jit_probe_unsafe()) }

    /// NCE 可行性预检。可能闪退。
    static var ncePrecheck: String { take(ruzu_ios_nce_precheck()) }

    /// 尝试自持 JIT（ptrace hack）。不会闪退。
    static var trySelfTrace: String { take(ruzu_ios_try_self_trace()) }

    /// 读出探测日志（危险探测执行前会写，闪退后靠它定位）。
    static var log: String { take(ruzu_ios_read_log()) }

    /// 在 App 启动时告诉 Rust 日志写到哪里（Documents/nce_probe.log）。
    static func configureLogPath() {
        let dir = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        let path = dir.appendingPathComponent("nce_probe.log").path
        path.withCString { ruzu_ios_set_log_path($0) }
    }

    /// 窗口接入自检：把 CAMetalLayer 指针交给 Rust 侧的 EmuWindow。
    static func windowProbe(layer: UnsafeMutableRawPointer,
                            width: UInt32, height: UInt32, scale: Float) -> String {
        take(ruzu_ios_window_probe(layer, width, height, scale))
    }

    static var coreLinkProbe: String { take(ruzu_ios_core_link_probe()) }

    /// 阶段 1：加载（只加载，不渲染）。
    static func loadGame(path: String) -> String {
        path.withCString { take(ruzu_ios_load_game($0)) }
    }

    /// 真正开机。
    static func boot(path: String, layer: UnsafeMutableRawPointer,
                     width: UInt32, height: UInt32, scale: Float) -> String {
        path.withCString { take(ruzu_ios_boot($0, layer, width, height, scale)) }
    }
}
