//! Process memory as reported by Linux, split into anonymous (heap/stack),
//! file-backed (e.g. the timetable mapping, reclaimable page cache) and swap.
use serde::Serialize;

#[derive(Clone, Debug, Default, Serialize)]
pub struct ProcessMemory {
    pub rss_kib: u64,
    pub rss_anon_kib: u64,
    pub rss_file_kib: u64,
    pub hwm_kib: u64,
    pub swap_kib: u64,
    /// From smaps_rollup (proportional set size), if readable.
    pub pss_kib: Option<u64>,
    pub pss_anon_kib: Option<u64>,
    pub pss_file_kib: Option<u64>,
}

impl ProcessMemory {
    pub fn current() -> Option<Self> {
        Self::of_pid("self")
    }

    pub fn of_pid(pid: &str) -> Option<Self> {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let rollup =
            std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).unwrap_or_default();
        let field = |text: &str, key: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
                .and_then(|v| v.trim().trim_end_matches(" kB").trim().parse().ok())
        };
        Some(Self {
            rss_kib: field(&status, "VmRSS")?,
            rss_anon_kib: field(&status, "RssAnon").unwrap_or(0),
            rss_file_kib: field(&status, "RssFile").unwrap_or(0),
            hwm_kib: field(&status, "VmHWM").unwrap_or(0),
            swap_kib: field(&status, "VmSwap").unwrap_or(0),
            pss_kib: field(&rollup, "Pss"),
            pss_anon_kib: field(&rollup, "Pss_Anon"),
            pss_file_kib: field(&rollup, "Pss_File"),
        })
    }
}
