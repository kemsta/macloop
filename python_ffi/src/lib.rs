mod stats;

use core_engine::{
    AppAudioSource, AppAudioSourceConfig, ApplicationInfo, AsrChunkView, AsrSampleSlice, AsrSink,
    AsrSinkCallback, AsrSinkConfig, AsrSinkInput, AsrSinkMetricsSnapshot, AudioEngineController,
    AudioProcessor, DisplayInfo, EngineError, MicInfo, MicrophoneSource, MicrophoneSourceConfig,
    NodeMetrics, SampleFormat, SourceType, StreamFormat, SyntheticSource, SyntheticSourceConfig,
    SystemAudioSource, SystemAudioSourceConfig, WavFileOutput, WavSinkMetricsSnapshot,
};
use numpy::ToPyArray;
use pyo3::exceptions::{PyOSError, PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyDict, PyList, PyModule};
use stats::{
    PyAsrInputStats, PyLatencyStats, PyPipelineStats, PyProcessorStats, PyStreamStats,
    PyWavSinkStats,
};
use std::collections::HashMap;
use std::fs::File;
use std::os::fd::FromRawFd;
use std::os::raw::c_int;
use std::sync::Arc;

const DEFAULT_COMMAND_CAPACITY: usize = 32;
const DEFAULT_GARBAGE_CAPACITY: usize = 32;
const DEFAULT_ROUTE_CAPACITY: usize = 4096;
const DEFAULT_MAX_PROCESSORS: usize = 32;
const DEFAULT_MAX_OUTPUTS: usize = 16;

#[derive(Default)]
struct GainProcessorNode {
    id: String,
    gain: f32,
    _metrics: Option<Arc<NodeMetrics>>,
}

impl AudioProcessor for GainProcessorNode {
    fn id(&self) -> &str {
        &self.id
    }

    fn set_metrics(&mut self, metrics: Arc<NodeMetrics>) {
        self._metrics = Some(metrics);
    }

    fn process(&mut self, buffer: &mut [f32]) {
        for sample in buffer {
            *sample *= self.gain;
        }
    }
}

enum StreamRuntime {
    AppAudio(AppAudioSource),
    Microphone(MicrophoneSource),
    SystemAudio(SystemAudioSource),
    Synthetic(SyntheticSource),
}

impl StreamRuntime {
    fn start(&mut self) -> Result<(), String> {
        match self {
            Self::AppAudio(source) => source.start().map_err(|e| e.to_string()),
            Self::Microphone(source) => source.start().map_err(|e| e.to_string()),
            Self::SystemAudio(source) => source.start().map_err(|e| e.to_string()),
            Self::Synthetic(source) => source.start().map_err(|e| e.to_string()),
        }
    }

    fn stop(&mut self) -> Result<(), String> {
        match self {
            Self::AppAudio(source) => source.stop().map_err(|e| e.to_string()),
            Self::Microphone(source) => source.stop().map_err(|e| e.to_string()),
            Self::SystemAudio(source) => source.stop().map_err(|e| e.to_string()),
            Self::Synthetic(source) => source.stop().map_err(|e| e.to_string()),
        }
    }
}

struct StreamRuntimeState {
    runtime: StreamRuntime,
    started: bool,
}

struct PythonAsrCallback {
    callback: Py<PyAny>,
}

impl AsrSinkCallback for PythonAsrCallback {
    fn on_chunk(&mut self, chunk: AsrChunkView<'_>) {
        let _ = Python::try_attach(|py| {
            let samples = match chunk.samples {
                AsrSampleSlice::F32(values) => values.to_pyarray(py).into_any().unbind(),
                AsrSampleSlice::I16(values) => values.to_pyarray(py).into_any().unbind(),
            };

            if let Err(err) = self
                .callback
                .call1(py, (chunk.input_id, chunk.frames, samples))
            {
                err.print(py);
            }
        });
    }
}

#[pyclass(name = "_AsrSinkBackend", module = "macloop._macloop", unsendable)]
struct PyAsrSinkBackend {
    sink: Option<AsrSink>,
    final_stats: Option<AsrSinkMetricsSnapshot>,
}

#[pymethods]
impl PyAsrSinkBackend {
    fn stats<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let snapshot = match (&self.sink, &self.final_stats) {
            (Some(sink), _) => sink.stats(),
            (None, Some(snapshot)) => snapshot.clone(),
            (None, None) => AsrSinkMetricsSnapshot::default(),
        };

        let out = PyDict::new(py);
        for (input_id, stats) in snapshot {
            out.set_item(
                input_id,
                Py::new(py, PyAsrInputStats::from_snapshot(py, stats)?)?,
            )?;
        }
        Ok(out)
    }

    fn close(&mut self) -> PyResult<()> {
        let Some(mut sink) = self.sink.take() else {
            return Ok(());
        };

        let stop_result = sink.stop();
        self.final_stats = Some(sink.stats());
        stop_result
            .map_err(|e| PyRuntimeError::new_err(format!("failed to stop asr sink: {e}")))?;
        Ok(())
    }
}

impl Drop for PyAsrSinkBackend {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[pyclass(name = "_WavSinkBackend", module = "macloop._macloop", unsendable)]
struct PyWavSinkBackend {
    sink: Option<WavFileOutput>,
    final_stats: Option<WavSinkMetricsSnapshot>,
}

#[pymethods]
impl PyWavSinkBackend {
    fn stats(&self, py: Python<'_>) -> PyResult<Py<PyWavSinkStats>> {
        let snapshot = match (&self.sink, &self.final_stats) {
            (Some(sink), _) => sink.stats(),
            (None, Some(snapshot)) => snapshot.clone(),
            (None, None) => WavSinkMetricsSnapshot::default(),
        };
        Py::new(py, PyWavSinkStats::from_snapshot(py, snapshot)?)
    }

    fn close(&mut self) -> PyResult<()> {
        let Some(mut sink) = self.sink.take() else {
            return Ok(());
        };

        let stop_result = sink.stop();
        self.final_stats = Some(sink.stats());
        stop_result
            .map_err(|e| PyRuntimeError::new_err(format!("failed to stop wav sink: {e}")))?;
        Ok(())
    }
}

impl Drop for PyWavSinkBackend {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[pyclass(name = "_AudioEngineBackend", module = "macloop._macloop", unsendable)]
struct PyAudioEngineBackend {
    controller: AudioEngineController,
    sources: HashMap<String, StreamRuntimeState>,
    route_streams: HashMap<String, String>,
    closed: bool,
}

#[pymethods]
impl PyAudioEngineBackend {
    #[new]
    fn new() -> Self {
        Self {
            controller: AudioEngineController::new(
                DEFAULT_COMMAND_CAPACITY,
                DEFAULT_GARBAGE_CAPACITY,
                DEFAULT_ROUTE_CAPACITY,
            ),
            sources: HashMap::new(),
            route_streams: HashMap::new(),
            closed: false,
        }
    }

    fn create_stream(
        &mut self,
        stream_id: String,
        source_kind: String,
        config: Bound<'_, PyDict>,
    ) -> PyResult<()> {
        self.ensure_open()?;

        match source_kind.as_str() {
            "microphone" => {
                let device_id = dict_optional_u32(&config, "device_id")?;
                let vpio_enabled = dict_bool_with_default(&config, "vpio_enabled", true)?;

                let pipeline = self
                    .controller
                    .create_stream(
                        stream_id.clone(),
                        SourceType::Microphone { device_id },
                        DEFAULT_MAX_PROCESSORS,
                        DEFAULT_MAX_OUTPUTS,
                    )
                    .map_err(engine_error)?;

                let source = MicrophoneSource::new(
                    pipeline,
                    MicrophoneSourceConfig {
                        device_id,
                        vpio_enabled,
                    },
                )
                .map_err(|e| {
                    PyRuntimeError::new_err(format!("failed to create microphone source: {e}"))
                })?;

                self.sources.insert(
                    stream_id,
                    StreamRuntimeState {
                        runtime: StreamRuntime::Microphone(source),
                        started: false,
                    },
                );
                Ok(())
            }
            "application_audio" => {
                let pid = dict_optional_u32(&config, "pid")?;
                let display_id = dict_optional_u32(&config, "display_id")?;

                let selected_pid = pid.ok_or_else(|| {
                    PyValueError::new_err("application_audio source requires a pid")
                })?;

                let pipeline = self
                    .controller
                    .create_stream(
                        stream_id.clone(),
                        SourceType::ApplicationAudio { pid: selected_pid },
                        DEFAULT_MAX_PROCESSORS,
                        DEFAULT_MAX_OUTPUTS,
                    )
                    .map_err(engine_error)?;

                let source =
                    AppAudioSource::new(pipeline, AppAudioSourceConfig { pid, display_id })
                        .map_err(|e| {
                            PyRuntimeError::new_err(format!(
                                "failed to create application audio source: {e}"
                            ))
                        })?;

                self.sources.insert(
                    stream_id,
                    StreamRuntimeState {
                        runtime: StreamRuntime::AppAudio(source),
                        started: false,
                    },
                );
                Ok(())
            }
            "system_audio" => {
                let display_id = dict_optional_u32(&config, "display_id")?;

                let pipeline = self
                    .controller
                    .create_stream(
                        stream_id.clone(),
                        SourceType::SystemAudio,
                        DEFAULT_MAX_PROCESSORS,
                        DEFAULT_MAX_OUTPUTS,
                    )
                    .map_err(engine_error)?;

                let source =
                    SystemAudioSource::new(pipeline, SystemAudioSourceConfig { display_id })
                        .map_err(|e| {
                            PyRuntimeError::new_err(format!(
                                "failed to create system audio source: {e}"
                            ))
                        })?;

                self.sources.insert(
                    stream_id,
                    StreamRuntimeState {
                        runtime: StreamRuntime::SystemAudio(source),
                        started: false,
                    },
                );
                Ok(())
            }
            "synthetic" => {
                let frames_per_callback =
                    dict_usize_with_default(&config, "frames_per_callback", 160)?;
                let callback_count = dict_usize_with_default(&config, "callback_count", 4)?;
                let start_value = dict_f32_with_default(&config, "start_value", 0.0)?;
                let step_value = dict_f32_with_default(&config, "step_value", 1.0)?;
                let interval_ms = dict_u64_with_default(&config, "interval_ms", 0)?;
                let start_delay_ms = dict_u64_with_default(&config, "start_delay_ms", 0)?;

                let pipeline = self
                    .controller
                    .create_stream(
                        stream_id.clone(),
                        SourceType::Synthetic,
                        DEFAULT_MAX_PROCESSORS,
                        DEFAULT_MAX_OUTPUTS,
                    )
                    .map_err(engine_error)?;

                let source = SyntheticSource::new(
                    pipeline,
                    SyntheticSourceConfig {
                        frames_per_callback,
                        callback_count,
                        start_value,
                        step_value,
                        interval: std::time::Duration::from_millis(interval_ms),
                        start_delay: std::time::Duration::from_millis(start_delay_ms),
                    },
                )
                .map_err(|e| {
                    PyValueError::new_err(format!("failed to create synthetic source: {e}"))
                })?;

                self.sources.insert(
                    stream_id,
                    StreamRuntimeState {
                        runtime: StreamRuntime::Synthetic(source),
                        started: false,
                    },
                );
                Ok(())
            }
            _ => Err(PyValueError::new_err(format!(
                "unsupported source_kind '{source_kind}'"
            ))),
        }
    }

    fn add_processor(
        &mut self,
        stream_id: String,
        processor_id: String,
        processor_kind: String,
        config: Bound<'_, PyDict>,
    ) -> PyResult<()> {
        self.ensure_open()?;

        match processor_kind.as_str() {
            "gain" => {
                let gain = dict_f32_with_default(&config, "gain", 1.0)?;
                self.controller
                    .add_processor(
                        &stream_id,
                        Box::new(GainProcessorNode {
                            id: processor_id,
                            gain,
                            _metrics: None,
                        }),
                    )
                    .map_err(engine_error)
            }
            _ => Err(PyValueError::new_err(format!(
                "unsupported processor_kind '{processor_kind}'"
            ))),
        }
    }

    fn route(&mut self, route_id: String, stream_id: String) -> PyResult<()> {
        self.ensure_open()?;
        self.controller
            .route(&stream_id, &route_id)
            .map_err(engine_error)?;
        self.route_streams.insert(route_id, stream_id);
        Ok(())
    }

    fn get_stats<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let out = PyDict::new(py);
        for (stream_id, snapshot) in self.controller.get_stats() {
            let stats = Py::new(py, PyStreamStats::from_snapshot(py, snapshot)?)?;
            out.set_item(stream_id, stats)?;
        }
        Ok(out)
    }

    fn close(&mut self) -> PyResult<()> {
        if self.closed {
            return Ok(());
        }

        let mut first_error: Option<PyErr> = None;

        for source in self.sources.values_mut() {
            if !source.started {
                continue;
            }
            if let Err(err) = source.runtime.stop() {
                if first_error.is_none() {
                    first_error = Some(PyRuntimeError::new_err(format!(
                        "failed to stop source: {err}"
                    )));
                }
            }
        }
        self.sources.clear();
        self.route_streams.clear();
        self.controller.tick_gc();
        self.closed = true;

        if let Some(err) = first_error {
            Err(err)
        } else {
            Ok(())
        }
    }
}

impl PyAudioEngineBackend {
    fn ensure_open(&self) -> PyResult<()> {
        if self.closed {
            Err(PyRuntimeError::new_err("audio engine backend is closed"))
        } else {
            Ok(())
        }
    }

    fn ensure_route_consumers_available(&self, route_ids: &[String]) -> PyResult<()> {
        if route_ids.is_empty() {
            return Err(PyValueError::new_err("routes must not be empty"));
        }

        for route_id in route_ids {
            if !self.controller.has_output_consumer(route_id) {
                return Err(PyValueError::new_err(format!(
                    "route '{route_id}' is not available"
                )));
            }
        }

        Ok(())
    }

    fn ensure_streams_started_for_routes(&mut self, route_ids: &[String]) -> PyResult<()> {
        let mut streams_to_start = Vec::<String>::new();

        for route_id in route_ids {
            let stream_id = self.route_streams.get(route_id).ok_or_else(|| {
                PyValueError::new_err(format!("route '{route_id}' is not registered"))
            })?;

            if !streams_to_start
                .iter()
                .any(|existing| existing == stream_id)
            {
                streams_to_start.push(stream_id.clone());
            }
        }

        for stream_id in streams_to_start {
            let state = self.sources.get_mut(&stream_id).ok_or_else(|| {
                PyValueError::new_err(format!("stream '{stream_id}' is not registered"))
            })?;

            if state.started {
                continue;
            }

            state.runtime.start().map_err(|err| {
                PyRuntimeError::new_err(format!(
                    "failed to start source for stream '{stream_id}': {err}"
                ))
            })?;
            state.started = true;
        }

        Ok(())
    }

    fn build_asr_sink(
        &mut self,
        route_ids: Vec<String>,
        sample_rate: u32,
        channels: u16,
        sample_format: String,
        chunk_frames: usize,
        callback: Py<PyAny>,
    ) -> PyResult<PyAsrSinkBackend> {
        self.ensure_open()?;
        self.ensure_route_consumers_available(&route_ids)?;

        let format = StreamFormat::with_sample_format(
            sample_rate,
            channels,
            parse_sample_format(&sample_format)?,
        );
        AsrSink::validate_config(AsrSinkConfig {
            format,
            chunk_frames,
        })
        .map_err(|err| PyValueError::new_err(err.to_string()))?;

        let mut inputs = Vec::with_capacity(route_ids.len());
        for route_id in &route_ids {
            let consumer = self
                .controller
                .take_output_consumer(route_id)
                .ok_or_else(|| {
                    PyValueError::new_err(format!("route '{route_id}' is not available"))
                })?;
            inputs.push(AsrSinkInput {
                input_id: route_id.clone(),
                consumer,
            });
        }

        let mut sink = AsrSink::spawn(
            inputs,
            AsrSinkConfig {
                format,
                chunk_frames,
            },
            Box::new(PythonAsrCallback { callback }),
        )
        .map_err(|err| PyRuntimeError::new_err(format!("failed to create asr sink: {err}")))?;

        if let Err(err) = self.ensure_streams_started_for_routes(&route_ids) {
            let _ = sink.stop();
            return Err(err);
        }

        Ok(PyAsrSinkBackend {
            sink: Some(sink),
            final_stats: None,
        })
    }

    fn build_wav_sink(
        &mut self,
        route_ids: Vec<String>,
        fd: i32,
        mix_gain: f32,
    ) -> PyResult<PyWavSinkBackend> {
        self.ensure_open()?;
        self.ensure_route_consumers_available(&route_ids)?;

        let mut consumers = Vec::with_capacity(route_ids.len());
        for route_id in &route_ids {
            let consumer = self
                .controller
                .take_output_consumer(route_id)
                .ok_or_else(|| {
                    PyValueError::new_err(format!("route '{route_id}' is not available"))
                })?;
            consumers.push(consumer);
        }

        let file = duplicate_file_descriptor(fd)
            .map_err(|e| PyOSError::new_err(format!("failed to duplicate file descriptor: {e}")))?;

        let mut sink = WavFileOutput::spawn_file_mix(
            file,
            self.controller.master_format(),
            consumers,
            mix_gain,
        )
        .map_err(|err| PyRuntimeError::new_err(format!("failed to create wav sink: {err}")))?;

        if let Err(err) = self.ensure_streams_started_for_routes(&route_ids) {
            let _ = sink.stop();
            return Err(err);
        }

        Ok(PyWavSinkBackend {
            sink: Some(sink),
            final_stats: None,
        })
    }
}

impl Drop for PyAudioEngineBackend {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[pyfunction]
fn list_microphones(py: Python<'_>) -> PyResult<Bound<'_, PyList>> {
    let list = PyList::empty(py);
    for mic in MicrophoneSource::list_mics() {
        append_microphone_info(&list, mic)?;
    }
    Ok(list)
}

#[pyfunction]
fn list_displays(py: Python<'_>) -> PyResult<Bound<'_, PyList>> {
    let list = PyList::empty(py);
    for display in SystemAudioSource::list_displays() {
        append_display_info(&list, display)?;
    }
    Ok(list)
}

#[pyfunction]
fn list_applications(py: Python<'_>) -> PyResult<Bound<'_, PyList>> {
    let list = PyList::empty(py);
    for application in AppAudioSource::list_applications() {
        append_application_info(&list, application)?;
    }
    Ok(list)
}

#[pyfunction(name = "_create_asr_sink")]
fn create_asr_sink(
    mut engine: PyRefMut<'_, PyAudioEngineBackend>,
    sink_id: String,
    route_ids: Vec<String>,
    sample_rate: u32,
    channels: u16,
    sample_format: String,
    chunk_frames: usize,
    callback: Py<PyAny>,
) -> PyResult<PyAsrSinkBackend> {
    let _ = sink_id;
    engine.build_asr_sink(
        route_ids,
        sample_rate,
        channels,
        sample_format,
        chunk_frames,
        callback,
    )
}

#[pyfunction(name = "_create_wav_sink")]
fn create_wav_sink(
    mut engine: PyRefMut<'_, PyAudioEngineBackend>,
    sink_id: String,
    route_ids: Vec<String>,
    fd: i32,
    mix_gain: f32,
) -> PyResult<PyWavSinkBackend> {
    let _ = sink_id;
    engine.build_wav_sink(route_ids, fd, mix_gain)
}

fn append_microphone_info(list: &Bound<'_, PyList>, mic: MicInfo) -> PyResult<()> {
    let dict = PyDict::new(list.py());
    dict.set_item("id", mic.id)?;
    dict.set_item("name", mic.name)?;
    dict.set_item("is_default", mic.is_default)?;
    list.append(dict)?;
    Ok(())
}

fn append_display_info(list: &Bound<'_, PyList>, display: DisplayInfo) -> PyResult<()> {
    let dict = PyDict::new(list.py());
    dict.set_item("id", display.id)?;
    dict.set_item("name", display.name)?;
    dict.set_item("width", display.width)?;
    dict.set_item("height", display.height)?;
    dict.set_item("is_default", display.is_default)?;
    list.append(dict)?;
    Ok(())
}

fn append_application_info(list: &Bound<'_, PyList>, application: ApplicationInfo) -> PyResult<()> {
    let dict = PyDict::new(list.py());
    dict.set_item("pid", application.pid)?;
    dict.set_item("name", application.name)?;
    dict.set_item("bundle_id", application.bundle_id)?;
    dict.set_item("is_default", application.is_default)?;
    list.append(dict)?;
    Ok(())
}

fn dict_optional_u32(config: &Bound<'_, PyDict>, key: &str) -> PyResult<Option<u32>> {
    match config.get_item(key)? {
        Some(value) => value.extract::<Option<u32>>(),
        None => Ok(None),
    }
}

fn dict_bool_with_default(config: &Bound<'_, PyDict>, key: &str, default: bool) -> PyResult<bool> {
    Ok(config
        .get_item(key)?
        .map(|value| value.extract::<bool>())
        .transpose()?
        .unwrap_or(default))
}

fn dict_usize_with_default(
    config: &Bound<'_, PyDict>,
    key: &str,
    default: usize,
) -> PyResult<usize> {
    Ok(config
        .get_item(key)?
        .map(|value| value.extract::<usize>())
        .transpose()?
        .unwrap_or(default))
}

fn dict_u64_with_default(config: &Bound<'_, PyDict>, key: &str, default: u64) -> PyResult<u64> {
    Ok(config
        .get_item(key)?
        .map(|value| value.extract::<u64>())
        .transpose()?
        .unwrap_or(default))
}

fn dict_f32_with_default(config: &Bound<'_, PyDict>, key: &str, default: f32) -> PyResult<f32> {
    Ok(config
        .get_item(key)?
        .map(|value| value.extract::<f32>())
        .transpose()?
        .unwrap_or(default))
}

fn parse_sample_format(value: &str) -> PyResult<SampleFormat> {
    match value {
        "f32" | "F32" => Ok(SampleFormat::F32),
        "i16" | "I16" => Ok(SampleFormat::I16),
        _ => Err(PyValueError::new_err(format!(
            "unsupported sample_format '{value}', expected 'f32' or 'i16'"
        ))),
    }
}

fn engine_error(err: EngineError) -> PyErr {
    PyRuntimeError::new_err(err.to_string())
}

#[cfg(unix)]
fn duplicate_file_descriptor(fd: i32) -> std::io::Result<File> {
    if fd < 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "file descriptor must be non-negative",
        ));
    }

    unsafe extern "C" {
        fn dup(oldfd: c_int) -> c_int;
    }

    let duplicated = unsafe { dup(fd as c_int) };
    if duplicated < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        let file = unsafe { File::from_raw_fd(duplicated) };
        Ok(file)
    }
}

#[cfg(not(unix))]
fn duplicate_file_descriptor(_fd: i32) -> std::io::Result<File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "file descriptor based wav sink is only supported on unix platforms",
    ))
}

#[pymodule]
fn _macloop(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyAudioEngineBackend>()?;
    m.add_class::<PyAsrSinkBackend>()?;
    m.add_class::<PyWavSinkBackend>()?;
    m.add_class::<PyLatencyStats>()?;
    m.add_class::<PyPipelineStats>()?;
    m.add_class::<PyProcessorStats>()?;
    m.add_class::<PyStreamStats>()?;
    m.add_class::<PyAsrInputStats>()?;
    m.add_class::<PyWavSinkStats>()?;
    m.add_function(wrap_pyfunction!(list_microphones, m)?)?;
    m.add_function(wrap_pyfunction!(list_displays, m)?)?;
    m.add_function(wrap_pyfunction!(list_applications, m)?)?;
    m.add_function(wrap_pyfunction!(create_asr_sink, m)?)?;
    m.add_function(wrap_pyfunction!(create_wav_sink, m)?)?;
    Ok(())
}
