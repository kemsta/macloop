use pyo3::prelude::*;

#[pyclass(from_py_object)]
#[derive(Clone, Debug)]
pub struct AudioProcessingConfig {
    #[pyo3(get, set)]
    pub sample_rate: u32,
    #[pyo3(get, set)]
    pub channels: u16,
    #[pyo3(get, set)]
    pub enable_aec: bool,
    #[pyo3(get, set)]
    pub enable_ns: bool,
    #[pyo3(get, set)]
    pub sample_format: String, // "f32" or "i16"
    #[pyo3(get, set)]
    pub aec_stream_delay_ms: i32, // Manual delay adjustment (positive = system ahead of mic)
    #[pyo3(get, set)]
    pub aec_auto_delay_tuning: bool, // Auto-tune stream delay from observed mic/system timestamp delta
    #[pyo3(get, set)]
    pub aec_max_delay_ms: i32, // Upper bound for auto delay tuning in stream mode
}

#[pymethods]
impl AudioProcessingConfig {
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
        aec_max_delay_ms: i32
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

    /// Calculate optimal AEC delay based on system latency measurements
    #[pyo3(name = "calibrate_delay")]
    fn calibrate_delay(&mut self, measured_system_latency_ms: f32, measured_mic_latency_ms: f32) {
        // Simple calibration: system delay minus microphone delay
        self.aec_stream_delay_ms = (measured_system_latency_ms - measured_mic_latency_ms) as i32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructor_sets_fields() {
        let c = AudioProcessingConfig::new(
            48_000,
            2,
            true,
            false,
            "i16".to_string(),
            12,
            false,
            250,
        );

        assert_eq!(c.sample_rate, 48_000);
        assert_eq!(c.channels, 2);
        assert!(c.enable_aec);
        assert!(!c.enable_ns);
        assert_eq!(c.sample_format, "i16");
        assert_eq!(c.aec_stream_delay_ms, 12);
        assert!(!c.aec_auto_delay_tuning);
        assert_eq!(c.aec_max_delay_ms, 250);
    }

    #[test]
    fn calibrate_delay_updates_stream_delay() {
        let mut c = AudioProcessingConfig::new(
            16_000,
            1,
            true,
            true,
            "f32".to_string(),
            0,
            true,
            140,
        );
        c.calibrate_delay(42.5, 10.0);
        assert_eq!(c.aec_stream_delay_ms, 32);
    }
}
