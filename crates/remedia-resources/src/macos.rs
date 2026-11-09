use super::*;

unsafe extern "C" {
    // The Mach SDK exposes mach_task_self() as this stable process-local port.
    #[link_name = "mach_task_self_"]
    static CURRENT_TASK: libc::mach_port_t;
}

pub fn collect(result: &mut ProcessSnapshot) {
    unix_usage(result);
    // TASK_VM_INFO revision 1 is a stable prefix: 19 u64-sized slots,
    // including the two adjacent i32 fields at slot 1. Request only that prefix.
    let mut info = [0_u64; 19];
    let mut count = (std::mem::size_of_val(&info) / 4) as u32;
    // SAFETY: aligned writable storage has the advertised number of natural_t
    // words; task_info copies a versioned prefix and updates count. No pointers escape.
    if unsafe { libc::task_info(CURRENT_TASK, 22, info.as_mut_ptr().cast(), &mut count) } == 0
        && count >= 38
    {
        result.memory.virtual_bytes = Some(info[0]);
        result.memory.resident_bytes = Some(info[2]);
        result.memory.peak_resident_bytes = Some(info[3]);
        result.memory.footprint_bytes = Some(info[18]);
    }
    // proc_taskinfo supplies actual kernel thread count, independently of players.
    let mut task: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&task) as i32;
    // SAFETY: libproc synchronously writes exactly this known C structure.
    if unsafe {
        libc::proc_pidinfo(
            std::process::id() as i32,
            libc::PROC_PIDTASKINFO,
            0,
            (&mut task as *mut libc::proc_taskinfo).cast(),
            size,
        )
    } == size
    {
        result.cpu.thread_count = u64::try_from(task.pti_threadnum).ok();
        result.memory.resident_bytes = result
            .memory
            .resident_bytes
            .or(Some(task.pti_resident_size));
        result.memory.virtual_bytes = result.memory.virtual_bytes.or(Some(task.pti_virtual_size));
    }
    // First obtain the required list size; a growing descriptor table is retried
    // once. Counting only a complete result avoids reporting a truncated count.
    for _ in 0..2 {
        let needed = unsafe {
            libc::proc_pidinfo(
                std::process::id() as i32,
                libc::PROC_PIDLISTFDS,
                0,
                std::ptr::null_mut(),
                0,
            )
        };
        if needed <= 0 {
            break;
        }
        let count = needed as usize / std::mem::size_of::<libc::proc_fdinfo>() + 32;
        let mut entries: Vec<libc::proc_fdinfo> =
            (0..count).map(|_| unsafe { std::mem::zeroed() }).collect();
        let size = std::mem::size_of_val(entries.as_slice()) as i32;
        // SAFETY: entries is aligned initialized storage of the stated byte length.
        let written = unsafe {
            libc::proc_pidinfo(
                std::process::id() as i32,
                libc::PROC_PIDLISTFDS,
                0,
                entries.as_mut_ptr().cast(),
                size,
            )
        };
        if written > 0 && written < size {
            result.open_handles =
                Some(written as u64 / std::mem::size_of::<libc::proc_fdinfo>() as u64);
            break;
        }
    }
    // Neither internal resident bytes nor phys_footprint is USS. Leave private
    // resident, private commit, PSS, and reserved virtual unsupported here.
}
