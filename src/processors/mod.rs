use crate::messages::AudioFrame;
use anyhow::Result;

// Sub-modules
pub mod timestamp;
pub mod resample;
pub mod aec;
pub mod noise_suppression;
pub mod quantizer;

// Re-exports
pub use timestamp::TimestampNormalizer;
pub use resample::ResampleProcessor;
pub use aec::AecProcessor;
pub use noise_suppression::NoiseSuppressionProcessor;
pub use quantizer::FrameQuantizer;

/// Trait for all audio processors in the pipeline
pub trait AudioProcessor: Send {
    /// Process a single audio frame
    /// Returns processed frame or None if more input is needed
    fn process(&mut self, frame: AudioFrame) -> Result<Option<AudioFrame>>;

    /// Return additional ready frames produced from previously buffered input.
    /// Processors that are strictly 1:1 can keep the default implementation.
    fn drain_ready(&mut self) -> Result<Option<AudioFrame>> {
        Ok(None)
    }
    
    /// Flush any remaining buffered data
    fn flush(&mut self) -> Vec<AudioFrame>;
    
    /// Reset processor state
    fn reset(&mut self);
}

/// Processor that passes frames through unchanged (for testing/debugging)
pub struct PassthroughProcessor;

impl AudioProcessor for PassthroughProcessor {
    fn process(&mut self, frame: AudioFrame) -> Result<Option<AudioFrame>> {
        Ok(Some(frame))
    }
    
    fn flush(&mut self) -> Vec<AudioFrame> {
        Vec::new()
    }
    
    fn reset(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::AudioSourceType;

    fn frame() -> AudioFrame {
        AudioFrame {
            source: AudioSourceType::Microphone,
            samples: vec![0.1, 0.2],
            sample_rate: 48_000,
            channels: 1,
            timestamp: 1,
        }
    }

    #[test]
    fn passthrough_returns_same_frame() {
        let mut p = PassthroughProcessor;
        let input = frame();
        let out = p.process(input.clone()).unwrap().unwrap();
        assert_eq!(out.samples, input.samples);
        assert_eq!(out.timestamp, input.timestamp);
    }

    #[test]
    fn default_drain_ready_is_none() {
        let mut p = PassthroughProcessor;
        assert!(p.drain_ready().unwrap().is_none());
    }
}
