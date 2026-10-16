//
//  JITEnable.c
//
//  见 RuzuJIT.h 的说明。这里的「自持 JIT」实现参考了：
//    * RetroArch  pkg/apple/JITSupport.m（其注释注明抄自 UTMApp，作者 osy）
//    * PPSSPP     ios/main.mm（jb_enable_ptrace_hack）
//  两者都是 GPL 系项目，与本项目许可一致。
//

#include "RuzuJIT.h"

#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/ptrace.h>
#include <sys/types.h>
#include <unistd.h>

#include <mach/exception_types.h>
#include <mach/mach.h>
#include <mach/mach_vm.h>
#include <mach/thread_status.h>
#include <mach/vm_region.h>

// ---------------------------------------------------------------- csops

extern int csops(pid_t pid, unsigned int ops, void *useraddr, size_t usersize);
#define RUZU_CS_OPS_STATUS 0

// 位含义取自 XNU cs_blobs.h
#define RUZU_CS_GET_TASK_ALLOW 0x00000004u /* 带 get-task-allow 签名 */
#define RUZU_CS_DEBUGGED 0x10000000u       /* 真的被调试过（运行时状态）*/

int ruzu_cs_flags(uint32_t *out) {
    uint32_t flags = 0;
    if (out == NULL) {
        return EINVAL;
    }
    if (csops(getpid(), RUZU_CS_OPS_STATUS, &flags, sizeof(flags)) != 0) {
        return errno ? errno : -1;
    }
    *out = flags;
    return 0;
}

// ---------------------------------------------------------------- 真实内存权限

int ruzu_page_protection(void *addr, size_t len) {
    if (addr == NULL || len == 0) {
        return -1;
    }
    mach_vm_address_t address = (mach_vm_address_t)(uintptr_t)addr;
    mach_vm_size_t size = 0;
    vm_region_basic_info_data_64_t info;
    mach_msg_type_number_t count = VM_REGION_BASIC_INFO_COUNT_64;
    mach_port_t object_name = MACH_PORT_NULL;

    kern_return_t kr = mach_vm_region(mach_task_self(), &address, &size,
                                      VM_REGION_BASIC_INFO_64,
                                      (vm_region_info_t)&info, &count, &object_name);
    if (kr != KERN_SUCCESS) {
        return -1;
    }

    int prot = 0;
    if (info.protection & VM_PROT_READ) {
        prot |= 1;
    }
    if (info.protection & VM_PROT_WRITE) {
        prot |= 2;
    }
    if (info.protection & VM_PROT_EXECUTE) {
        prot |= 4;
    }
    return prot;
}

void ruzu_prot_to_string(int prot, char *out, size_t out_len) {
    if (out == NULL || out_len < 4) {
        return;
    }
    if (prot < 0) {
        snprintf(out, out_len, "未知");
        return;
    }
    out[0] = (prot & 1) ? 'R' : '-';
    out[1] = (prot & 2) ? 'W' : '-';
    out[2] = (prot & 4) ? 'X' : '-';
    out[3] = '\0';
}

// ---------------------------------------------------------------- 自持 JIT

extern int ptrace(int request, pid_t pid, caddr_t addr, int data);
extern boolean_t exc_server(mach_msg_header_t *request, mach_msg_header_t *reply);

#ifndef PT_TRACE_ME
#define PT_TRACE_ME 0
#endif
#ifndef PT_SIGEXC
#define PT_SIGEXC 12
#endif

static mach_port_t g_ruzu_exc_port = MACH_PORT_NULL;

static void *ruzu_exception_thread(void *arg) {
    mach_port_t port = *(mach_port_t *)arg;
    // 一直在这里 serve，直到进程结束
    mach_msg_server(exc_server, 2048, port, MACH_MSG_OPTION_NONE);
    return NULL;
}

int ruzu_try_self_trace(char *why, size_t why_len) {
    uint32_t flags = 0;
    int have_flags = (ruzu_cs_flags(&flags) == 0);
    int already_debugged = have_flags && ((flags & RUZU_CS_DEBUGGED) != 0);

    if (ptrace(PT_TRACE_ME, 0, NULL, 0) < 0) {
        int e = errno ? errno : -1;
        if (why) {
            snprintf(why, why_len,
                     "ptrace(PT_TRACE_ME) 失败 errno=%d (%s)。\n"
                     "常见原因：已经有调试器附加（那就已经具备 JIT 资格），"
                     "或者当前内核不允许自持（iOS 26+ 已封）。",
                     e, strerror(e));
        }
        return e;
    }

    if (!already_debugged) {
        // 自持会让内核把信号改成 Mach 软件异常投递；不接管的话异常一来系统会卡死。
        ptrace(PT_SIGEXC, 0, NULL, 0);

        kern_return_t kr = mach_port_allocate(mach_task_self(), MACH_PORT_RIGHT_RECEIVE,
                                              &g_ruzu_exc_port);
        if (kr != KERN_SUCCESS) {
            if (why) {
                snprintf(why, why_len, "mach_port_allocate 失败 kr=%d", kr);
            }
            return 2;
        }
        kr = mach_port_insert_right(mach_task_self(), g_ruzu_exc_port, g_ruzu_exc_port,
                                    MACH_MSG_TYPE_MAKE_SEND);
        if (kr != KERN_SUCCESS) {
            if (why) {
                snprintf(why, why_len, "mach_port_insert_right 失败 kr=%d", kr);
            }
            return 3;
        }
        kr = task_set_exception_ports(mach_task_self(), EXC_MASK_SOFTWARE,
                                      g_ruzu_exc_port, EXCEPTION_DEFAULT,
                                      THREAD_STATE_NONE);
        if (kr != KERN_SUCCESS) {
            if (why) {
                snprintf(why, why_len, "task_set_exception_ports 失败 kr=%d", kr);
            }
            return 4;
        }
        pthread_t thread;
        if (pthread_create(&thread, NULL, ruzu_exception_thread, &g_ruzu_exc_port) != 0) {
            if (why) {
                snprintf(why, why_len, "启动异常处理线程失败");
            }
            return 5;
        }
        if (why) {
            snprintf(why, why_len,
                     "已自持成功：PT_TRACE_ME + PT_SIGEXC + Mach 异常服务线程已就绪。\n"
                     "（这条路的可用性随系统版本而变；iOS 26+ 内核已封，届时请改用调试器附加）");
        }
    } else {
        // 已经有调试器了，只需要把 EXC_BAD_ACCESS 静音（JIT 代码常触发它）
        task_set_exception_ports(mach_task_self(), EXC_MASK_BAD_ACCESS, MACH_PORT_NULL,
                                 EXCEPTION_DEFAULT, THREAD_STATE_NONE);
        if (why) {
            snprintf(why, why_len, "本进程已有调试器附加，无需自持（已把 EXC_BAD_ACCESS 设为忽略）");
        }
    }
    return 0;
}
