//! Merge admission against the process memory budget (#1122).
//!
//! An 8 GiB host used to take the 8 GiB AUTO step, so the process budget
//! was the whole machine. A segment-merge storm then allocated a working
//! set measured at 5–9× the input bytes and the process died. These
//! functions size and defer batches from the resolved budget. They do
//! not allocate.
//!
//! A budget whose 95% watermark still covers a historical 512 MiB batch
//! (8× working set plus the 2 GiB memtable floor) keeps that batch and
//! the configured parallelism. That is the 8 GiB tier and above — a
//! 16 GiB host and larger — so those machines merge as they did before.

/// Historical input ceiling for one merge batch. The AUTO path used this
/// whenever `XERJ_MAX_PROCESS_MEMORY_MB` was unset.
pub const HISTORICAL_BATCH_CAP_BYTES: u64 = 512 * 1024 * 1024;

/// Smallest input cap. Below this a tier can stop making progress.
pub const MIN_BATCH_CAP_BYTES: u64 = 32 * 1024 * 1024;

/// Planning multiplier for one batch's heap. Mailbox-shaped merges were
/// measured at 5–9× the input bytes (`merge_parsed` held 191 MB on a
/// 20k-doc index). 8× is the figure a batch must fit under.
pub const MERGE_WORKING_SET_NUMERATOR: u64 = 8;

/// Upper bound on the input bytes of one merge batch.
///
/// `budget_bytes == 0` means the budget is unknown (no governor). That
/// keeps the historical 512 MiB ceiling. A positive budget that can hold
/// the historical batch also keeps 512 MiB. A smaller budget uses
/// `budget / 64`, clamped to [`MIN_BATCH_CAP_BYTES`]..=[`HISTORICAL_BATCH_CAP_BYTES`].
/// A 4 GiB budget — the AUTO cap on an 8 GiB host — is 64 MiB.
pub fn merge_batch_input_cap_bytes(budget_bytes: u64) -> u64 {
    if budget_bytes == 0 || historical_batch_fits(budget_bytes) {
        return HISTORICAL_BATCH_CAP_BYTES;
    }
    (budget_bytes / 64).clamp(MIN_BATCH_CAP_BYTES, HISTORICAL_BATCH_CAP_BYTES)
}

/// `true` when a 512 MiB batch plus the 2 GiB memtable floor still sits
/// under the 95% watermark of `budget_bytes`.
fn historical_batch_fits(budget_bytes: u64) -> bool {
    let historical = HISTORICAL_BATCH_CAP_BYTES
        .saturating_mul(MERGE_WORKING_SET_NUMERATOR)
        .saturating_add(2 * 1024 * 1024 * 1024);
    budget_bytes.saturating_mul(95) / 100 >= historical
}

/// How many merge batches may run at once.
///
/// A budget that still uses the historical 512 MiB batch returns
/// `configured` unchanged (at least 1). A smaller budget limits
/// concurrency so the batches' estimated working sets share a quarter
/// of the budget, and never drops below one.
pub fn merge_parallelism_under_budget(configured: usize, budget_bytes: u64) -> usize {
    let configured = configured.max(1);
    let cap = merge_batch_input_cap_bytes(budget_bytes);
    if cap >= HISTORICAL_BATCH_CAP_BYTES {
        return configured;
    }
    let per = cap.saturating_mul(MERGE_WORKING_SET_NUMERATOR).max(1);
    let slots = usize::try_from((budget_bytes / 4) / per).unwrap_or(usize::MAX);
    configured.min(slots.max(1))
}

/// Whether one batch may start, ignoring the one-batch drain exception.
///
/// `rss_bytes == 0` means RSS is unknown. `watermark_bytes == 0` means
/// the RSS check is off. Either one starts the batch: a guess must not
/// stall merges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeBatchDecision {
    Start,
    Defer,
}

pub fn decide_merge_batch(
    input_bytes: u64,
    rss_bytes: u64,
    watermark_bytes: u64,
) -> MergeBatchDecision {
    if watermark_bytes == 0 || rss_bytes == 0 {
        return MergeBatchDecision::Start;
    }
    let estimate = input_bytes.saturating_mul(MERGE_WORKING_SET_NUMERATOR);
    if rss_bytes.saturating_add(estimate) >= watermark_bytes {
        MergeBatchDecision::Defer
    } else {
        MergeBatchDecision::Start
    }
}

/// Which of `input_bytes` (one entry per selected batch, policy order)
/// starts in this pass.
///
/// On a budget that still uses the 512 MiB batch, every entry starts.
/// On a smaller budget, a batch whose estimated working set would cross
/// the watermark waits for the next check. The first batch still starts:
/// its input is already capped, and a merge is what retires the
/// per-segment caches that hold RSS up. Zero batches would leave that
/// RSS in place.
pub fn throttle_merge_batches(
    input_bytes: &[u64],
    budget_bytes: u64,
    rss_bytes: u64,
    watermark_bytes: u64,
) -> Vec<bool> {
    let small = merge_batch_input_cap_bytes(budget_bytes) < HISTORICAL_BATCH_CAP_BYTES;
    let mut start = Vec::with_capacity(input_bytes.len());
    let mut started = 0usize;
    for bytes in input_bytes {
        if !small {
            start.push(true);
            continue;
        }
        let fits =
            decide_merge_batch(*bytes, rss_bytes, watermark_bytes) == MergeBatchDecision::Start;
        if fits || started == 0 {
            start.push(true);
            started += 1;
        } else {
            start.push(false);
        }
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;

    #[test]
    fn merges_are_throttled_under_a_small_budget() {
        // AUTO cap on an 8 GiB host: half of RAM, the setting that kept
        // the Windows 10 field report up (`XERJ_MAX_PROCESS_MEMORY_MB=4096`).
        let budget = 4 * GIB;
        let cap = merge_batch_input_cap_bytes(budget);
        assert_eq!(
            cap,
            64 * MIB,
            "4 GiB / 64 = 64 MiB, not the 512 MiB default"
        );
        assert_eq!(
            merge_parallelism_under_budget(4, budget),
            2,
            "four workers would hold 4 × 512 MiB; the budget allows two"
        );

        let watermark = budget * 95 / 100;
        // RSS already within 32 MiB of the watermark. One capped batch
        // still runs (it retires segments). The other two wait.
        let rss = watermark - 32 * MIB;
        let plan = throttle_merge_batches(&[cap, cap, cap], budget, rss, watermark);
        assert_eq!(plan, vec![true, false, false]);

        // The same batches with headroom all start. Parallelism is a
        // separate bound, applied by the caller.
        let open = throttle_merge_batches(&[cap, cap, cap], budget, 256 * MIB, watermark);
        assert_eq!(open, vec![true, true, true]);

        // Unknown RSS does not stall the drain.
        assert_eq!(
            decide_merge_batch(cap, 0, watermark),
            MergeBatchDecision::Start
        );
        assert_eq!(decide_merge_batch(cap, rss, 0), MergeBatchDecision::Start);
    }

    #[test]
    fn a_normal_host_keeps_the_historical_batch_and_parallelism() {
        // The 8 GiB tier (16 GiB of RAM and up, under 64 GiB) and the
        // 16 / 32 GiB tiers all cover a 512 MiB batch under the watermark.
        for budget in [8 * GIB, 16 * GIB, 32 * GIB] {
            assert_eq!(
                merge_batch_input_cap_bytes(budget),
                HISTORICAL_BATCH_CAP_BYTES
            );
            assert_eq!(merge_parallelism_under_budget(4, budget), 4);
            let watermark = budget * 95 / 100;
            let plan = throttle_merge_batches(
                &[HISTORICAL_BATCH_CAP_BYTES; 3],
                budget,
                watermark,
                watermark,
            );
            assert_eq!(
                plan,
                vec![true, true, true],
                "budget {budget} must not defer a historical batch"
            );
        }
        // No governor: the pre-change fallback.
        assert_eq!(merge_batch_input_cap_bytes(0), HISTORICAL_BATCH_CAP_BYTES);
        assert_eq!(merge_parallelism_under_budget(1, 0), 1);
    }

    #[test]
    fn the_batch_cap_floors_at_thirty_two_mebibytes() {
        assert_eq!(merge_batch_input_cap_bytes(GIB), MIN_BATCH_CAP_BYTES);
        assert_eq!(merge_batch_input_cap_bytes(2 * GIB), MIN_BATCH_CAP_BYTES);
    }
}
