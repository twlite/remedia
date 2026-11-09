use super::*;
use windows_sys::Win32::{
    Foundation::*,
    System::{Diagnostics::ToolHelp::*, Memory::*, ProcessStatus::*, Threading::*},
};

pub fn collect(result: &mut ProcessSnapshot) {
    // SAFETY: current-process pseudo handle is always valid and is not closed;
    // all native calls receive correctly sized, initialized owned output buffers.
    unsafe {
        let process = GetCurrentProcess();
        let mut memory: PROCESS_MEMORY_COUNTERS_EX2 = std::mem::zeroed();
        memory.cb = std::mem::size_of_val(&memory) as u32;
        let extended = K32GetProcessMemoryInfo(
            process,
            (&mut memory as *mut PROCESS_MEMORY_COUNTERS_EX2).cast(),
            memory.cb,
        ) != 0;
        if extended {
            result.memory.private_resident_bytes = Some(memory.PrivateWorkingSetSize as u64);
        } else {
            memory.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        }
        if extended
            || K32GetProcessMemoryInfo(
                process,
                (&mut memory as *mut PROCESS_MEMORY_COUNTERS_EX2).cast(),
                memory.cb,
            ) != 0
        {
            result.memory.resident_bytes = Some(memory.WorkingSetSize as u64);
            result.memory.peak_resident_bytes = Some(memory.PeakWorkingSetSize as u64);
            result.memory.committed_private_bytes = Some(memory.PrivateUsage as u64);
        }
        let (mut creation, mut exit, mut kernel, mut user): (
            FILETIME,
            FILETIME,
            FILETIME,
            FILETIME,
        ) = std::mem::zeroed();
        if GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) != 0 {
            let ms = |v: FILETIME| {
                ((u64::from(v.dwHighDateTime) << 32) | u64::from(v.dwLowDateTime)) as f64 / 10_000.0
            };
            result.cpu.user_time_ms = Some(ms(user));
            result.cpu.system_time_ms = Some(ms(kernel));
        }
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot != INVALID_HANDLE_VALUE {
            let mut entry: THREADENTRY32 = std::mem::zeroed();
            entry.dwSize = std::mem::size_of_val(&entry) as u32;
            let mut threads = 0;
            if Thread32First(snapshot, &mut entry) != 0 {
                loop {
                    if entry.th32OwnerProcessID == std::process::id() {
                        threads += 1;
                    }
                    if Thread32Next(snapshot, &mut entry) == 0 {
                        break;
                    }
                }
                if GetLastError() == ERROR_NO_MORE_FILES {
                    result.cpu.thread_count = Some(threads);
                }
            }
            CloseHandle(snapshot);
        }
        let mut handles = 0;
        if GetProcessHandleCount(process, &mut handles) != 0 {
            result.open_handles = Some(u64::from(handles));
        }
        // VirtualQuery covers address-space regions, not resident RAM. Only
        // MEM_RESERVE is counted as reserved/uncommitted; free holes are excluded.
        let (mut address, mut virtual_bytes, mut reserved) = (0_usize, 0_u64, 0_u64);
        loop {
            let mut region: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
            if VirtualQuery(
                address as *const _,
                &mut region,
                std::mem::size_of_val(&region),
            ) == 0
            {
                if address > 0 && GetLastError() == ERROR_INVALID_PARAMETER {
                    result.memory.virtual_bytes = Some(virtual_bytes);
                    result.memory.reserved_virtual_bytes = Some(reserved);
                }
                break;
            }
            if region.State != MEM_FREE {
                virtual_bytes = virtual_bytes.saturating_add(region.RegionSize as u64);
            }
            if region.State == MEM_RESERVE {
                reserved = reserved.saturating_add(region.RegionSize as u64);
            }
            let Some(next) = (region.BaseAddress as usize)
                .checked_add(region.RegionSize)
                .filter(|next| *next > address)
            else {
                break;
            };
            address = next;
        }
    }
}
