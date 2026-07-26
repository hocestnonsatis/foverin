#![no_std]
#![no_main]

use aya_ebpf::{
    EbpfContext,
    helpers::{
        TASK_COMM_LEN, bpf_get_current_task, bpf_probe_read_kernel, bpf_probe_read_kernel_str_bytes,
    },
    macros::{map, tracepoint},
    maps::{PerCpuArray, RingBuf},
    programs::TracePointContext,
};
use foverin_common::ProcessEvent;

include!(concat!(env!("OUT_DIR"), "/task_offsets.rs"));

/// Shared ring buffer for process-exec events (256 KiB).
#[map]
static EVENTS: RingBuf = RingBuf::with_byte_size(256 * 1024, 0);

/// Scratch buffer — eBPF stack is tiny; build the event here per-CPU.
#[map]
static EVENT_BUF: PerCpuArray<ProcessEvent> = PerCpuArray::with_max_entries(1, 0);

/// Offsets from `/sys/kernel/tracing/events/sched/sched_process_exec/format`
/// (common fields occupy 0..8; `__data_loc char[] filename` starts at 8).
const FILENAME_DATA_LOC_OFFSET: usize = 8;

/// Synthetic token written when a Steam child execs an unknown binary.
const STEAM_APP: &[u8] = b"steam_app\0";

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

    // Lineage: Steam-spawned children (often OOV game.exe under Proton) → `steam_app`.
    // That token is in the userspace VOCAB gaming group so cold UCB priors bias performance.
    if parent_comm_is_steam() {
        write_steam_app(&mut buf.filename);
    }

    let Some(mut entry) = EVENTS.reserve::<ProcessEvent>(0) else {
        return Ok(0);
    };
    entry.write(*buf);
    entry.submit(0);

    Ok(0)
}

/// Read `current->real_parent->comm` via `bpf_get_current_task` + BTF offsets.
fn parent_comm_is_steam() -> bool {
    let task = unsafe { bpf_get_current_task() } as *const u8;
    if task.is_null() {
        return false;
    }

    let parent_ptr_addr = unsafe { task.add(TASK_REAL_PARENT_OFFSET) } as *const *const u8;
    let parent = match unsafe { bpf_probe_read_kernel(parent_ptr_addr) } {
        Ok(p) => p,
        Err(_) => return false,
    };
    if parent.is_null() {
        return false;
    }

    let comm_addr = unsafe { parent.add(TASK_COMM_OFFSET) } as *const [u8; TASK_COMM_LEN];
    let comm: [u8; TASK_COMM_LEN] = match unsafe { bpf_probe_read_kernel(comm_addr) } {
        Ok(c) => c,
        Err(_) => return false,
    };

    // Exact match on the TASK_COMM_LEN-padded basename "steam".
    comm_eq(&comm, b"steam")
}

fn comm_eq(comm: &[u8; TASK_COMM_LEN], name: &[u8]) -> bool {
    if name.len() >= TASK_COMM_LEN {
        return false;
    }
    let mut i = 0;
    while i < name.len() {
        if comm[i] != name[i] {
            return false;
        }
        i += 1;
    }
    // Null-terminated (remaining bytes may be zero-padded).
    comm[name.len()] == 0
}

fn write_steam_app(filename: &mut [u8; foverin_common::FILENAME_LEN]) {
    *filename = [0; foverin_common::FILENAME_LEN];
    let mut i = 0;
    while i < STEAM_APP.len() && i < foverin_common::FILENAME_LEN {
        filename[i] = STEAM_APP[i];
        i += 1;
    }
}

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[unsafe(link_section = "license")]
#[unsafe(no_mangle)]
static LICENSE: [u8; 13] = *b"Dual MIT/GPL\0";
