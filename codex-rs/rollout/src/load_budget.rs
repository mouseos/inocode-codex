//! Memory guard for reading a rollout into memory.
//!
//! A rollout can be tens of GB. Anything that would hold it whole must stop with a clear error
//! rather than exhaust RAM. Memory held by decoded items has been measured at 1.2x to 3.5x the
//! bytes read, so the guard counts three times the bytes against the budget.

use std::io;

const MEMORY_FACTOR: u64 = 3;
const FALLBACK_BUDGET: u64 = 2 * 1024 * 1024 * 1024;

/// The most memory one rollout load may hold, in bytes: `rollout.max_load_bytes`, or a quarter of
/// physical memory when it is 0.
pub fn max_load_bytes() -> u64 {
    let configured = codex_inocode_flags::int_param("rollout.max_load_bytes");
    if configured > 0 {
        return configured;
    }
    let Ok(mut system) = sysinfo::System::new() else {
        return FALLBACK_BUDGET;
    };
    system.refresh_memory();
    match system.total_memory() {
        0 => FALLBACK_BUDGET,
        total => total / 4,
    }
}

/// Fails when holding `bytes_read` bytes of decoded rollout would exceed `budget`.
pub fn check_budget(bytes_read: u64, budget: u64) -> io::Result<()> {
    if bytes_read.saturating_mul(MEMORY_FACTOR) > budget {
        return Err(io::Error::other(format!(
            "このセッションは大きすぎます(読み込み {} MB が予算 {} MB を超えます)。\
             `inocode/sessions/slim` で軽くするか、分割してください。\
             / session too large to load: {} MB read exceeds the {} MB budget",
            bytes_read >> 20,
            budget >> 20,
            bytes_read >> 20,
            budget >> 20,
        )));
    }
    Ok(())
}

/// Rollouts bigger than this are never read whole: `rollout.stream_threshold_bytes`.
pub fn stream_threshold_bytes() -> u64 {
    match codex_inocode_flags::int_param("rollout.stream_threshold_bytes") {
        0 => u64::MAX,
        bytes => bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_counts_three_times_the_bytes() {
        assert!(check_budget(100, 300).is_ok());
        assert!(check_budget(101, 300).is_err());
    }
}
