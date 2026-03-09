use crate::engine::RealTimePipeline;
use crate::format::{StreamFormat, MASTER_FORMAT};
use crate::sources::screen_capture::{
    normalize_audio_buffers_into_scratch, select_item_by_id, select_items_by_ids, AudioBufferRef,
};

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

fn select_display<'a, T>(
    displays: &'a [T],
    display_id: Option<u32>,
    id_of: impl Fn(&T) -> u32,
) -> Result<&'a T, AppAudioError> {
    select_item_by_id(
        displays,
        display_id,
        id_of,
        || AppAudioError::NoDisplaysAvailable,
        AppAudioError::DisplayNotFound,
    )
}

fn select_applications<'a, T>(
    applications: &'a [T],
    pids: &[u32],
    id_of: impl Fn(&T) -> u32,
) -> Result<Vec<&'a T>, AppAudioError> {
    select_items_by_ids(
        applications,
        pids,
        id_of,
        || AppAudioError::NoApplicationsSelected,
        || AppAudioError::NoApplicationsAvailable,
        AppAudioError::ApplicationsNotFound,
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
            let selected_display =
                select_display(&displays, config.display_id, |display| display.display_id())?;

            let applications = content.applications();
            let selected_app_refs = select_applications(&applications, &config.pids, |app| {
                app.process_id().max(0) as u32
            })?;

            let filter = SCContentFilter::create()
                .with_display(selected_display)
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

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct TestDisplay {
        id: u32,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct TestApplication {
        pid: u32,
    }

    #[test]
    fn default_config_starts_with_no_selected_applications() {
        let cfg = AppAudioSourceConfig::default();
        assert!(cfg.pids.is_empty());
        assert_eq!(cfg.display_id, None);
    }

    #[test]
    fn select_display_uses_first_display_by_default() {
        let displays = [TestDisplay { id: 10 }, TestDisplay { id: 20 }];
        let selected = select_display(&displays, None, |display| display.id).expect("display");

        assert_eq!(selected.id, 10);
    }

    #[test]
    fn select_applications_requires_non_empty_pid_list() {
        let apps = [TestApplication { pid: 10 }];
        let err =
            select_applications(&apps, &[], |application| application.pid).expect_err("no pids");

        match err {
            AppAudioError::NoApplicationsSelected => {}
            other => panic!("expected NoApplicationsSelected, got {other:?}"),
        }
    }

    #[test]
    fn select_applications_reports_missing_pids() {
        let apps = [TestApplication { pid: 10 }, TestApplication { pid: 20 }];
        let err = select_applications(&apps, &[10, 30], |application| application.pid)
            .expect_err("missing pids");

        match err {
            AppAudioError::ApplicationsNotFound(pids) => assert_eq!(pids, vec![30]),
            other => panic!("expected ApplicationsNotFound, got {other:?}"),
        }
    }

    #[test]
    fn select_applications_preserves_requested_order() {
        let apps = [
            TestApplication { pid: 10 },
            TestApplication { pid: 20 },
            TestApplication { pid: 30 },
        ];
        let selected = select_applications(&apps, &[30, 10], |application| application.pid)
            .expect("select apps");

        assert_eq!(
            selected
                .iter()
                .map(|application| application.pid)
                .collect::<Vec<_>>(),
            vec![30, 10]
        );
    }
}
