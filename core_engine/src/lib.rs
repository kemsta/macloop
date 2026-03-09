pub mod analyzer;
pub mod converter;
pub mod engine;
pub mod format;
pub mod metrics;
pub mod outputs;
pub mod processor;
pub mod sources;

pub use analyzer::AudioAnalyzer;
pub use converter::{
    convert_f32_to_i16, InputConversionError, InputConverter, MasterFormatConverter,
};
pub use engine::{
    run_mock_callback_demo, AudioEngineController, EngineError, PipelineCommand, RealTimePipeline,
    RouteConsumer, RouteProducer, SourceType,
};
pub use format::{SampleFormat, StreamFormat, MASTER_FORMAT};
pub use metrics::{
    EngineMetricsSnapshot, LatencyHistogram, LatencyHistogramSnapshot, NodeMetrics,
    NodeMetricsSnapshot, PipelineMetrics, PipelineMetricsSnapshot, StreamMetricsSnapshot,
    LATENCY_BUCKET_BOUNDS_US,
};
pub use outputs::asr_sink::{
    AsrChunkView, AsrInputId, AsrInputMetricsSnapshot, AsrSampleSlice, AsrSink, AsrSinkCallback,
    AsrSinkConfig, AsrSinkError, AsrSinkInput, AsrSinkMetricsSnapshot,
};
pub use outputs::wav_file::{WavFileOutput, WavOutputError, WavSinkMetricsSnapshot};
pub use processor::{AudioProcessor, NodeId, OutputId, StreamId};
pub use sources::app_audio::{
    AppAudioError, AppAudioSource, AppAudioSourceConfig, ApplicationInfo,
};
pub use sources::microphone::{MicInfo, MicrophoneError, MicrophoneSource, MicrophoneSourceConfig};
pub use sources::synthetic::{SyntheticSource, SyntheticSourceConfig, SyntheticSourceError};
pub use sources::system_audio::{
    DisplayInfo, SystemAudioError, SystemAudioSource, SystemAudioSourceConfig,
};
