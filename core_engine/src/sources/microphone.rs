use crate::converter::{InputConversionError, InputConverter, MasterFormatConverter};
use crate::engine::RealTimePipeline;
use crate::format::{SampleFormat, StreamFormat, MASTER_FORMAT};

const DRIVER_FORMAT: StreamFormat = StreamFormat::with_sample_format(48_000, 1, SampleFormat::F32);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicInfo {
    pub id: u32,
    pub name: String,
    pub is_default: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct MicrophoneSourceConfig {
    pub device_id: Option<u32>,
    pub vpio_enabled: bool,
}

impl Default for MicrophoneSourceConfig {
    fn default() -> Self {
        Self {
            device_id: None,
            vpio_enabled: true,
        }
    }
}

#[derive(Debug)]
pub enum MicrophoneError {
    UnsupportedPlatform,
    InvalidConfig(String),
    Converter(InputConversionError),
    Driver(String),
}

impl std::fmt::Display for MicrophoneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                write!(f, "microphone source is only implemented for macOS")
            }
            Self::InvalidConfig(err) => write!(f, "invalid microphone config: {err}"),
            Self::Converter(err) => write!(f, "converter error: {err}"),
            Self::Driver(err) => write!(f, "driver error: {err}"),
        }
    }
}

impl std::error::Error for MicrophoneError {}

impl From<InputConversionError> for MicrophoneError {
    fn from(value: InputConversionError) -> Self {
        Self::Converter(value)
    }
}

fn validate_microphone_config(config: MicrophoneSourceConfig) -> Result<(), MicrophoneError> {
    if config.vpio_enabled && config.device_id.is_some() {
        return Err(MicrophoneError::InvalidConfig(
            "device_id is not supported when vpio_enabled=true; disable VPIO to select an explicit input device".to_string(),
        ));
    }

    Ok(())
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use coreaudio::audio_unit::audio_format::LinearPcmFlags;
    use coreaudio::audio_unit::macos_helpers::{
        audio_unit_from_device_id, get_audio_device_ids_for_scope, get_default_device_id,
        get_device_name,
    };
    use coreaudio::audio_unit::render_callback::{self, data};
    use coreaudio::audio_unit::{
        AudioUnit, Element, IOType, SampleFormat as CaSampleFormat, Scope,
    };
    use objc2_audio_toolbox::{kAudioOutputUnitProperty_EnableIO, kAudioUnitProperty_StreamFormat};

    type ArgsInterleaved = render_callback::Args<data::Interleaved<f32>>;
    type ArgsNonInterleaved = render_callback::Args<data::NonInterleaved<f32>>;
    const K_AU_VOICE_IO_PROPERTY_BYPASS_VOICE_PROCESSING: u32 = 2100;
    const K_AU_VOICE_IO_PROPERTY_VOICE_PROCESSING_ENABLE_AGC: u32 = 2101;
    const K_AU_VOICE_IO_PROPERTY_MUTE_OUTPUT: u32 = 2104;
    const K_AU_VOICE_IO_PROPERTY_OTHER_AUDIO_DUCKING_CONFIGURATION: u32 = 2108;

    #[repr(C)]
    #[derive(Debug, Clone, Copy, Default)]
    struct AuVoiceIoOtherAudioDuckingConfiguration {
        m_enable_advanced_ducking: u8,
        _pad: [u8; 3],
        m_ducking_level: u32,
    }

    pub struct MicrophoneSource {
        audio_unit: AudioUnit,
        pub input_format: StreamFormat,
        pub output_format: StreamFormat,
    }

    impl MicrophoneSource {
        fn configure_vpio_io(audio_unit: &mut AudioUnit) {
            let enabled: u32 = 1;
            let disabled: u32 = 0;
            let _ = audio_unit
                .set_property(
                    kAudioOutputUnitProperty_EnableIO,
                    Scope::Input,
                    Element::Input,
                    Some(&enabled),
                )
                .ok();
            let _ = audio_unit
                .set_property(
                    kAudioOutputUnitProperty_EnableIO,
                    Scope::Output,
                    Element::Output,
                    Some(&disabled),
                )
                .ok();
        }

        fn set_default_vpio_flags(audio_unit: &mut AudioUnit) {
            let bypass_voice_processing: u32 = 0;
            let enable_agc: u32 = 0;
            let mute_output: u32 = 0;
            let ducking_cfg = AuVoiceIoOtherAudioDuckingConfiguration {
                m_enable_advanced_ducking: 0,
                _pad: [0; 3],
                m_ducking_level: 10,
            };

            for element in [Element::Input, Element::Output] {
                let _ = audio_unit
                    .set_property(
                        K_AU_VOICE_IO_PROPERTY_BYPASS_VOICE_PROCESSING,
                        Scope::Global,
                        element,
                        Some(&bypass_voice_processing),
                    )
                    .ok();
            }

            for element in [Element::Input, Element::Output] {
                let _ = audio_unit
                    .set_property(
                        K_AU_VOICE_IO_PROPERTY_VOICE_PROCESSING_ENABLE_AGC,
                        Scope::Global,
                        element,
                        Some(&enable_agc),
                    )
                    .ok();
            }

            for element in [Element::Input, Element::Output] {
                let _ = audio_unit
                    .set_property(
                        K_AU_VOICE_IO_PROPERTY_OTHER_AUDIO_DUCKING_CONFIGURATION,
                        Scope::Global,
                        element,
                        Some(&ducking_cfg),
                    )
                    .ok();
            }

            for element in [Element::Input, Element::Output] {
                let _ = audio_unit
                    .set_property(
                        K_AU_VOICE_IO_PROPERTY_MUTE_OUTPUT,
                        Scope::Global,
                        element,
                        Some(&mute_output),
                    )
                    .ok();
            }
        }

        pub fn list_mics() -> Vec<MicInfo> {
            let default_id = get_default_device_id(true);
            let mut out = Vec::new();

            if let Ok(device_ids) = get_audio_device_ids_for_scope(Scope::Input) {
                for device_id in device_ids {
                    out.push(MicInfo {
                        id: device_id,
                        name: get_device_name(device_id)
                            .unwrap_or_else(|_| format!("Audio Device {device_id}")),
                        is_default: default_id == Some(device_id),
                    });
                }
            }

            out
        }

        pub fn new(
            mut pipeline: RealTimePipeline,
            config: MicrophoneSourceConfig,
        ) -> Result<Self, MicrophoneError> {
            validate_microphone_config(config)?;

            let mut audio_unit = if config.vpio_enabled {
                let mut au = AudioUnit::new(IOType::VoiceProcessingIO)
                    .map_err(|e| MicrophoneError::Driver(e.to_string()))?;
                Self::configure_vpio_io(&mut au);
                au
            } else {
                let devid = match config.device_id {
                    Some(id) => id,
                    None => get_default_device_id(true).ok_or_else(|| {
                        MicrophoneError::Driver("no default input device".to_string())
                    })?,
                };
                audio_unit_from_device_id(devid, true)
                    .map_err(|e| MicrophoneError::Driver(e.to_string()))?
            };

            let flags = LinearPcmFlags::IS_FLOAT | LinearPcmFlags::IS_PACKED;
            let stream_format = coreaudio::audio_unit::StreamFormat {
                sample_rate: DRIVER_FORMAT.sample_rate as f64,
                sample_format: CaSampleFormat::F32,
                flags,
                channels: DRIVER_FORMAT.channels as u32,
            };
            let asbd = stream_format.to_asbd();

            if config.vpio_enabled {
                // Some VoiceProcessingIO paths expose this property as read-only. Treat as best-effort.
                let _ = audio_unit
                    .set_property(
                        kAudioUnitProperty_StreamFormat,
                        Scope::Output,
                        Element::Input,
                        Some(&asbd),
                    )
                    .ok();
                Self::set_default_vpio_flags(&mut audio_unit);
            } else {
                audio_unit
                    .set_property(
                        kAudioUnitProperty_StreamFormat,
                        Scope::Output,
                        Element::Input,
                        Some(&asbd),
                    )
                    .map_err(|e| MicrophoneError::Driver(e.to_string()))?;
            }

            let actual = audio_unit
                .input_stream_format()
                .map_err(|e| MicrophoneError::Driver(e.to_string()))?;
            if actual.sample_format != CaSampleFormat::F32 {
                return Err(MicrophoneError::Driver(format!(
                    "unsupported input sample format from driver: {:?}",
                    actual.sample_format
                )));
            }

            let input_format = StreamFormat::with_sample_format(
                actual.sample_rate.round() as u32,
                actual.channels as u16,
                SampleFormat::F32,
            );
            let non_interleaved = actual.flags.contains(LinearPcmFlags::IS_NON_INTERLEAVED);

            let mut converter: Box<dyn InputConverter> =
                Box::new(MasterFormatConverter::new(input_format, MASTER_FORMAT)?);
            let mut converted = Vec::<f32>::new();
            let mut interleaved_in = Vec::<f32>::new();

            if non_interleaved {
                audio_unit
                    .set_input_callback(move |args: ArgsNonInterleaved| {
                        interleaved_in.clear();
                        let channels: Vec<&[f32]> = args.data.channels().collect();
                        if channels.is_empty() {
                            return Ok(());
                        }
                        let frames = args.num_frames;
                        interleaved_in.reserve(frames * channels.len());

                        for frame in 0..frames {
                            for ch in &channels {
                                if frame < ch.len() {
                                    interleaved_in.push(ch[frame]);
                                }
                            }
                        }

                        if converter.convert(&interleaved_in, &mut converted).is_ok() {
                            pipeline.process_callback(converted.as_mut_slice());
                        }
                        Ok(())
                    })
                    .map_err(|e| MicrophoneError::Driver(e.to_string()))?;
            } else {
                audio_unit
                    .set_input_callback(move |args: ArgsInterleaved| {
                        if converter.convert(args.data.buffer, &mut converted).is_ok() {
                            pipeline.process_callback(converted.as_mut_slice());
                        }
                        Ok(())
                    })
                    .map_err(|e| MicrophoneError::Driver(e.to_string()))?;
            }

            Ok(Self {
                audio_unit,
                input_format,
                output_format: MASTER_FORMAT,
            })
        }

        pub fn start(&mut self) -> Result<(), MicrophoneError> {
            self.audio_unit
                .start()
                .map_err(|e| MicrophoneError::Driver(e.to_string()))
        }

        pub fn stop(&mut self) -> Result<(), MicrophoneError> {
            self.audio_unit
                .stop()
                .map_err(|e| MicrophoneError::Driver(e.to_string()))
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::*;

    pub struct MicrophoneSource {
        pub input_format: StreamFormat,
        pub output_format: StreamFormat,
    }

    impl MicrophoneSource {
        pub fn list_mics() -> Vec<MicInfo> {
            Vec::new()
        }

        pub fn new(
            _pipeline: RealTimePipeline,
            config: MicrophoneSourceConfig,
        ) -> Result<Self, MicrophoneError> {
            let _ = config;
            Err(MicrophoneError::UnsupportedPlatform)
        }

        pub fn start(&mut self) -> Result<(), MicrophoneError> {
            Err(MicrophoneError::UnsupportedPlatform)
        }

        pub fn stop(&mut self) -> Result<(), MicrophoneError> {
            Err(MicrophoneError::UnsupportedPlatform)
        }
    }
}

pub use imp::MicrophoneSource;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_uses_vpio_with_default_device() {
        let cfg = MicrophoneSourceConfig::default();
        assert_eq!(cfg.device_id, None);
        assert!(cfg.vpio_enabled);
    }

    #[test]
    fn explicit_device_is_rejected_when_vpio_is_enabled() {
        let err = validate_microphone_config(MicrophoneSourceConfig {
            device_id: Some(42),
            vpio_enabled: true,
        })
        .expect_err("invalid microphone config");

        match err {
            MicrophoneError::InvalidConfig(message) => {
                assert!(message.contains("device_id is not supported"));
            }
            other => panic!("expected InvalidConfig, got {other:?}"),
        }
    }
}
