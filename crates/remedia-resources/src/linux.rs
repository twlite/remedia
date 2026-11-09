use super::*;

fn field(contents: &str, name: &str, kib: bool) -> Option<u64> {
    let line = contents.lines().find_map(|line| line.strip_prefix(name))?;
    let mut words = line.split_whitespace();
    let value = words.next()?.parse::<u64>().ok()?;
    if kib {
        (words.next()? == "kB").then_some(())?;
        value.checked_mul(1024)
    } else {
        Some(value)
    }
}

fn memory(status: &str, rollup: &str) -> MemorySnapshot {
    MemorySnapshot {
        resident_bytes: field(rollup, "Rss:", true).or_else(|| field(status, "VmRSS:", true)),
        private_resident_bytes: field(rollup, "Private_Clean:", true)
            .zip(field(rollup, "Private_Dirty:", true))
            .and_then(|(a, b)| a.checked_add(b)),
        proportional_resident_bytes: field(rollup, "Pss:", true),
        virtual_bytes: field(status, "VmSize:", true),
        peak_resident_bytes: field(status, "VmHWM:", true),
        ..Default::default()
    }
}

#[cfg(target_os = "linux")]
pub fn collect(result: &mut ProcessSnapshot) {
    unix_usage(result);
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    // smaps_rollup is optional (kernel support/permissions). Never infer USS from RSS.
    let rollup = std::fs::read_to_string("/proc/self/smaps_rollup").unwrap_or_default();
    result.memory = memory(&status, &rollup);
    result.cpu.thread_count = field(&status, "Threads:", false);
    result.open_handles = std::fs::read_dir("/proc/self/fd")
        .ok()
        .map(|entries| entries.filter_map(Result::ok).count().saturating_sub(1) as u64);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separates_private_resident_pss_and_virtual_and_converts_kib() {
        let result = memory(
            "VmRSS: 20 kB\nVmSize: 90 kB\nVmHWM: 30 kB",
            "Rss: 21 kB\nPrivate_Clean: 3 kB\nPrivate_Dirty: 4 kB\nPss: 10 kB",
        );
        assert_eq!(result.resident_bytes, Some(21 * 1024));
        assert_eq!(result.private_resident_bytes, Some(7 * 1024));
        assert_eq!(result.proportional_resident_bytes, Some(10 * 1024));
        assert_eq!(result.virtual_bytes, Some(90 * 1024));
        assert!(result.committed_private_bytes.is_none());
        assert!(result.reserved_virtual_bytes.is_none());
        let unavailable = memory("VmRSS: 20 kB", "");
        assert!(unavailable.private_resident_bytes.is_none());
        assert_eq!(unavailable.resident_bytes, Some(20 * 1024));
    }
}

#[cfg(test)]
mod unavailable_tests {
    use super::*;

    #[test]
    fn malformed_units_overflows_and_partial_private_data_remain_unavailable() {
        assert_eq!(field("VmRSS: 5 MB", "VmRSS:", true), None);
        assert_eq!(
            field("VmRSS: 18446744073709551615 kB", "VmRSS:", true),
            None
        );
        assert!(
            memory("VmRSS: 5 kB", "Private_Clean: 1 kB")
                .private_resident_bytes
                .is_none()
        );
    }
}
