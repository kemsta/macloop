use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Default)]
pub struct StageStats {
    pub samples: u64,
    pub total_ns: u128,
    pub max_ns: u64,
}

impl StageStats {
    pub fn record(&mut self, duration_ns: u64) {
        self.samples += 1;
        self.total_ns += duration_ns as u128;
        self.max_ns = self.max_ns.max(duration_ns);
    }

    pub fn avg_ns(&self) -> f64 {
        if self.samples == 0 {
            0.0
        } else {
            self.total_ns as f64 / self.samples as f64
        }
    }
}

#[derive(Clone, Debug)]
pub struct AecAutoTuneStats {
    pub enabled: bool,
    pub frozen: bool,
    pub applied_delay_ms: i32,
    pub best_delay_ms: i32,
    pub step_ms: i32,
    pub direction: i32,
    pub interval_frames: u64,
    pub max_delay_ms: i32,
    pub last_erle: Option<f64>,
    pub erle_ema: Option<f64>,
    pub best_erle: Option<f64>,
    pub last_apm_delay_ms: Option<u32>,
    pub tune_events: u64,
    pub rollback_events: u64,
    pub freeze_events: u64,
    pub skipped_inactive_mic: u64,
    pub skipped_inactive_system: u64,
}

impl Default for AecAutoTuneStats {
    fn default() -> Self {
        Self {
            enabled: false,
            frozen: false,
            applied_delay_ms: 0,
            best_delay_ms: 0,
            step_ms: 0,
            direction: 1,
            interval_frames: 0,
            max_delay_ms: 0,
            last_erle: None,
            erle_ema: None,
            best_erle: None,
            last_apm_delay_ms: None,
            tune_events: 0,
            rollback_events: 0,
            freeze_events: 0,
            skipped_inactive_mic: 0,
            skipped_inactive_system: 0,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RuntimeStats {
    pub frames_in_mic: u64,
    pub frames_in_system: u64,
    pub frames_out_mic: u64,
    pub frames_out_system: u64,
    pub processor_errors: u64,
    pub processor_drain_errors: u64,
    pub callback_errors: u64,

    pub timestamp_processor: StageStats,
    pub webrtc_resample_processor: StageStats,
    pub quantizer_processor: StageStats,
    pub aec_processor: StageStats,
    pub ns_processor: StageStats,
    pub processing_time: StageStats,
    pub total_pipeline: StageStats,

    pub aec_tuner: AecAutoTuneStats,
}

#[derive(Clone)]
pub struct RuntimeStatsHandle {
    inner: Arc<Mutex<RuntimeStats>>,
}

impl RuntimeStatsHandle {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(RuntimeStats::default())),
        }
    }

    pub fn reset(&self) {
        if let Ok(mut stats) = self.inner.lock() {
            *stats = RuntimeStats::default();
        }
    }

    pub fn update<F>(&self, update_fn: F)
    where
        F: FnOnce(&mut RuntimeStats),
    {
        if let Ok(mut stats) = self.inner.lock() {
            update_fn(&mut stats);
        }
    }

    pub fn snapshot(&self) -> RuntimeStats {
        if let Ok(stats) = self.inner.lock() {
            stats.clone()
        } else {
            RuntimeStats::default()
        }
    }
}
