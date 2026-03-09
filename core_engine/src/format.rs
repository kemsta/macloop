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
