mod stats;

use core_engine::{
    AppAudioSource, AppAudioSourceConfig, ApplicationInfo, AsrChunkView, AsrSampleSlice, AsrSink,
    AsrSinkCallback, AsrSinkConfig, AsrSinkInput, AsrSinkMetricsSnapshot, AudioEngineController,
    AudioProcessor, DisplayInfo, EngineError, MicInfo, MicrophoneSource, MicrophoneSourceConfig,
    NodeMetrics, RouteConsumer, SampleFormat, SourceType, StreamFormat, SyntheticSource,
    SyntheticSourceConfig, SystemAudioSource, SystemAudioSourceConfig, WavFileOutput,
    WavSinkMetricsSnapshot,
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

type DetachedAsrStartResult = Result<AsrSink, (String, Vec<(String, RouteConsumer)>)>;

type DetachedWavStartResult = Result<WavFileOutput, (String, Vec<(String, RouteConsumer)>)>;

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

    fn close(&mut self, py: Python<'_>) -> PyResult<()> {
        let Some(mut sink) = self.sink.take() else {
            return Ok(());
        };

        let (stop_result, final_stats) = py.detach(move || {
            let stop_result = sink.stop().map_err(|e| e.to_string());
            let final_stats = sink.stats();
            (stop_result, final_stats)
        });
        self.final_stats = Some(final_stats);
        stop_result
            .map_err(|e| PyRuntimeError::new_err(format!("failed to stop asr sink: {e}")))?;
        Ok(())
    }
}

impl Drop for PyAsrSinkBackend {
    fn drop(&mut self) {
        let _ = Python::try_attach(|py| self.close(py));
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

    fn close(&mut self, py: Python<'_>) -> PyResult<()> {
        let Some(mut sink) = self.sink.take() else {
            return Ok(());
        };

        let (stop_result, final_stats) = py.detach(move || {
            let stop_result = sink.stop().map_err(|e| e.to_string());
            let final_stats = sink.stats();
            (stop_result, final_stats)
        });
        self.final_stats = Some(final_stats);
        stop_result
            .map_err(|e| PyRuntimeError::new_err(format!("failed to stop wav sink: {e}")))?;
        Ok(())
    }
}

impl Drop for PyWavSinkBackend {
    fn drop(&mut self) {
        let _ = Python::try_attach(|py| self.close(py));
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
        py: Python<'_>,
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

                let state = py
                    .detach(move || {
                        MicrophoneSource::new(
                            pipeline,
                            MicrophoneSourceConfig {
                                device_id,
                                vpio_enabled,
                            },
                        )
                        .map(|source| StreamRuntimeState {
                            runtime: StreamRuntime::Microphone(source),
                            started: false,
                        })
                        .map_err(|e| format!("failed to create microphone source: {e}"))
                    })
                    .map_err(PyRuntimeError::new_err)?;

                self.sources.insert(stream_id, state);
                Ok(())
            }
            "application_audio" => {
                let pids = dict_u32_list(&config, "pids")?;
                let display_id = dict_optional_u32(&config, "display_id")?;

                if pids.is_empty() {
                    return Err(PyValueError::new_err(
                        "application_audio source requires at least one pid in 'pids'",
                    ));
                }

                let pipeline = self
                    .controller
                    .create_stream(
                        stream_id.clone(),
                        SourceType::ApplicationAudio,
                        DEFAULT_MAX_PROCESSORS,
                        DEFAULT_MAX_OUTPUTS,
                    )
                    .map_err(engine_error)?;

                let state = py
                    .detach(move || {
                        AppAudioSource::new(pipeline, AppAudioSourceConfig { pids, display_id })
                            .map(|source| StreamRuntimeState {
                                runtime: StreamRuntime::AppAudio(source),
                                started: false,
                            })
                            .map_err(|e| format!("failed to create application audio source: {e}"))
                    })
                    .map_err(PyRuntimeError::new_err)?;

                self.sources.insert(stream_id, state);
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

                let state = py
                    .detach(move || {
                        SystemAudioSource::new(pipeline, SystemAudioSourceConfig { display_id })
                            .map(|source| StreamRuntimeState {
                                runtime: StreamRuntime::SystemAudio(source),
                                started: false,
                            })
                            .map_err(|e| format!("failed to create system audio source: {e}"))
                    })
                    .map_err(PyRuntimeError::new_err)?;

                self.sources.insert(stream_id, state);
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

                let state = py
                    .detach(move || {
                        SyntheticSource::new(
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
                        .map(|source| StreamRuntimeState {
                            runtime: StreamRuntime::Synthetic(source),
                            started: false,
                        })
                        .map_err(|e| format!("failed to create synthetic source: {e}"))
                    })
                    .map_err(PyValueError::new_err)?;

                self.sources.insert(stream_id, state);
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

    fn close(&mut self, py: Python<'_>) -> PyResult<()> {
        if self.closed {
            return Ok(());
        }

        let sources = std::mem::take(&mut self.sources);
        let stop_result = py.detach(move || -> Result<(), String> {
            let mut first_error: Option<String> = None;

            for (_, mut source) in sources {
                if !source.started {
                    continue;
                }
                if let Err(err) = source.runtime.stop() {
                    if first_error.is_none() {
                        first_error = Some(format!("failed to stop source: {err}"));
                    }
                }
            }

            if let Some(err) = first_error {
                Err(err)
            } else {
                Ok(())
            }
        });
        self.route_streams.clear();
        self.controller.tick_gc();
        self.closed = true;

        stop_result.map_err(PyRuntimeError::new_err)
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

    fn stream_ids_for_routes(&self, route_ids: &[String]) -> PyResult<Vec<String>> {
        let mut stream_ids = Vec::<String>::new();

        for route_id in route_ids {
            let stream_id = self.route_streams.get(route_id).ok_or_else(|| {
                PyValueError::new_err(format!("route '{route_id}' is not registered"))
            })?;

            if !stream_ids.iter().any(|existing| existing == stream_id) {
                stream_ids.push(stream_id.clone());
            }
        }

        Ok(stream_ids)
    }

    fn take_stream_states(
        &mut self,
        stream_ids: &[String],
    ) -> PyResult<Vec<(String, StreamRuntimeState)>> {
        for stream_id in stream_ids {
            if !self.sources.contains_key(stream_id) {
                return Err(PyValueError::new_err(format!(
                    "stream '{stream_id}' is not registered"
                )));
            }
        }

        let mut states = Vec::with_capacity(stream_ids.len());
        for stream_id in stream_ids {
            let state = self
                .sources
                .remove(stream_id)
                .expect("stream state presence checked before removal");
            states.push((stream_id.clone(), state));
        }
        Ok(states)
    }

    fn restore_stream_states(&mut self, states: Vec<(String, StreamRuntimeState)>) {
        for (stream_id, state) in states {
            self.sources.insert(stream_id, state);
        }
    }

    fn take_route_consumers(
        &mut self,
        route_ids: &[String],
    ) -> PyResult<Vec<(String, RouteConsumer)>> {
        self.ensure_route_consumers_available(route_ids)?;

        let mut consumers = Vec::with_capacity(route_ids.len());
        for route_id in route_ids {
            let consumer = self
                .controller
                .take_output_consumer(route_id)
                .expect("route consumer presence checked before removal");
            consumers.push((route_id.clone(), consumer));
        }
        Ok(consumers)
    }

    fn restore_route_consumers(&mut self, consumers: Vec<(String, RouteConsumer)>) {
        for (route_id, consumer) in consumers {
            let _ = self.controller.restore_output_consumer(route_id, consumer);
        }
    }

    fn build_asr_sink(
        &mut self,
        py: Python<'_>,
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

        let stream_ids = self.stream_ids_for_routes(&route_ids)?;
        let stream_states = self.take_stream_states(&stream_ids)?;
        let started_states = match py.detach(move || {
            let mut states = stream_states;
            let mut started_in_this_call = Vec::<usize>::new();
            for index in 0..states.len() {
                if states[index].1.started {
                    continue;
                }

                let stream_id = states[index].0.clone();
                if let Err(err) = states[index].1.runtime.start() {
                    for started_index in started_in_this_call {
                        let started_state = &mut states[started_index].1;
                        let _ = started_state.runtime.stop();
                        started_state.started = false;
                    }
                    return Err((
                        format!("failed to start source for stream '{stream_id}': {err}"),
                        states,
                    ));
                }
                states[index].1.started = true;
                started_in_this_call.push(index);
            }
            Ok(states)
        }) {
            Ok(states) => states,
            Err((err, states)) => {
                self.restore_stream_states(states);
                return Err(PyRuntimeError::new_err(err));
            }
        };
        self.restore_stream_states(started_states);

        let route_consumers = self.take_route_consumers(&route_ids)?;
        let detached_result: DetachedAsrStartResult = py.detach(move || {
            let inputs = route_consumers
                .into_iter()
                .map(|(route_id, consumer)| AsrSinkInput {
                    input_id: route_id,
                    consumer,
                })
                .collect();

            match AsrSink::try_spawn(
                inputs,
                AsrSinkConfig {
                    format,
                    chunk_frames,
                },
                Box::new(PythonAsrCallback { callback }),
            ) {
                Ok(sink) => Ok(sink),
                Err((err, inputs)) => Err((
                    format!("failed to create asr sink: {err}"),
                    inputs
                        .into_iter()
                        .map(|input| (input.input_id, input.consumer))
                        .collect(),
                )),
            }
        });

        match detached_result {
            Ok(sink) => Ok(PyAsrSinkBackend {
                sink: Some(sink),
                final_stats: None,
            }),
            Err((err, route_consumers)) => {
                self.restore_route_consumers(route_consumers);
                Err(PyRuntimeError::new_err(err))
            }
        }
    }

    fn build_wav_sink(
        &mut self,
        py: Python<'_>,
        route_ids: Vec<String>,
        fd: i32,
        mix_gain: f32,
    ) -> PyResult<PyWavSinkBackend> {
        self.ensure_open()?;
        self.ensure_route_consumers_available(&route_ids)?;

        let file = duplicate_file_descriptor(fd)
            .map_err(|e| PyOSError::new_err(format!("failed to duplicate file descriptor: {e}")))?;
        let stream_ids = self.stream_ids_for_routes(&route_ids)?;
        let stream_states = self.take_stream_states(&stream_ids)?;
        let started_states = match py.detach(move || {
            let mut states = stream_states;
            let mut started_in_this_call = Vec::<usize>::new();
            for index in 0..states.len() {
                if states[index].1.started {
                    continue;
                }

                let stream_id = states[index].0.clone();
                if let Err(err) = states[index].1.runtime.start() {
                    for started_index in started_in_this_call {
                        let started_state = &mut states[started_index].1;
                        let _ = started_state.runtime.stop();
                        started_state.started = false;
                    }
                    return Err((
                        format!("failed to start source for stream '{stream_id}': {err}"),
                        states,
                    ));
                }
                states[index].1.started = true;
                started_in_this_call.push(index);
            }
            Ok(states)
        }) {
            Ok(states) => states,
            Err((err, states)) => {
                self.restore_stream_states(states);
                return Err(PyRuntimeError::new_err(err));
            }
        };
        self.restore_stream_states(started_states);

        let route_consumers = self.take_route_consumers(&route_ids)?;
        let master_format = self.controller.master_format();
        let detached_result: DetachedWavStartResult = py.detach(move || {
            let consumers = route_consumers
                .into_iter()
                .map(|(_, consumer)| consumer)
                .collect();

            match WavFileOutput::try_spawn_file_mix(file, master_format, consumers, mix_gain) {
                Ok(sink) => Ok(sink),
                Err((err, consumers)) => Err((
                    format!("failed to create wav sink: {err}"),
                    route_ids.into_iter().zip(consumers).collect(),
                )),
            }
        });

        match detached_result {
            Ok(sink) => Ok(PyWavSinkBackend {
                sink: Some(sink),
                final_stats: None,
            }),
            Err((err, route_consumers)) => {
                self.restore_route_consumers(route_consumers);
                Err(PyRuntimeError::new_err(err))
            }
        }
    }
}

impl Drop for PyAudioEngineBackend {
    fn drop(&mut self) {
        let _ = Python::try_attach(|py| self.close(py));
    }
}

#[pyfunction]
fn list_microphones(py: Python<'_>) -> PyResult<Bound<'_, PyList>> {
    let microphones = py.detach(MicrophoneSource::list_mics);
    let list = PyList::empty(py);
    for mic in microphones {
        append_microphone_info(&list, mic)?;
    }
    Ok(list)
}

#[pyfunction]
fn list_displays(py: Python<'_>) -> PyResult<Bound<'_, PyList>> {
    let displays = py.detach(SystemAudioSource::list_displays);
    let list = PyList::empty(py);
    for display in displays {
        append_display_info(&list, display)?;
    }
    Ok(list)
}

#[pyfunction]
fn list_applications(py: Python<'_>) -> PyResult<Bound<'_, PyList>> {
    let applications = py.detach(AppAudioSource::list_applications);
    let list = PyList::empty(py);
    for application in applications {
        append_application_info(&list, application)?;
    }
    Ok(list)
}

#[pyfunction(name = "_create_asr_sink")]
fn create_asr_sink(
    py: Python<'_>,
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
        py,
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
    py: Python<'_>,
    mut engine: PyRefMut<'_, PyAudioEngineBackend>,
    sink_id: String,
    route_ids: Vec<String>,
    fd: i32,
    mix_gain: f32,
) -> PyResult<PyWavSinkBackend> {
    let _ = sink_id;
    engine.build_wav_sink(py, route_ids, fd, mix_gain)
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

fn dict_u32_list(config: &Bound<'_, PyDict>, key: &str) -> PyResult<Vec<u32>> {
    match config.get_item(key)? {
        Some(value) => value.extract::<Vec<u32>>(),
        None => Ok(Vec::new()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use pyo3::types::PyDict;

    fn with_python<T>(f: impl FnOnce(Python<'_>) -> T) -> T {
        Python::initialize();
        Python::attach(f)
    }

    #[test]
    fn parse_sample_format_accepts_supported_values() {
        assert!(matches!(parse_sample_format("f32"), Ok(SampleFormat::F32)));
        assert!(matches!(parse_sample_format("F32"), Ok(SampleFormat::F32)));
        assert!(matches!(parse_sample_format("i16"), Ok(SampleFormat::I16)));
        assert!(matches!(parse_sample_format("I16"), Ok(SampleFormat::I16)));
    }

    #[test]
    fn parse_sample_format_rejects_unsupported_values() {
        Python::initialize();
        let err = parse_sample_format("u8").unwrap_err();
        assert!(err.to_string().contains("unsupported sample_format 'u8'"));
    }

    #[test]
    fn duplicate_file_descriptor_rejects_negative_fd() {
        let err = duplicate_file_descriptor(-1).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn dict_helpers_extract_values_and_defaults() {
        with_python(|py| {
            let dict = PyDict::new(py);
            dict.set_item("device_id", 7).unwrap();
            dict.set_item("pids", vec![11_u32, 22_u32]).unwrap();
            dict.set_item("enabled", true).unwrap();
            dict.set_item("size", 123_usize).unwrap();
            dict.set_item("delay_ms", 45_u64).unwrap();
            dict.set_item("gain", 1.5_f32).unwrap();

            assert_eq!(dict_optional_u32(&dict, "device_id").unwrap(), Some(7));
            assert_eq!(dict_optional_u32(&dict, "missing").unwrap(), None);
            assert_eq!(dict_u32_list(&dict, "pids").unwrap(), vec![11, 22]);
            assert_eq!(dict_u32_list(&dict, "missing").unwrap(), Vec::<u32>::new());
            assert!(dict_bool_with_default(&dict, "enabled", false).unwrap());
            assert!(dict_bool_with_default(&dict, "missing", true).unwrap());
            assert_eq!(dict_usize_with_default(&dict, "size", 9).unwrap(), 123);
            assert_eq!(dict_usize_with_default(&dict, "missing", 9).unwrap(), 9);
            assert_eq!(dict_u64_with_default(&dict, "delay_ms", 1).unwrap(), 45);
            assert_eq!(dict_u64_with_default(&dict, "missing", 1).unwrap(), 1);
            assert_eq!(dict_f32_with_default(&dict, "gain", 0.5).unwrap(), 1.5);
            assert_eq!(dict_f32_with_default(&dict, "missing", 0.5).unwrap(), 0.5);
        });
    }

    #[test]
    fn backend_rejects_unsupported_source_kind() {
        with_python(|py| {
            let mut backend = PyAudioEngineBackend::new();
            let config = PyDict::new(py);
            let err = backend
                .create_stream(py, "stream".to_string(), "nope".to_string(), config)
                .unwrap_err();
            assert!(err.to_string().contains("unsupported source_kind 'nope'"));
        });
    }

    #[test]
    fn backend_requires_application_audio_pids() {
        with_python(|py| {
            let mut backend = PyAudioEngineBackend::new();
            let config = PyDict::new(py);
            let err = backend
                .create_stream(
                    py,
                    "app_stream".to_string(),
                    "application_audio".to_string(),
                    config,
                )
                .unwrap_err();
            assert!(err
                .to_string()
                .contains("requires at least one pid in 'pids'"));
        });
    }

    #[test]
    fn backend_rejects_unsupported_processor_kind() {
        with_python(|py| {
            let mut backend = PyAudioEngineBackend::new();
            let config = PyDict::new(py);
            let err = backend
                .add_processor(
                    "stream".to_string(),
                    "processor".to_string(),
                    "nope".to_string(),
                    config,
                )
                .unwrap_err();
            assert!(err
                .to_string()
                .contains("unsupported processor_kind 'nope'"));
        });
    }

    #[test]
    fn backend_close_marks_engine_closed() {
        with_python(|py| {
            let mut backend = PyAudioEngineBackend::new();
            backend.close(py).unwrap();
            let err = backend.ensure_open().unwrap_err();
            assert!(err.to_string().contains("audio engine backend is closed"));
        });
    }
}
