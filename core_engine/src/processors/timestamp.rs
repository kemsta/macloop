use crate::messages::{AudioFrame, AudioSourceType};
use super::AudioProcessor;
use anyhow::Result;

/// Processor that normalizes timestamps to have consistent baseline
pub struct TimestampNormalizer {
    first_sys_timestamp: Option<u64>,
    first_mic_timestamp: Option<u64>,
    normalized_start_time: u64,
}

impl TimestampNormalizer {
    pub fn new() -> Self {
        let normalized_start_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
            
        Self {
            first_sys_timestamp: None,
            first_mic_timestamp: None,
            normalized_start_time,
        }
    }
    
    fn normalize_timestamp(&mut self, timestamp: u64, source: AudioSourceType) -> u64 {
        match source {
            AudioSourceType::System => {
                if let Some(first) = self.first_sys_timestamp {
                    self.normalized_start_time + (timestamp.saturating_sub(first))
                } else {
                    self.first_sys_timestamp = Some(timestamp);
                    self.normalized_start_time
                }
            }
            AudioSourceType::Microphone => {
                if let Some(first) = self.first_mic_timestamp {
                    self.normalized_start_time + (timestamp.saturating_sub(first))
                } else {
                    self.first_mic_timestamp = Some(timestamp);
                    self.normalized_start_time
                }
            }
        }
    }
}

impl AudioProcessor for TimestampNormalizer {
    fn process(&mut self, mut frame: AudioFrame) -> Result<Option<AudioFrame>> {
        frame.timestamp = self.normalize_timestamp(frame.timestamp, frame.source);
        Ok(Some(frame))
    }
    
    fn flush(&mut self) -> Vec<AudioFrame> {
        Vec::new()
    }
    
    fn reset(&mut self) {
        self.first_sys_timestamp = None;
        self.first_mic_timestamp = None;
        self.normalized_start_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(source: AudioSourceType, timestamp: u64) -> AudioFrame {
        AudioFrame {
            source,
            samples: vec![0.0; 4],
            sample_rate: 48_000,
            channels: 1,
            timestamp,
        }
    }

    #[test]
    fn normalizes_each_source_with_independent_baseline() {
        let mut p = TimestampNormalizer::new();
        let sys0 = p.process(frame(AudioSourceType::System, 1_000)).unwrap().unwrap();
        let sys1 = p.process(frame(AudioSourceType::System, 1_500)).unwrap().unwrap();
        let mic0 = p.process(frame(AudioSourceType::Microphone, 5_000)).unwrap().unwrap();
        let mic1 = p.process(frame(AudioSourceType::Microphone, 5_700)).unwrap().unwrap();

        assert_eq!(sys1.timestamp.saturating_sub(sys0.timestamp), 500);
        assert_eq!(mic1.timestamp.saturating_sub(mic0.timestamp), 700);
        assert_eq!(sys0.timestamp, mic0.timestamp);
    }

    #[test]
    fn reset_restarts_normalization_epoch() {
        let mut p = TimestampNormalizer::new();
        let before_reset = p.process(frame(AudioSourceType::System, 10_000)).unwrap().unwrap();
        p.reset();
        let after_reset = p.process(frame(AudioSourceType::System, 20_000)).unwrap().unwrap();

        assert!(after_reset.timestamp >= before_reset.timestamp);
    }
}
