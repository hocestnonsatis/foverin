#![no_std]
#![no_main]

use aya_ebpf::{
    EbpfContext,
    helpers::bpf_probe_read_kernel_str_bytes,
    macros::{map, tracepoint},
    maps::{PerCpuArray, RingBuf},
    programs::TracePointContext,
};
use foverin_common::ProcessEvent;

/// Shared ring buffer for process-exec events (256 KiB).
#[map]
static EVENTS: RingBuf = RingBuf::with_byte_size(256 * 1024, 0);

/// Scratch buffer — eBPF stack is tiny; build the event here per-CPU.
#[map]
static EVENT_BUF: PerCpuArray<ProcessEvent> = PerCpuArray::with_max_entries(1, 0);

/// Offsets from `/sys/kernel/tracing/events/sched/sched_process_exec/format`
/// (common fields occupy 0..8; `__data_loc char[] filename` starts at 8).
const FILENAME_DATA_LOC_OFFSET: usize = 8;

/// Attached from userspace to `sched/sched_process_exec`.
#[tracepoint]
pub fn foverin(ctx: TracePointContext) -> u32 {
    match try_foverin(ctx) {
        Ok(ret) => ret,
        Err(ret) => ret,
    }
}

fn try_foverin(ctx: TracePointContext) -> Result<u32, u32> {
    let buf = unsafe {
        let ptr = EVENT_BUF.get_ptr_mut(0).ok_or(1u32)?;
        &mut *ptr
    };
    *buf = ProcessEvent {
        // Prefer tgid (userspace PID) over the tracepoint's thread pid.
        pid: ctx.tgid(),
        filename: [0; foverin_common::FILENAME_LEN],
    };

    // `__data_loc`: low 16 bits = offset from start of ctx, high 16 = length.
    let data_loc: u32 = unsafe { ctx.read_at(FILENAME_DATA_LOC_OFFSET) }.map_err(|e| e as u32)?;
    let offset = (data_loc & 0xffff) as usize;
    let filename_ptr = unsafe { (ctx.as_ptr() as *const u8).add(offset) };
    let _ = unsafe { bpf_probe_read_kernel_str_bytes(filename_ptr, &mut buf.filename) };

    let Some(mut entry) = EVENTS.reserve::<ProcessEvent>(0) else {
        return Ok(0);
    };
    entry.write(*buf);
    entry.submit(0);

    Ok(0)
}

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[unsafe(link_section = "license")]
#[unsafe(no_mangle)]
static LICENSE: [u8; 13] = *b"Dual MIT/GPL\0";
