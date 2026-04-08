#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    F32,
    I16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: SampleFormat,
}

impl StreamFormat {
    pub const fn new(sample_rate: u32, channels: u16) -> Self {
        Self {
            sample_rate,
            channels,
            sample_format: SampleFormat::F32,
        }
    }

    pub const fn with_sample_format(
        sample_rate: u32,
        channels: u16,
        sample_format: SampleFormat,
    ) -> Self {
        Self {
            sample_rate,
            channels,
            sample_format,
        }
    }
}

pub const MASTER_FORMAT: StreamFormat = StreamFormat {
    sample_rate: 48_000,
    channels: 2,
    sample_format: SampleFormat::F32,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_format_new_defaults_to_f32() {
        let format = StreamFormat::new(16_000, 1);
        assert_eq!(format.sample_rate, 16_000);
        assert_eq!(format.channels, 1);
        assert_eq!(format.sample_format, SampleFormat::F32);
    }

    #[test]
    fn stream_format_with_sample_format_uses_provided_format() {
        let format = StreamFormat::with_sample_format(44_100, 2, SampleFormat::I16);
        assert_eq!(format.sample_rate, 44_100);
        assert_eq!(format.channels, 2);
        assert_eq!(format.sample_format, SampleFormat::I16);
    }

    #[test]
    fn master_format_matches_expected_defaults() {
        assert_eq!(MASTER_FORMAT.sample_rate, 48_000);
        assert_eq!(MASTER_FORMAT.channels, 2);
        assert_eq!(MASTER_FORMAT.sample_format, SampleFormat::F32);
    }
}
