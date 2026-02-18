use pyo3::prelude::*;
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
    pub gil_acquire_failures: u64,

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

#[pyclass]
pub struct PipelineStats {
    #[pyo3(get)]
    pub frames_in_mic: u64,
    #[pyo3(get)]
    pub frames_in_system: u64,
    #[pyo3(get)]
    pub frames_out_mic: u64,
    #[pyo3(get)]
    pub frames_out_system: u64,
    #[pyo3(get)]
    pub processor_errors: u64,
    #[pyo3(get)]
    pub processor_drain_errors: u64,
    #[pyo3(get)]
    pub callback_errors: u64,
    #[pyo3(get)]
    pub gil_acquire_failures: u64,

    #[pyo3(get)]
    pub timestamp_avg_ms: f64,
    #[pyo3(get)]
    pub timestamp_max_ms: f64,
    #[pyo3(get)]
    pub webrtc_resample_avg_ms: f64,
    #[pyo3(get)]
    pub webrtc_resample_max_ms: f64,
    #[pyo3(get)]
    pub quantizer_avg_ms: f64,
    #[pyo3(get)]
    pub quantizer_max_ms: f64,
    #[pyo3(get)]
    pub aec_avg_ms: f64,
    #[pyo3(get)]
    pub aec_max_ms: f64,
    #[pyo3(get)]
    pub ns_avg_ms: f64,
    #[pyo3(get)]
    pub ns_max_ms: f64,
    #[pyo3(get)]
    pub processing_avg_ms: f64,
    #[pyo3(get)]
    pub processing_max_ms: f64,
    #[pyo3(get)]
    pub total_pipeline_avg_ms: f64,
    #[pyo3(get)]
    pub total_pipeline_max_ms: f64,

    #[pyo3(get)]
    pub aec_tune_enabled: bool,
    #[pyo3(get)]
    pub aec_tune_frozen: bool,
    #[pyo3(get)]
    pub aec_applied_delay_ms: i32,
    #[pyo3(get)]
    pub aec_best_delay_ms: i32,
    #[pyo3(get)]
    pub aec_step_ms: i32,
    #[pyo3(get)]
    pub aec_direction: i32,
    #[pyo3(get)]
    pub aec_interval_frames: u64,
    #[pyo3(get)]
    pub aec_max_delay_ms: i32,
    #[pyo3(get)]
    pub aec_last_erle: Option<f64>,
    #[pyo3(get)]
    pub aec_erle_ema: Option<f64>,
    #[pyo3(get)]
    pub aec_best_erle: Option<f64>,
    #[pyo3(get)]
    pub aec_last_apm_delay_ms: Option<u32>,
    #[pyo3(get)]
    pub aec_tune_events: u64,
    #[pyo3(get)]
    pub aec_rollback_events: u64,
    #[pyo3(get)]
    pub aec_freeze_events: u64,
    #[pyo3(get)]
    pub aec_skipped_inactive_mic: u64,
    #[pyo3(get)]
    pub aec_skipped_inactive_system: u64,
}

impl PipelineStats {
    pub fn from_runtime(s: RuntimeStats) -> Self {
        Self {
            frames_in_mic: s.frames_in_mic,
            frames_in_system: s.frames_in_system,
            frames_out_mic: s.frames_out_mic,
            frames_out_system: s.frames_out_system,
            processor_errors: s.processor_errors,
            processor_drain_errors: s.processor_drain_errors,
            callback_errors: s.callback_errors,
            gil_acquire_failures: s.gil_acquire_failures,

            timestamp_avg_ms: s.timestamp_processor.avg_ns() / 1_000_000.0,
            timestamp_max_ms: s.timestamp_processor.max_ns as f64 / 1_000_000.0,
            webrtc_resample_avg_ms: s.webrtc_resample_processor.avg_ns() / 1_000_000.0,
            webrtc_resample_max_ms: s.webrtc_resample_processor.max_ns as f64 / 1_000_000.0,
            quantizer_avg_ms: s.quantizer_processor.avg_ns() / 1_000_000.0,
            quantizer_max_ms: s.quantizer_processor.max_ns as f64 / 1_000_000.0,
            aec_avg_ms: s.aec_processor.avg_ns() / 1_000_000.0,
            aec_max_ms: s.aec_processor.max_ns as f64 / 1_000_000.0,
            ns_avg_ms: s.ns_processor.avg_ns() / 1_000_000.0,
            ns_max_ms: s.ns_processor.max_ns as f64 / 1_000_000.0,
            processing_avg_ms: s.processing_time.avg_ns() / 1_000_000.0,
            processing_max_ms: s.processing_time.max_ns as f64 / 1_000_000.0,
            total_pipeline_avg_ms: s.total_pipeline.avg_ns() / 1_000_000.0,
            total_pipeline_max_ms: s.total_pipeline.max_ns as f64 / 1_000_000.0,

            aec_tune_enabled: s.aec_tuner.enabled,
            aec_tune_frozen: s.aec_tuner.frozen,
            aec_applied_delay_ms: s.aec_tuner.applied_delay_ms,
            aec_best_delay_ms: s.aec_tuner.best_delay_ms,
            aec_step_ms: s.aec_tuner.step_ms,
            aec_direction: s.aec_tuner.direction,
            aec_interval_frames: s.aec_tuner.interval_frames,
            aec_max_delay_ms: s.aec_tuner.max_delay_ms,
            aec_last_erle: s.aec_tuner.last_erle,
            aec_erle_ema: s.aec_tuner.erle_ema,
            aec_best_erle: s.aec_tuner.best_erle,
            aec_last_apm_delay_ms: s.aec_tuner.last_apm_delay_ms,
            aec_tune_events: s.aec_tuner.tune_events,
            aec_rollback_events: s.aec_tuner.rollback_events,
            aec_freeze_events: s.aec_tuner.freeze_events,
            aec_skipped_inactive_mic: s.aec_tuner.skipped_inactive_mic,
            aec_skipped_inactive_system: s.aec_tuner.skipped_inactive_system,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_updates_and_snapshot() {
        let h = RuntimeStatsHandle::new();
        h.update(|s| {
            s.frames_in_mic += 2;
            s.processing_time.record(1_000_000);
        });
        let snap = h.snapshot();
        assert_eq!(snap.frames_in_mic, 2);
        assert_eq!(snap.processing_time.samples, 1);
        assert_eq!(snap.processing_time.max_ns, 1_000_000);
    }

    #[test]
    fn pipeline_stats_conversion_uses_ms_units() {
        let mut r = RuntimeStats::default();
        r.processing_time.record(2_000_000);
        r.total_pipeline.record(5_000_000);
        r.aec_tuner.applied_delay_ms = 42;
        let p = PipelineStats::from_runtime(r);

        assert_eq!(p.processing_avg_ms, 2.0);
        assert_eq!(p.total_pipeline_max_ms, 5.0);
        assert_eq!(p.aec_applied_delay_ms, 42);
    }
}
