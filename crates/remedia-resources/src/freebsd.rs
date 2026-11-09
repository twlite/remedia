use super::*;

/// Read this process's `kinfo_proc` through `sysctl(KERN_PROC_PID)`; no subprocess is used.
pub fn collect(result: &mut ProcessSnapshot) {
    unix_usage(result);

    // SAFETY: getpid has no preconditions.
    let pid = unsafe { libc::getpid() };
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    // SAFETY: kinfo_proc is plain old data; the kernel fills the zeroed buffer.
    let mut info: libc::kinfo_proc = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::kinfo_proc>();
    // SAFETY: `mib` has four entries, `info` is writable for `length` bytes, and no new
    // value is supplied.
    let status = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            (&raw mut info).cast(),
            &mut length,
            std::ptr::null(),
            0,
        )
    };

    if status != 0 || length < std::mem::size_of::<libc::kinfo_proc>() {
        return;
    }

    // SAFETY: sysconf has no preconditions.
    let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok();

    result.cpu.thread_count = u64::try_from(info.ki_numthreads).ok();
    result.memory.resident_bytes = u64::try_from(info.ki_rssize)
        .ok()
        .zip(page)
        .and_then(|(pages, page)| pages.checked_mul(page));
    result.memory.virtual_bytes = u64::try_from(info.ki_size).ok();
}
