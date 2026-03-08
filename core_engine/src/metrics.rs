use std::array;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use crate::processor::{NodeId, StreamId};

pub const LATENCY_BUCKET_BOUNDS_US: [u32; 24] = [
    1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1_024, 2_048, 4_096, 8_192, 16_384, 32_768, 65_536,
    131_072, 262_144, 524_288, 1_048_576, 2_097_152, 4_194_304, 8_388_608,
];
pub const LATENCY_WINDOW_SLOT_SECS: u64 = 1;
pub const LATENCY_WINDOW_SLOTS: usize = 60;
pub const LATENCY_WINDOW_SECS: u64 = LATENCY_WINDOW_SLOT_SECS * LATENCY_WINDOW_SLOTS as u64;

const INVALID_EPOCH: u64 = u64::MAX;

struct LatencyHistogramSlot {
    epoch: AtomicU64,
    max_us: AtomicU32,
    count: AtomicU64,
    buckets: [AtomicU64; LATENCY_BUCKET_BOUNDS_US.len()],
}

impl Default for LatencyHistogramSlot {
    fn default() -> Self {
        Self {
            epoch: AtomicU64::new(INVALID_EPOCH),
            max_us: AtomicU32::new(0),
            count: AtomicU64::new(0),
            buckets: array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

pub struct LatencyHistogram {
    created_at: Instant,
    last_us: AtomicU32,
    slots: [LatencyHistogramSlot; LATENCY_WINDOW_SLOTS],
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self {
            created_at: Instant::now(),
            last_us: AtomicU32::new(0),
            slots: array::from_fn(|_| LatencyHistogramSlot::default()),
        }
    }
}

impl LatencyHistogram {
    pub fn record(&self, elapsed_us: u32) {
        self.last_us.store(elapsed_us, Ordering::Relaxed);
        let epoch = self.current_epoch();
        let slot = &self.slots[slot_index(epoch)];
        let slot_epoch = slot.epoch.load(Ordering::Relaxed);
        if slot_epoch != epoch {
            self.reset_slot(slot, epoch);
        }

        slot.buckets[bucket_index(elapsed_us)].fetch_add(1, Ordering::Relaxed);
        let mut current_max = slot.max_us.load(Ordering::Relaxed);
        while elapsed_us > current_max {
            match slot.max_us.compare_exchange_weak(
                current_max,
                elapsed_us,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current_max = observed,
            }
        }
        slot.count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> LatencyHistogramSnapshot {
        let epoch_now = self.current_epoch();
        let window_start = epoch_now.saturating_sub(LATENCY_WINDOW_SLOTS.saturating_sub(1) as u64);
        let mut count = 0_u64;
        let mut max_us = 0_u32;
        let mut buckets = vec![0_u64; LATENCY_BUCKET_BOUNDS_US.len()];

        for slot in &self.slots {
            let epoch = slot.epoch.load(Ordering::Acquire);
            if epoch == INVALID_EPOCH || epoch < window_start || epoch > epoch_now {
                continue;
            }

            count = count.saturating_add(slot.count.load(Ordering::Relaxed));
            max_us = max_us.max(slot.max_us.load(Ordering::Relaxed));

            for (index, bucket) in slot.buckets.iter().enumerate() {
                buckets[index] = buckets[index].saturating_add(bucket.load(Ordering::Relaxed));
            }
        }

        let bucket_bounds_us = LATENCY_BUCKET_BOUNDS_US.to_vec();

        LatencyHistogramSnapshot {
            last_us: self.last_us.load(Ordering::Relaxed),
            max_us,
            count,
            bucket_bounds_us: bucket_bounds_us.clone(),
            buckets: buckets.clone(),
            p50_us: percentile_from_histogram(&bucket_bounds_us, &buckets, count, 0.50),
            p90_us: percentile_from_histogram(&bucket_bounds_us, &buckets, count, 0.90),
            p95_us: percentile_from_histogram(&bucket_bounds_us, &buckets, count, 0.95),
            p99_us: percentile_from_histogram(&bucket_bounds_us, &buckets, count, 0.99),
        }
    }

    fn current_epoch(&self) -> u64 {
        self.created_at.elapsed().as_secs() / LATENCY_WINDOW_SLOT_SECS
    }

    fn reset_slot(&self, slot: &LatencyHistogramSlot, epoch: u64) {
        for bucket in &slot.buckets {
            bucket.store(0, Ordering::Relaxed);
        }
        slot.max_us.store(0, Ordering::Relaxed);
        slot.count.store(0, Ordering::Relaxed);
        slot.epoch.store(epoch, Ordering::Release);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatencyHistogramSnapshot {
    pub last_us: u32,
    pub max_us: u32,
    pub count: u64,
    pub bucket_bounds_us: Vec<u32>,
    pub buckets: Vec<u64>,
    pub p50_us: u32,
    pub p90_us: u32,
    pub p95_us: u32,
    pub p99_us: u32,
}

impl Default for LatencyHistogramSnapshot {
    fn default() -> Self {
        Self {
            last_us: 0,
            max_us: 0,
            count: 0,
            bucket_bounds_us: LATENCY_BUCKET_BOUNDS_US.to_vec(),
            buckets: vec![0; LATENCY_BUCKET_BOUNDS_US.len()],
            p50_us: 0,
            p90_us: 0,
            p95_us: 0,
            p99_us: 0,
        }
    }
}

pub struct NodeMetrics {
    pub processing_time_us: AtomicU32,
    pub latency: LatencyHistogram,
}

impl Default for NodeMetrics {
    fn default() -> Self {
        Self {
            processing_time_us: AtomicU32::new(0),
            latency: LatencyHistogram::default(),
        }
    }
}

impl NodeMetrics {
    pub fn snapshot(&self) -> NodeMetricsSnapshot {
        let latency = self.latency.snapshot();
        NodeMetricsSnapshot {
            processing_time_us: self.processing_time_us.load(Ordering::Relaxed),
            max_processing_time_us: latency.max_us,
            latency,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NodeMetricsSnapshot {
    pub processing_time_us: u32,
    pub max_processing_time_us: u32,
    pub latency: LatencyHistogramSnapshot,
}

pub struct PipelineMetrics {
    pub total_callback_time_us: AtomicU32,
    pub dropped_frames: AtomicU32,
    pub buffer_size: AtomicU32,
    pub latency: LatencyHistogram,
}

impl Default for PipelineMetrics {
    fn default() -> Self {
        Self {
            total_callback_time_us: AtomicU32::new(0),
            dropped_frames: AtomicU32::new(0),
            buffer_size: AtomicU32::new(0),
            latency: LatencyHistogram::default(),
        }
    }
}

impl PipelineMetrics {
    pub fn snapshot(&self) -> PipelineMetricsSnapshot {
        PipelineMetricsSnapshot {
            total_callback_time_us: self.total_callback_time_us.load(Ordering::Relaxed),
            dropped_frames: self.dropped_frames.load(Ordering::Relaxed),
            buffer_size: self.buffer_size.load(Ordering::Relaxed),
            latency: self.latency.snapshot(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PipelineMetricsSnapshot {
    pub total_callback_time_us: u32,
    pub dropped_frames: u32,
    pub buffer_size: u32,
    pub latency: LatencyHistogramSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StreamMetricsSnapshot {
    pub pipeline: PipelineMetricsSnapshot,
    pub processors: HashMap<NodeId, NodeMetricsSnapshot>,
}

pub type EngineMetricsSnapshot = HashMap<StreamId, StreamMetricsSnapshot>;

fn slot_index(epoch: u64) -> usize {
    (epoch as usize) % LATENCY_WINDOW_SLOTS
}

fn bucket_index(elapsed_us: u32) -> usize {
    let idx = if elapsed_us <= 1 {
        0
    } else {
        (u32::BITS - (elapsed_us - 1).leading_zeros()) as usize
    };
    idx.min(LATENCY_BUCKET_BOUNDS_US.len() - 1)
}

fn percentile_from_histogram(
    bucket_bounds_us: &[u32],
    buckets: &[u64],
    count: u64,
    percentile: f64,
) -> u32 {
    if count == 0 || buckets.is_empty() || bucket_bounds_us.is_empty() {
        return 0;
    }

    let target_rank = ((count as f64) * percentile).ceil() as u64;
    let target_rank = target_rank.max(1);

    let mut cumulative = 0_u64;
    for (index, bucket_count) in buckets.iter().enumerate() {
        cumulative = cumulative.saturating_add(*bucket_count);
        if cumulative >= target_rank {
            return bucket_bounds_us[index];
        }
    }

    *bucket_bounds_us.last().unwrap_or(&0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn histogram_places_values_into_log_buckets() {
        let histogram = LatencyHistogram::default();
        histogram.record(1);
        histogram.record(2);
        histogram.record(3);
        histogram.record(1_000);

        let snapshot = histogram.snapshot();
        assert_eq!(snapshot.count, 4);
        assert_eq!(snapshot.buckets[0], 1);
        assert_eq!(snapshot.buckets[1], 1);
        assert_eq!(snapshot.buckets[2], 1);
        assert_eq!(snapshot.max_us, 1_000);
        assert!(snapshot.p99_us >= snapshot.p50_us);
    }

    #[test]
    fn histogram_exposes_window_configuration() {
        assert_eq!(LATENCY_WINDOW_SLOT_SECS, 1);
        assert_eq!(LATENCY_WINDOW_SLOTS, 60);
        assert_eq!(LATENCY_WINDOW_SECS, 60);
    }

    #[test]
    fn histogram_excludes_stale_slots_outside_window() {
        let mut histogram = LatencyHistogram::default();
        histogram.created_at = Instant::now() - Duration::from_secs(LATENCY_WINDOW_SECS + 1);

        let stale_epoch = 0_u64;
        let current_epoch = histogram.current_epoch();
        let current_slot = &histogram.slots[slot_index(current_epoch)];
        let stale_slot = &histogram.slots[slot_index(stale_epoch)];

        stale_slot.epoch.store(stale_epoch, Ordering::Relaxed);
        stale_slot.count.store(3, Ordering::Relaxed);
        stale_slot.max_us.store(1_000, Ordering::Relaxed);
        stale_slot.buckets[bucket_index(1_000)].store(3, Ordering::Relaxed);

        current_slot.epoch.store(current_epoch, Ordering::Relaxed);
        current_slot.count.store(1, Ordering::Relaxed);
        current_slot.max_us.store(8, Ordering::Relaxed);
        current_slot.buckets[bucket_index(8)].store(1, Ordering::Relaxed);

        let snapshot = histogram.snapshot();
        assert_eq!(snapshot.count, 1);
        assert_eq!(snapshot.max_us, 8);
        assert_eq!(snapshot.buckets[bucket_index(8)], 1);
        assert_eq!(snapshot.buckets[bucket_index(1_000)], 0);
    }
}
