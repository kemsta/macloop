use crate::engine::RealTimePipeline;
use crate::format::{StreamFormat, MASTER_FORMAT};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationInfo {
    pub pid: u32,
    pub name: String,
    pub bundle_id: String,
    pub is_default: bool,
}

#[derive(Debug, Clone, Default)]
pub struct AppAudioSourceConfig {
    pub pids: Vec<u32>,
    pub display_id: Option<u32>,
}

#[derive(Debug)]
pub enum AppAudioError {
    UnsupportedPlatform,
    NoApplicationsAvailable,
    NoApplicationsSelected,
    ApplicationsNotFound(Vec<u32>),
    NoDisplaysAvailable,
    DisplayNotFound(u32),
    Driver(String),
}

impl std::fmt::Display for AppAudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                write!(f, "application audio source is only implemented for macOS")
            }
            Self::NoApplicationsAvailable => {
                write!(f, "no applications are available for audio capture")
            }
            Self::NoApplicationsSelected => {
                write!(f, "at least one application pid must be provided")
            }
            Self::ApplicationsNotFound(pids) => {
                let joined = pids
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "application pid(s) not found: {joined}")
            }
            Self::NoDisplaysAvailable => {
                write!(f, "no displays are available for application audio capture")
            }
            Self::DisplayNotFound(display_id) => {
                write!(f, "display with id {display_id} was not found")
            }
            Self::Driver(err) => write!(f, "driver error: {err}"),
        }
    }
}

impl std::error::Error for AppAudioError {}

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
            scratch.clear();
            let target_channels = MASTER_FORMAT.channels as usize;

            let Some(first_buffer) = audio_data.get(0) else {
                return;
            };

            let num_buffers = audio_data.num_buffers();
            if num_buffers == 0 {
                return;
            }

            if num_buffers == 1 {
                let samples: &[f32] = bytemuck::cast_slice::<u8, f32>(first_buffer.data());
                let input_channels = usize::max(first_buffer.number_channels as usize, 1);
                let frames = samples.len() / input_channels;
                scratch.reserve(frames * target_channels);

                for frame in samples.chunks_exact(input_channels) {
                    if input_channels == 1 {
                        let sample = frame[0];
                        for _ in 0..target_channels {
                            scratch.push(sample);
                        }
                    } else {
                        for sample in frame.iter().take(target_channels) {
                            scratch.push(*sample);
                        }
                    }
                }
                return;
            }

            let mut buffers = Vec::with_capacity(num_buffers);
            let mut frames = usize::MAX;
            for buffer in audio_data.iter() {
                let samples: &[f32] = bytemuck::cast_slice::<u8, f32>(buffer.data());
                let channels = usize::max(buffer.number_channels as usize, 1);
                frames = frames.min(samples.len() / channels);
                buffers.push((samples, channels));
            }

            if frames == usize::MAX || frames == 0 {
                return;
            }

            scratch.reserve(frames * target_channels);

            for frame_index in 0..frames {
                let mut emitted = 0_usize;
                let mut fallback_sample = 0.0_f32;
                let mut have_fallback = false;

                for (samples, channels) in &buffers {
                    let base = frame_index * *channels;
                    for channel_index in 0..*channels {
                        let sample = samples[base + channel_index];
                        if !have_fallback {
                            fallback_sample = sample;
                            have_fallback = true;
                        }
                        if emitted < target_channels {
                            scratch.push(sample);
                            emitted += 1;
                        }
                    }
                    if emitted >= target_channels {
                        break;
                    }
                }

                if have_fallback {
                    while emitted < target_channels {
                        scratch.push(fallback_sample);
                        emitted += 1;
                    }
                }
            }
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

    pub struct AppAudioSource {
        stream: SCStream,
        pub input_format: StreamFormat,
        pub output_format: StreamFormat,
        pub pids: Vec<u32>,
        pub display_id: u32,
    }

    impl AppAudioSource {
        pub fn list_applications() -> Vec<ApplicationInfo> {
            match SCShareableContent::get() {
                Ok(content) => content
                    .applications()
                    .into_iter()
                    .enumerate()
                    .map(|(index, app)| ApplicationInfo {
                        pid: app.process_id().max(0) as u32,
                        name: app.application_name(),
                        bundle_id: app.bundle_identifier(),
                        is_default: index == 0,
                    })
                    .collect(),
                Err(_) => Vec::new(),
            }
        }

        pub fn new(
            pipeline: RealTimePipeline,
            config: AppAudioSourceConfig,
        ) -> Result<Self, AppAudioError> {
            let content =
                SCShareableContent::get().map_err(|e| AppAudioError::Driver(e.to_string()))?;

            let displays = content.displays();
            let selected_display = match config.display_id {
                Some(display_id) => displays
                    .into_iter()
                    .find(|display| display.display_id() == display_id)
                    .ok_or(AppAudioError::DisplayNotFound(display_id))?,
                None => displays
                    .into_iter()
                    .next()
                    .ok_or(AppAudioError::NoDisplaysAvailable)?,
            };

            let applications = content.applications();
            if config.pids.is_empty() {
                return Err(AppAudioError::NoApplicationsSelected);
            }

            if applications.is_empty() {
                return Err(AppAudioError::NoApplicationsAvailable);
            }

            let mut selected_apps = Vec::with_capacity(config.pids.len());
            let mut missing_pids = Vec::new();

            for pid in &config.pids {
                match applications.iter().find(|app| app.process_id() == *pid as i32) {
                    Some(app) => selected_apps.push(app.clone()),
                    None => missing_pids.push(*pid),
                }
            }

            if !missing_pids.is_empty() {
                return Err(AppAudioError::ApplicationsNotFound(missing_pids));
            }

            let selected_app_refs = selected_apps.iter().collect::<Vec<_>>();

            let filter = SCContentFilter::create()
                .with_display(&selected_display)
                .with_including_applications(&selected_app_refs, &[])
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
                return Err(AppAudioError::Driver(
                    "failed to register ScreenCaptureKit audio output handler".to_string(),
                ));
            }

            Ok(Self {
                stream,
                input_format: MASTER_FORMAT,
                output_format: MASTER_FORMAT,
                pids: config.pids,
                display_id: selected_display.display_id(),
            })
        }

        pub fn start(&mut self) -> Result<(), AppAudioError> {
            self.stream
                .start_capture()
                .map_err(|e| AppAudioError::Driver(e.to_string()))
        }

        pub fn stop(&mut self) -> Result<(), AppAudioError> {
            self.stream
                .stop_capture()
                .map_err(|e| AppAudioError::Driver(e.to_string()))
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::*;

    pub struct AppAudioSource {
        pub input_format: StreamFormat,
        pub output_format: StreamFormat,
        pub pids: Vec<u32>,
        pub display_id: u32,
    }

    impl AppAudioSource {
        pub fn list_applications() -> Vec<ApplicationInfo> {
            Vec::new()
        }

        pub fn new(
            _pipeline: RealTimePipeline,
            _config: AppAudioSourceConfig,
        ) -> Result<Self, AppAudioError> {
            Err(AppAudioError::UnsupportedPlatform)
        }

        pub fn start(&mut self) -> Result<(), AppAudioError> {
            Err(AppAudioError::UnsupportedPlatform)
        }

        pub fn stop(&mut self) -> Result<(), AppAudioError> {
            Err(AppAudioError::UnsupportedPlatform)
        }
    }
}

pub use imp::AppAudioSource;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_starts_with_no_selected_applications() {
        let cfg = AppAudioSourceConfig::default();
        assert!(cfg.pids.is_empty());
        assert_eq!(cfg.display_id, None);
    }
}
