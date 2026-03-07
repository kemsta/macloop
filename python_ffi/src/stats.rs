use core_engine::stats::RuntimeStats;
use pyo3::prelude::*;

#[pyclass(name = "PipelineStats", module = "macloop._macloop")]
pub struct PyPipelineStats {
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

impl PyPipelineStats {
    pub fn from_runtime(s: RuntimeStats, gil_acquire_failures: u64) -> Self {
        Self {
            frames_in_mic: s.frames_in_mic,
            frames_in_system: s.frames_in_system,
            frames_out_mic: s.frames_out_mic,
            frames_out_system: s.frames_out_system,
            processor_errors: s.processor_errors,
            processor_drain_errors: s.processor_drain_errors,
            callback_errors: s.callback_errors,
            gil_acquire_failures,

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
