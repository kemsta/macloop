#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AudioSourceType {
    Microphone, // The primary stream (Voice)
    System,     // The reference stream (Context/Echo)
}

/// Universal audio frame for entire processing pipeline
/// Can represent raw capture data, processed frames, resampled output, etc.
#[derive(Debug, Clone)]
pub struct AudioFrame {
    pub source: AudioSourceType,
    pub samples: Vec<f32>,     // Audio data (interleaved or mono)
    pub sample_rate: u32,      // Current sample rate (may change in pipeline)
    pub channels: u16,         // Current channel count (may change in pipeline)
    pub timestamp: u64,        // Presentation timestamp in nanoseconds
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_frame_clone_preserves_fields() {
        let f = AudioFrame {
            source: AudioSourceType::System,
            samples: vec![0.1, -0.2],
            sample_rate: 48_000,
            channels: 2,
            timestamp: 123,
        };
        let c = f.clone();
        assert_eq!(c.source, AudioSourceType::System);
        assert_eq!(c.samples, vec![0.1, -0.2]);
        assert_eq!(c.sample_rate, 48_000);
        assert_eq!(c.channels, 2);
        assert_eq!(c.timestamp, 123);
    }
}
