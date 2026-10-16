//
//  MetalProbe.swift
//  验证官方 Metal 后端在这台设备上能不能正常工作：
//  建队列 → 拿 drawable → 清屏一帧 → present。
//  这跟 ruzu 的 video_core/renderer_metal 用的是同一套 CAMetalLayer 机制。
//

import Foundation
import Metal
import QuartzCore
import UIKit

enum MetalProbe {

    /// 队列要活着，否则 command buffer 提交后立刻被回收。
    private static var queueCache: [ObjectIdentifier: MTLCommandQueue] = [:]

    static func run(layer: CAMetalLayer) -> String {
        var out = ""

        guard let device = layer.device ?? MTLCreateSystemDefaultDevice() else {
            return "[FAIL] 这台设备没有 Metal device（模拟器里也不会走这条路）"
        }
        layer.device = device
        layer.pixelFormat = .bgra8Unorm
        layer.framebufferOnly = true
        layer.presentsWithTransaction = false

        out += "[OK]  Metal 设备：\(device.name)\n"
        out += "      registryID：\(device.registryID)\n"
        out += "      最大线程组尺寸：\(device.maxThreadsPerThreadgroup.width)×\(device.maxThreadsPerThreadgroup.height)×\(device.maxThreadsPerThreadgroup.depth)\n"

        guard let queue = device.makeCommandQueue() else {
            return out + "[FAIL] 创建 MTLCommandQueue 失败"
        }
        queueCache[ObjectIdentifier(device)] = queue

        guard layer.drawableSize.width > 0, layer.drawableSize.height > 0 else {
            return out + "[FAIL] drawableSize 为 0（视图还没上屏？）"
        }

        guard let drawable = layer.nextDrawable() else {
            return out + "[FAIL] nextDrawable 返回 nil"
        }
        out += "[OK]  拿到 drawable：\(Int(drawable.texture.width))×\(Int(drawable.texture.height))\n"

        let descriptor = MTLRenderPassDescriptor()
        descriptor.colorAttachments[0].texture = drawable.texture
        descriptor.colorAttachments[0].loadAction = .clear
        descriptor.colorAttachments[0].storeAction = .store
        descriptor.colorAttachments[0].clearColor = MTLClearColor(
            red: 0.15, green: 0.35, blue: 0.85, alpha: 1.0
        )

        guard let buffer = queue.makeCommandBuffer(),
              let encoder = buffer.makeRenderCommandEncoder(descriptor: descriptor) else {
            return out + "[FAIL] 创建 command buffer / render encoder 失败"
        }
        encoder.endEncoding()
        buffer.present(drawable)
        buffer.commit()
        buffer.waitUntilCompleted()

        if let error = buffer.error {
            out += "[FAIL] 提交出错：\(error)\n"
        } else {
            out += "[OK]  已清屏并 present 一帧 —— 如果上面的方块变蓝，Metal 通路就是通的。\n"
        }
        return out
    }
}
