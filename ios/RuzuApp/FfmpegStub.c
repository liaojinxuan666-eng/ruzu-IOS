//
//  FfmpegStub.c
//
//  iOS 上暂不提供 FFmpeg（视频硬件解码 / nvdec）。
//
//  背景：`video_core/src/host1x/ffmpeg/ffmpeg.rs` 里有 30 个 `ruzu_ffmpeg_*`
//  外部函数，真正实现在 `ffmpeg_shim.c` —— 那个文件要 libavcodec 的头文件和库。
//  我们在 iOS 上跳过了它的编译（改造见 ios/patches/apply_ios_patches.py），
//  于是链接期会出现 30 个未定义符号。
//
//  这里给出**安全桩**：
//    * 所有返回指针的函数都返回**非 NULL** 的有效指针（调用方会用 CStr::from_ptr
//      解引用，返回 NULL 会直接崩）；
//    * 所有"操作类"函数返回负值（失败），让上层优雅地判定"解码不可用"；
//    * 不分配、不解码、不接触任何 FFmpeg 类型。
//
//  影响范围：只在游戏播放过场视频时才会走到，FreeBrick 这类家酿完全不受影响。
//  将来要做视频解码时，把这份桩换成真正为本平台交叉编译的 FFmpeg 即可。
//

#include <stdint.h>
#include <stdlib.h>
#include <string.h>

// 用一个 1 字节的哑对象当句柄，保证非 NULL。
static void *ffmpeg_disabled_handle(void) {
    static char dummy = 0;
    return &dummy;
}

void *ruzu_ffmpeg_decoder_create(uint64_t codec, int prefer_mediacodec) {
    (void)codec;
    (void)prefer_mediacodec;
    return ffmpeg_disabled_handle();
}

const char *ruzu_ffmpeg_decoder_name(const void *decoder) {
    (void)decoder;
    return "ffmpeg-disabled-on-ios";
}

int ruzu_ffmpeg_decoder_open(void *decoder, const unsigned char *extradata,
                             uintptr_t extradata_size) {
    (void)decoder;
    (void)extradata;
    (void)extradata_size;
    return -1; // 打开失败 → 上层判定不支持该编码
}

void ruzu_ffmpeg_decoder_destroy(void *decoder) {
    (void)decoder;
}

void *ruzu_ffmpeg_hardware_context_create(void) {
    return ffmpeg_disabled_handle();
}

void ruzu_ffmpeg_hardware_context_destroy(void *hardware) {
    (void)hardware;
}

int ruzu_ffmpeg_decoder_supports_decoding_on_device(uint64_t codec, int device_type,
                                                    int *out_pix_fmt) {
    (void)codec;
    (void)device_type;
    if (out_pix_fmt != NULL) {
        *out_pix_fmt = 0;
    }
    return 0; // 不支持
}

uintptr_t ruzu_ffmpeg_supported_device_types(int *out, uintptr_t out_capacity) {
    (void)out;
    (void)out_capacity;
    return 0;
}

uintptr_t ruzu_ffmpeg_preferred_device_types(int *out, uintptr_t out_capacity) {
    (void)out;
    (void)out_capacity;
    return 0;
}

const char *ruzu_ffmpeg_device_type_name(int device_type) {
    (void)device_type;
    return "none";
}

int ruzu_ffmpeg_decoder_send_packet(void *decoder, const unsigned char *data, uintptr_t size,
                                    int64_t pts, int64_t dts) {
    (void)decoder;
    (void)data;
    (void)size;
    (void)pts;
    (void)dts;
    return -1;
}

void *ruzu_ffmpeg_decoder_receive_frame_with_hw_transfer(void *decoder) {
    (void)decoder;
    return NULL; // 没有帧
}

int ruzu_ffmpeg_hardware_initialize_with_type(void *hardware, int device_type) {
    (void)hardware;
    (void)device_type;
    return -1;
}

int ruzu_ffmpeg_hardware_last_error(const void *hardware) {
    (void)hardware;
    return -1;
}

const char *ruzu_ffmpeg_hardware_vaapi_vendor_name(const void *hardware, int device_type) {
    (void)hardware;
    (void)device_type;
    return "none";
}

int ruzu_ffmpeg_decoder_initialize_hardware(void *decoder, const void *hardware,
                                            int pixel_format) {
    (void)decoder;
    (void)hardware;
    (void)pixel_format;
    return -1;
}

int ruzu_ffmpeg_decoder_last_error(const void *decoder) {
    (void)decoder;
    return -1;
}

int ruzu_ffmpeg_error_is_eof_or_again(int error) {
    (void)error;
    return 0;
}

void ruzu_ffmpeg_error_string(int errnum, char *out, uintptr_t out_size) {
    (void)errnum;
    if (out != NULL && out_size > 0) {
        const char *msg = "ffmpeg 未在 iOS 上编译（视频解码不可用）";
        strncpy(out, msg, (size_t)out_size - 1);
        out[out_size - 1] = '\0';
    }
}

void *ruzu_ffmpeg_frame_create(void) {
    return NULL;
}

void ruzu_ffmpeg_frame_destroy(void *frame) {
    (void)frame;
}

int ruzu_ffmpeg_frame_width(const void *frame) {
    (void)frame;
    return 0;
}

int ruzu_ffmpeg_frame_height(const void *frame) {
    (void)frame;
    return 0;
}

int ruzu_ffmpeg_frame_format(const void *frame) {
    (void)frame;
    return 0;
}

int ruzu_ffmpeg_frame_stride(const void *frame, int plane) {
    (void)frame;
    (void)plane;
    return 0;
}

const unsigned char *ruzu_ffmpeg_frame_plane(const void *frame, int plane) {
    (void)frame;
    (void)plane;
    return NULL;
}

int ruzu_ffmpeg_frame_interlaced(const void *frame) {
    (void)frame;
    return 0;
}

int ruzu_ffmpeg_frame_is_hardware_decoded(const void *frame) {
    (void)frame;
    return 0;
}
