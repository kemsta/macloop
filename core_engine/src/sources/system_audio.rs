use crate::engine::RealTimePipeline;
use crate::format::{StreamFormat, MASTER_FORMAT};
use crate::sources::screen_capture::{
    normalize_audio_buffers_into_scratch, select_item_by_id, AudioBufferRef,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayInfo {
    pub id: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub is_default: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemAudioSourceConfig {
    pub display_id: Option<u32>,
}

#[derive(Debug)]
pub enum SystemAudioError {
    UnsupportedPlatform,
    NoDisplaysAvailable,
    DisplayNotFound(u32),
    Driver(String),
}

impl std::fmt::Display for SystemAudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                write!(f, "system audio source is only implemented for macOS")
            }
            Self::NoDisplaysAvailable => {
                write!(f, "no displays are available for system audio capture")
            }
            Self::DisplayNotFound(display_id) => {
                write!(f, "display with id {display_id} was not found")
            }
            Self::Driver(err) => write!(f, "driver error: {err}"),
        }
    }
}

impl std::error::Error for SystemAudioError {}

fn select_display<'a, T>(
    displays: &'a [T],
    display_id: Option<u32>,
    id_of: impl Fn(&T) -> u32,
) -> Result<&'a T, SystemAudioError> {
    select_item_by_id(
        displays,
        display_id,
        id_of,
        || SystemAudioError::NoDisplaysAvailable,
        SystemAudioError::DisplayNotFound,
    )
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use screencapturekit::prelude::*;
    use screencapturekit::stream::output_trait::SCStreamOutputTrait;
    use screencapturekit::stream::output_type::SCStreamOutputType;
    use screencapturekit::AudioBufferList;
    use std::cell::RefCell;

    struct HandlerState {
        pipeline: RealTimePipeline,
        scratch: Vec<f32>,
    }

    struct AudioOutputHandler {
        state: RefCell<HandlerState>,
    }

    impl AudioOutputHandler {
        fn copy_audio_data_into_scratch(audio_data: &AudioBufferList, scratch: &mut Vec<f32>) {
            let buffers = audio_data
                .iter()
                .map(|buffer| AudioBufferRef {
                    samples: bytemuck::cast_slice::<u8, f32>(buffer.data()),
                    channels: usize::max(buffer.number_channels as usize, 1),
                })
                .collect::<Vec<_>>();

            normalize_audio_buffers_into_scratch(
                &buffers,
                MASTER_FORMAT.channels as usize,
                scratch,
            );
        }
    }

    impl SCStreamOutputTrait for AudioOutputHandler {
        fn did_output_sample_buffer(
            &self,
            sample_buffer: CMSampleBuffer,
            of_type: SCStreamOutputType,
        ) {
            if of_type != SCStreamOutputType::Audio {
                return;
            }

            let Some(audio_data) = sample_buffer.audio_buffer_list() else {
                return;
            };

            let Ok(mut state) = self.state.try_borrow_mut() else {
                return;
            };

            Self::copy_audio_data_into_scratch(&audio_data, &mut state.scratch);
            if !state.scratch.is_empty() {
                let HandlerState {
                    pipeline, scratch, ..
                } = &mut *state;
                pipeline.process_callback(scratch.as_mut_slice());
            }
        }
    }

    pub struct SystemAudioSource {
        stream: SCStream,
        pub input_format: StreamFormat,
        pub output_format: StreamFormat,
        pub display_id: u32,
    }

    impl SystemAudioSource {
        pub fn list_displays() -> Vec<DisplayInfo> {
            match SCShareableContent::get() {
                Ok(content) => content
                    .displays()
                    .into_iter()
                    .enumerate()
                    .map(|(index, display)| DisplayInfo {
                        id: display.display_id(),
                        name: format!("Display {}", display.display_id()),
                        width: display.width(),
                        height: display.height(),
                        is_default: index == 0,
                    })
                    .collect(),
                Err(_) => Vec::new(),
            }
        }

        pub fn new(
            pipeline: RealTimePipeline,
            config: SystemAudioSourceConfig,
        ) -> Result<Self, SystemAudioError> {
            let content =
                SCShareableContent::get().map_err(|e| SystemAudioError::Driver(e.to_string()))?;

            let displays = content.displays();
            let selected_display =
                select_display(&displays, config.display_id, |display| display.display_id())?;

            let filter = SCContentFilter::create()
                .with_display(selected_display)
                .with_excluding_windows(&[])
                .build();

            let mut stream_config = SCStreamConfiguration::new();
            stream_config.set_captures_audio(true);
            stream_config.set_excludes_current_process_audio(true);
            stream_config.set_sample_rate(MASTER_FORMAT.sample_rate as i32);
            stream_config.set_channel_count(MASTER_FORMAT.channels as i32);

            let mut stream = SCStream::new(&filter, &stream_config);
            let handler_registered = stream.add_output_handler(
                AudioOutputHandler {
                    state: RefCell::new(HandlerState {
                        pipeline,
                        scratch: Vec::new(),
                    }),
                },
                SCStreamOutputType::Audio,
            );

            if handler_registered.is_none() {
                return Err(SystemAudioError::Driver(
                    "failed to register ScreenCaptureKit audio output handler".to_string(),
                ));
            }

            Ok(Self {
                stream,
                input_format: MASTER_FORMAT,
                output_format: MASTER_FORMAT,
                display_id: selected_display.display_id(),
            })
        }

        pub fn start(&mut self) -> Result<(), SystemAudioError> {
            self.stream
                .start_capture()
                .map_err(|e| SystemAudioError::Driver(e.to_string()))
        }

        pub fn stop(&mut self) -> Result<(), SystemAudioError> {
            self.stream
                .stop_capture()
                .map_err(|e| SystemAudioError::Driver(e.to_string()))
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::*;

    pub struct SystemAudioSource {
        pub input_format: StreamFormat,
        pub output_format: StreamFormat,
        pub display_id: u32,
    }

    impl SystemAudioSource {
        pub fn list_displays() -> Vec<DisplayInfo> {
            Vec::new()
        }

        pub fn new(
            _pipeline: RealTimePipeline,
            _config: SystemAudioSourceConfig,
        ) -> Result<Self, SystemAudioError> {
            Err(SystemAudioError::UnsupportedPlatform)
        }

        pub fn start(&mut self) -> Result<(), SystemAudioError> {
            Err(SystemAudioError::UnsupportedPlatform)
        }

        pub fn stop(&mut self) -> Result<(), SystemAudioError> {
            Err(SystemAudioError::UnsupportedPlatform)
        }
    }
}

pub use imp::SystemAudioSource;

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct TestDisplay {
        id: u32,
    }

    #[test]
    fn default_config_uses_first_available_display() {
        let cfg = SystemAudioSourceConfig::default();
        assert_eq!(cfg.display_id, None);
    }

    #[test]
    fn select_display_uses_first_display_by_default() {
        let displays = [TestDisplay { id: 10 }, TestDisplay { id: 20 }];
        let selected = select_display(&displays, None, |display| display.id).expect("display");

        assert_eq!(selected.id, 10);
    }

    #[test]
    fn select_display_reports_missing_display() {
        let displays = [TestDisplay { id: 10 }];
        let err = select_display(&displays, Some(20), |display| display.id).expect_err("missing");

        match err {
            SystemAudioError::DisplayNotFound(display_id) => assert_eq!(display_id, 20),
            other => panic!("expected DisplayNotFound, got {other:?}"),
        }
    }

    #[test]
    fn select_display_requires_at_least_one_display() {
        let err = select_display::<TestDisplay>(&[], None, |display| display.id)
            .expect_err("no displays");

        match err {
            SystemAudioError::NoDisplaysAvailable => {}
            other => panic!("expected NoDisplaysAvailable, got {other:?}"),
        }
    }
}
