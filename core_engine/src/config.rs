#[derive(Clone, Debug)]
pub struct AudioProcessingConfig {
    pub sample_rate: u32,
    pub channels: u16,
    pub enable_aec: bool,
    pub enable_ns: bool,
    pub sample_format: String,
    pub aec_stream_delay_ms: i32,
    pub aec_auto_delay_tuning: bool,
    pub aec_max_delay_ms: i32,
}

impl Default for AudioProcessingConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            channels: 2,
            enable_aec: false,
            enable_ns: false,
            sample_format: "f32".to_string(),
            aec_stream_delay_ms: 0,
            aec_auto_delay_tuning: false,
            aec_max_delay_ms: 140,
        }
    }
}

impl AudioProcessingConfig {
    pub fn calibrate_delay(&mut self, measured_system_latency_ms: f32, measured_mic_latency_ms: f32) {
        self.aec_stream_delay_ms = (measured_system_latency_ms - measured_mic_latency_ms) as i32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_sets_expected_fields() {
        let c = AudioProcessingConfig::default();
        assert_eq!(c.sample_rate, 48_000);
        assert_eq!(c.channels, 2);
        assert!(!c.enable_aec);
        assert!(!c.enable_ns);
        assert_eq!(c.sample_format, "f32");
        assert_eq!(c.aec_stream_delay_ms, 0);
        assert!(!c.aec_auto_delay_tuning);
        assert_eq!(c.aec_max_delay_ms, 140);
    }

    #[test]
    fn calibrate_delay_updates_stream_delay() {
        let mut c = AudioProcessingConfig::default();
        c.calibrate_delay(42.5, 10.0);
        assert_eq!(c.aec_stream_delay_ms, 32);
    }
}
