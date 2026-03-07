use core_engine::config::AudioProcessingConfig as CoreConfig;
use pyo3::prelude::*;

#[pyclass(name = "AudioProcessingConfig", module = "macloop._macloop", from_py_object)]
#[derive(Clone, Debug)]
pub struct PyAudioProcessingConfig {
    #[pyo3(get, set)]
    pub sample_rate: u32,
    #[pyo3(get, set)]
    pub channels: u16,
    #[pyo3(get, set)]
    pub enable_aec: bool,
    #[pyo3(get, set)]
    pub enable_ns: bool,
    #[pyo3(get, set)]
    pub sample_format: String,
    #[pyo3(get, set)]
    pub aec_stream_delay_ms: i32,
    #[pyo3(get, set)]
    pub aec_auto_delay_tuning: bool,
    #[pyo3(get, set)]
    pub aec_max_delay_ms: i32,
}

impl Default for PyAudioProcessingConfig {
    fn default() -> Self {
        let cfg = CoreConfig::default();
        Self {
            sample_rate: cfg.sample_rate,
            channels: cfg.channels,
            enable_aec: cfg.enable_aec,
            enable_ns: cfg.enable_ns,
            sample_format: cfg.sample_format,
            aec_stream_delay_ms: cfg.aec_stream_delay_ms,
            aec_auto_delay_tuning: cfg.aec_auto_delay_tuning,
            aec_max_delay_ms: cfg.aec_max_delay_ms,
        }
    }
}

impl From<PyAudioProcessingConfig> for CoreConfig {
    fn from(value: PyAudioProcessingConfig) -> Self {
        Self {
            sample_rate: value.sample_rate,
            channels: value.channels,
            enable_aec: value.enable_aec,
            enable_ns: value.enable_ns,
            sample_format: value.sample_format,
            aec_stream_delay_ms: value.aec_stream_delay_ms,
            aec_auto_delay_tuning: value.aec_auto_delay_tuning,
            aec_max_delay_ms: value.aec_max_delay_ms,
        }
    }
}

impl From<&PyAudioProcessingConfig> for CoreConfig {
    fn from(value: &PyAudioProcessingConfig) -> Self {
        Self {
            sample_rate: value.sample_rate,
            channels: value.channels,
            enable_aec: value.enable_aec,
            enable_ns: value.enable_ns,
            sample_format: value.sample_format.clone(),
            aec_stream_delay_ms: value.aec_stream_delay_ms,
            aec_auto_delay_tuning: value.aec_auto_delay_tuning,
            aec_max_delay_ms: value.aec_max_delay_ms,
        }
    }
}

#[pymethods]
impl PyAudioProcessingConfig {
    #[new]
    #[pyo3(signature = (
        sample_rate=48000,
        channels=2,
        enable_aec=false,
        enable_ns=false,
        sample_format="f32".to_string(),
        aec_stream_delay_ms=0,
        aec_auto_delay_tuning=false,
        aec_max_delay_ms=140
    ))]
    fn new(
        sample_rate: u32,
        channels: u16,
        enable_aec: bool,
        enable_ns: bool,
        sample_format: String,
        aec_stream_delay_ms: i32,
        aec_auto_delay_tuning: bool,
        aec_max_delay_ms: i32,
    ) -> Self {
        Self {
            sample_rate,
            channels,
            enable_aec,
            enable_ns,
            sample_format,
            aec_stream_delay_ms,
            aec_auto_delay_tuning,
            aec_max_delay_ms,
        }
    }

    #[pyo3(name = "calibrate_delay")]
    fn calibrate_delay(&mut self, measured_system_latency_ms: f32, measured_mic_latency_ms: f32) {
        let mut core_cfg = CoreConfig::from(self.clone());
        core_cfg.calibrate_delay(measured_system_latency_ms, measured_mic_latency_ms);
        self.aec_stream_delay_ms = core_cfg.aec_stream_delay_ms;
    }
}
