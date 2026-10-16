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
}
