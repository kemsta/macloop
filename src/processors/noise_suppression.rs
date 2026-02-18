use webrtc_audio_processing::{Processor, Config};
use webrtc_audio_processing::config::{NoiseSuppressionLevel, NoiseSuppression, HighPassFilter};
use crate::messages::AudioFrame;
use crate::config::AudioProcessingConfig;
use super::AudioProcessor;
use anyhow::Result;

/// Noise Suppression processor for reducing background noise
pub struct NoiseSuppressionProcessor {
    apm: Option<Processor>,
    config: AudioProcessingConfig,
}

impl NoiseSuppressionProcessor {
    pub fn new(config: AudioProcessingConfig) -> Self {
        let apm = if config.enable_ns {
            Some(Self::create_ns_apm(&config))
        } else {
            None
        };
        
        Self {
            apm,
            config,
        }
    }
    
    fn create_ns_apm(_config: &AudioProcessingConfig) -> Processor {
        let apm = Processor::new(48_000).expect("Failed to create WebRTC Processor for Noise Suppression");

        let mut apm_config = Config::default();
        
        // Enable High Pass Filter for better NS performance
        apm_config.high_pass_filter = Some(HighPassFilter::default());
        
        // Configure Noise Suppression
        apm_config.noise_suppression = Some(NoiseSuppression {
            level: NoiseSuppressionLevel::High,
            analyze_linear_aec_output: false,
        });
        
        // Disable other features for pure NS processing
        apm_config.echo_canceller = None;
        apm_config.gain_controller = None;

        apm.set_config(apm_config);
        apm
    }
}

impl AudioProcessor for NoiseSuppressionProcessor {
    fn process(&mut self, mut frame: AudioFrame) -> Result<Option<AudioFrame>> {
        if let Some(apm) = &mut self.apm {
            // Validate frame format
            let expected = apm.num_samples_per_frame();
            if frame.samples.len() != expected {
                eprintln!("Warning: NS frame size mismatch. Expected {}, got {}", 
                    expected, frame.samples.len());
                return Ok(Some(frame));
            }
            
            if frame.sample_rate != 48000 || frame.channels != 1 {
                eprintln!("Warning: NS expects 48kHz mono, got {}Hz {}ch", 
                    frame.sample_rate, frame.channels);
                return Ok(Some(frame));
            }
            
            // Process with noise suppression (no render frame needed for NS)
            match apm.process_capture_frame([frame.samples.as_mut_slice()]) {
                Ok(()) => {
                    // Successfully processed
                }
                Err(e) => {
                    eprintln!("Warning: NS processing error: {}", e);
                    // Return unprocessed frame on error
                }
            }
        }
        
        Ok(Some(frame))
    }
    
    fn flush(&mut self) -> Vec<AudioFrame> {
        // Noise suppression is stateless, no frames to flush
        Vec::new()
    }
    
    fn reset(&mut self) {
        // Reset NS processor state if needed
        if self.config.enable_ns {
            self.apm = Some(Self::create_ns_apm(&self.config));
        } else {
            self.apm = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::AudioSourceType;

    fn config(enable_ns: bool) -> AudioProcessingConfig {
        AudioProcessingConfig {
            sample_rate: 16_000,
            channels: 1,
            enable_aec: false,
            enable_ns,
            sample_format: "f32".to_string(),
            aec_stream_delay_ms: 0,
            aec_auto_delay_tuning: false,
            aec_max_delay_ms: 140,
        }
    }

    fn frame(samples: usize, rate: u32, ch: u16) -> AudioFrame {
        AudioFrame {
            source: AudioSourceType::Microphone,
            samples: vec![0.1; samples],
            sample_rate: rate,
            channels: ch,
            timestamp: 0,
        }
    }

    #[test]
    fn disabled_ns_passes_audio_through() {
        let mut ns = NoiseSuppressionProcessor::new(config(false));
        let input = frame(100, 16_000, 1);
        let out = ns.process(input.clone()).unwrap().unwrap();
        assert_eq!(out.samples, input.samples);
        assert_eq!(out.sample_rate, input.sample_rate);
    }

    #[test]
    fn enabled_ns_keeps_frame_on_invalid_format() {
        let mut ns = NoiseSuppressionProcessor::new(config(true));
        // Wrong format for WebRTC NS (expects 48kHz mono frame size).
        let input = frame(160, 16_000, 1);
        let out = ns.process(input.clone()).unwrap().unwrap();
        assert_eq!(out.samples.len(), input.samples.len());
        assert_eq!(out.sample_rate, input.sample_rate);
        assert_eq!(out.channels, input.channels);
    }

    #[test]
    fn reset_recreates_processor_state() {
        let mut ns = NoiseSuppressionProcessor::new(config(true));
        assert!(ns.apm.is_some());
        ns.reset();
        assert!(ns.apm.is_some());

        let mut ns_disabled = NoiseSuppressionProcessor::new(config(false));
        assert!(ns_disabled.apm.is_none());
        ns_disabled.reset();
        assert!(ns_disabled.apm.is_none());
    }
}
