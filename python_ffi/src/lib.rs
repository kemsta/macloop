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
use pyo3::exceptions::{PyOSError, PyRuntimeError, PyTimeoutError, PyValueError};
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
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_COMMAND_CAPACITY: usize = 32;
const DEFAULT_GARBAGE_CAPACITY: usize = 32;
const DEFAULT_ROUTE_CAPACITY: usize = 4096;
const DEFAULT_MAX_PROCESSORS: usize = 32;
const DEFAULT_MAX_OUTPUTS: usize = 16;
const NATIVE_SOURCE_START_TIMEOUT: Duration = Duration::from_secs(10);
const NATIVE_SOURCE_STOP_TIMEOUT: Duration = Duration::from_secs(2);

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

struct StreamRuntimeState {
    runtime: StreamRuntime,
    started: bool,
}

type DeferredSourceResult<T> = Arc<Mutex<Option<Result<T, (T, String)>>>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingCleanupPhase {
    Starting,
    ReadyToStop,
    Stopping,
}

struct PendingCleanup<T> {
    phase: PendingCleanupPhase,
    completion: DeferredSourceResult<T>,
    ready: mpsc::Receiver<()>,
}

impl<T> PendingCleanup<T> {
    fn ready_to_stop(result: Result<T, (T, String)>) -> Self {
        let (_ready_tx, ready) = mpsc::sync_channel(1);
        Self {
            phase: PendingCleanupPhase::ReadyToStop,
            completion: Arc::new(Mutex::new(Some(result))),
            ready,
        }
    }
}

enum PendingRuntimeCleanup {
    AppAudio(PendingCleanup<AppAudioSource>),
    Microphone(PendingCleanup<MicrophoneSource>),
    SystemAudio(PendingCleanup<SystemAudioSource>),
}

enum PendingRuntimeCleanupProgress {
    Cleaned,
    Failed(String),
    Pending(PendingRuntimeCleanup),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BackendPoisonState {
    reason: String,
    timed_out_stream_ids: Vec<String>,
}

impl BackendPoisonState {
    fn new(reason: String, timed_out_stream_ids: Vec<String>) -> Self {
        Self {
            reason,
            timed_out_stream_ids,
        }
    }

    fn runtime_error_message(&self) -> String {
        if self.timed_out_stream_ids.is_empty() {
            format!(
                "audio engine backend is unusable after a native startup failure: {}",
                self.reason
            )
        } else {
            format!(
                "audio engine backend is unusable after a native startup timeout: {} (timed out stream(s): {})",
                self.reason,
                self.timed_out_stream_ids.join(", "),
            )
        }
    }
}

struct RollbackError {
    poison: BackendPoisonState,
    cleanup: HashMap<String, PendingRuntimeCleanup>,
}

struct StartStreamsError {
    message: String,
    states: Vec<(String, StreamRuntimeState)>,
    poison: Option<BackendPoisonState>,
    cleanup: HashMap<String, PendingRuntimeCleanup>,
}

enum StartStreamStateError {
    Recoverable {
        state: StreamRuntimeState,
        message: String,
    },
    Fatal {
        state: Option<StreamRuntimeState>,
        cleanup: Option<PendingRuntimeCleanup>,
        message: String,
    },
}

enum TimedSourceStartError<T> {
    Recoverable {
        source: T,
        message: String,
    },
    Fatal {
        source: Option<T>,
        cleanup: Option<PendingCleanup<T>>,
        message: String,
    },
}

enum TimedSourceStopError<T> {
    Recoverable {
        source: T,
        message: String,
    },
    Fatal {
        source: Option<T>,
        cleanup: Option<PendingCleanup<T>>,
        message: String,
    },
}

enum StopStreamRuntimeError {
    Recoverable {
        runtime: StreamRuntime,
        message: String,
    },
    Fatal {
        runtime: Option<StreamRuntime>,
        cleanup: Option<PendingRuntimeCleanup>,
        message: String,
    },
}

fn runtime_stop_priority(runtime: &StreamRuntime) -> u8 {
    match runtime {
        StreamRuntime::Microphone(_) => 0,
        StreamRuntime::AppAudio(_) | StreamRuntime::SystemAudio(_) => 1,
        StreamRuntime::Synthetic(_) => 2,
    }
}

fn remaining_deadline(deadline: Instant) -> Option<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
}

fn lifecycle_timeout_message(action: &str, label: &str, timeout: Duration) -> String {
    format!(
        "timed out {action} {label} before the {}s lifecycle deadline elapsed",
        timeout.as_secs()
    )
}

fn take_deferred_source_result<T>(
    completion: &DeferredSourceResult<T>,
) -> Option<Result<T, (T, String)>> {
    completion
        .lock()
        .expect("deferred source result mutex should not be poisoned")
        .take()
}

fn wait_for_deferred_source_result<T>(
    completion: &DeferredSourceResult<T>,
    ready: &mpsc::Receiver<()>,
    deadline: Instant,
) -> Option<Result<T, (T, String)>> {
    if let Some(result) = take_deferred_source_result(completion) {
        return Some(result);
    }

    let timeout = remaining_deadline(deadline)?;
    match ready.recv_timeout(timeout) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => take_deferred_source_result(completion),
        Err(RecvTimeoutError::Timeout) => None,
    }
}

fn start_native_source_with_deadline<T, E, FStart>(
    mut source: T,
    label: &'static str,
    deadline: Instant,
    start: FStart,
) -> Result<T, TimedSourceStartError<T>>
where
    T: Send + 'static,
    E: ToString + Send + 'static,
    FStart: Fn(&mut T) -> Result<(), E> + Send + Copy + 'static,
{
    let Some(timeout) = remaining_deadline(deadline) else {
        return Err(TimedSourceStartError::Fatal {
            source: Some(source),
            cleanup: None,
            message: lifecycle_timeout_message("starting", label, NATIVE_SOURCE_START_TIMEOUT),
        });
    };

    let completion: DeferredSourceResult<T> = Arc::new(Mutex::new(None));
    let completion_worker = Arc::clone(&completion);
    let (ready_tx, ready) = mpsc::sync_channel(1);

    thread::spawn(move || {
        let result = match start(&mut source) {
            Ok(()) => Ok(source),
            Err(err) => Err((source, err.to_string())),
        };

        *completion_worker
            .lock()
            .expect("deferred source result mutex should not be poisoned") = Some(result);
        let _ = ready_tx.send(());
    });

    match ready.recv_timeout(timeout) {
        Ok(()) => match take_deferred_source_result(&completion)
            .expect("source start completion should be available after readiness signal")
        {
            Ok(source) => Ok(source),
            Err((source, message)) => Err(TimedSourceStartError::Recoverable { source, message }),
        },
        Err(RecvTimeoutError::Timeout) => Err(TimedSourceStartError::Fatal {
            source: None,
            cleanup: Some(PendingCleanup {
                phase: PendingCleanupPhase::Starting,
                completion,
                ready,
            }),
            message: lifecycle_timeout_message("starting", label, NATIVE_SOURCE_START_TIMEOUT),
        }),
        Err(RecvTimeoutError::Disconnected) => match take_deferred_source_result(&completion) {
            Some(Ok(source)) => Ok(source),
            Some(Err((source, message))) => {
                Err(TimedSourceStartError::Recoverable { source, message })
            }
            None => Err(TimedSourceStartError::Fatal {
                source: None,
                cleanup: None,
                message: format!("{label} start worker disconnected unexpectedly"),
            }),
        },
    }
}

fn progress_pending_cleanup<T, E, FStop>(
    handle: PendingCleanup<T>,
    label: &'static str,
    deadline: Instant,
    stop: FStop,
    wrap_cleanup: impl FnOnce(PendingCleanup<T>) -> PendingRuntimeCleanup + Copy,
) -> PendingRuntimeCleanupProgress
where
    T: Send + 'static,
    E: ToString + Send + 'static,
    FStop: Fn(&mut T) -> Result<(), E> + Send + Copy + 'static,
{
    match handle.phase {
        PendingCleanupPhase::Starting | PendingCleanupPhase::ReadyToStop => {
            match wait_for_deferred_source_result(&handle.completion, &handle.ready, deadline) {
                Some(Ok(source)) => {
                    match stop_native_source_with_deadline(source, label, deadline, stop) {
                        Ok(_source) => PendingRuntimeCleanupProgress::Cleaned,
                        Err(TimedSourceStopError::Recoverable {
                            source: _source,
                            message,
                        }) => PendingRuntimeCleanupProgress::Failed(format!(
                            "failed to stop timed out {label}: {message}"
                        )),
                        Err(TimedSourceStopError::Fatal {
                            source: _source,
                            cleanup: Some(cleanup),
                            message: _,
                        }) => PendingRuntimeCleanupProgress::Pending(wrap_cleanup(cleanup)),
                        Err(TimedSourceStopError::Fatal {
                            source: _source,
                            cleanup: None,
                            message,
                        }) => PendingRuntimeCleanupProgress::Failed(format!(
                            "failed to stop timed out {label}: {message}"
                        )),
                    }
                }
                Some(Err((_source, _message))) => PendingRuntimeCleanupProgress::Cleaned,
                None => PendingRuntimeCleanupProgress::Pending(wrap_cleanup(handle)),
            }
        }
        PendingCleanupPhase::Stopping => {
            match wait_for_deferred_source_result(&handle.completion, &handle.ready, deadline) {
                Some(Ok(_source)) => PendingRuntimeCleanupProgress::Cleaned,
                Some(Err((_source, message))) => PendingRuntimeCleanupProgress::Failed(format!(
                    "failed to finish stopping timed out {label}: {message}"
                )),
                None => PendingRuntimeCleanupProgress::Pending(wrap_cleanup(handle)),
            }
        }
    }
}

impl PendingRuntimeCleanup {
    fn try_cleanup_before(self, deadline: Instant) -> PendingRuntimeCleanupProgress {
        match self {
            Self::AppAudio(handle) => progress_pending_cleanup(
                handle,
                "application audio source",
                deadline,
                AppAudioSource::stop,
                PendingRuntimeCleanup::AppAudio,
            ),
            Self::Microphone(handle) => progress_pending_cleanup(
                handle,
                "microphone source",
                deadline,
                MicrophoneSource::stop,
                PendingRuntimeCleanup::Microphone,
            ),
            Self::SystemAudio(handle) => progress_pending_cleanup(
                handle,
                "system audio source",
                deadline,
                SystemAudioSource::stop,
                PendingRuntimeCleanup::SystemAudio,
            ),
        }
    }
}

fn pending_cleanup_from_ready_runtime(runtime: StreamRuntime) -> Option<PendingRuntimeCleanup> {
    match runtime {
        StreamRuntime::AppAudio(source) => Some(PendingRuntimeCleanup::AppAudio(
            PendingCleanup::ready_to_stop(Ok(source)),
        )),
        StreamRuntime::Microphone(source) => Some(PendingRuntimeCleanup::Microphone(
            PendingCleanup::ready_to_stop(Ok(source)),
        )),
        StreamRuntime::SystemAudio(source) => Some(PendingRuntimeCleanup::SystemAudio(
            PendingCleanup::ready_to_stop(Ok(source)),
        )),
        StreamRuntime::Synthetic(_) => None,
    }
}

fn cleanup_pending_cleanups_with_deadline(
    cleanups: HashMap<String, PendingRuntimeCleanup>,
    deadline: Instant,
) -> (HashMap<String, PendingRuntimeCleanup>, Option<String>) {
    let mut remaining = HashMap::new();
    let mut first_error = None;

    for (stream_id, cleanup) in cleanups {
        match cleanup.try_cleanup_before(deadline) {
            PendingRuntimeCleanupProgress::Cleaned => {}
            PendingRuntimeCleanupProgress::Failed(message) => {
                if first_error.is_none() {
                    first_error = Some(format!(
                        "failed to clean up timed out source for stream '{stream_id}': {message}"
                    ));
                }
            }
            PendingRuntimeCleanupProgress::Pending(cleanup) => {
                if first_error.is_none() {
                    first_error = Some(format!(
                        "timed out waiting for cleanup of stream '{stream_id}' before the {}s lifecycle deadline elapsed",
                        NATIVE_SOURCE_STOP_TIMEOUT.as_secs()
                    ));
                }
                remaining.insert(stream_id, cleanup);
            }
        }
    }

    (remaining, first_error)
}

fn start_stream_state_with_deadline(
    state: StreamRuntimeState,
    deadline: Instant,
) -> Result<StreamRuntimeState, StartStreamStateError> {
    if state.started {
        return Ok(state);
    }

    match state.runtime {
        StreamRuntime::AppAudio(source) => match start_native_source_with_deadline(
            source,
            "application audio source",
            deadline,
            AppAudioSource::start,
        ) {
            Ok(source) => Ok(StreamRuntimeState {
                runtime: StreamRuntime::AppAudio(source),
                started: true,
            }),
            Err(TimedSourceStartError::Recoverable { source, message }) => {
                Err(StartStreamStateError::Recoverable {
                    state: StreamRuntimeState {
                        runtime: StreamRuntime::AppAudio(source),
                        started: false,
                    },
                    message,
                })
            }
            Err(TimedSourceStartError::Fatal {
                source,
                cleanup,
                message,
            }) => Err(StartStreamStateError::Fatal {
                state: source.map(|source| StreamRuntimeState {
                    runtime: StreamRuntime::AppAudio(source),
                    started: false,
                }),
                cleanup: cleanup.map(PendingRuntimeCleanup::AppAudio),
                message,
            }),
        },
        StreamRuntime::SystemAudio(source) => match start_native_source_with_deadline(
            source,
            "system audio source",
            deadline,
            SystemAudioSource::start,
        ) {
            Ok(source) => Ok(StreamRuntimeState {
                runtime: StreamRuntime::SystemAudio(source),
                started: true,
            }),
            Err(TimedSourceStartError::Recoverable { source, message }) => {
                Err(StartStreamStateError::Recoverable {
                    state: StreamRuntimeState {
                        runtime: StreamRuntime::SystemAudio(source),
                        started: false,
                    },
                    message,
                })
            }
            Err(TimedSourceStartError::Fatal {
                source,
                cleanup,
                message,
            }) => Err(StartStreamStateError::Fatal {
                state: source.map(|source| StreamRuntimeState {
                    runtime: StreamRuntime::SystemAudio(source),
                    started: false,
                }),
                cleanup: cleanup.map(PendingRuntimeCleanup::SystemAudio),
                message,
            }),
        },
        StreamRuntime::Microphone(source) => match start_native_source_with_deadline(
            source,
            "microphone source",
            deadline,
            MicrophoneSource::start,
        ) {
            Ok(source) => Ok(StreamRuntimeState {
                runtime: StreamRuntime::Microphone(source),
                started: true,
            }),
            Err(TimedSourceStartError::Recoverable { source, message }) => {
                Err(StartStreamStateError::Recoverable {
                    state: StreamRuntimeState {
                        runtime: StreamRuntime::Microphone(source),
                        started: false,
                    },
                    message,
                })
            }
            Err(TimedSourceStartError::Fatal {
                source,
                cleanup,
                message,
            }) => Err(StartStreamStateError::Fatal {
                state: source.map(|source| StreamRuntimeState {
                    runtime: StreamRuntime::Microphone(source),
                    started: false,
                }),
                cleanup: cleanup.map(PendingRuntimeCleanup::Microphone),
                message,
            }),
        },
        StreamRuntime::Synthetic(mut source) => match source.start() {
            Ok(()) => Ok(StreamRuntimeState {
                runtime: StreamRuntime::Synthetic(source),
                started: true,
            }),
            Err(message) => Err(StartStreamStateError::Recoverable {
                state: StreamRuntimeState {
                    runtime: StreamRuntime::Synthetic(source),
                    started: false,
                },
                message: message.to_string(),
            }),
        },
    }
}

fn stop_native_source_with_deadline<T, E, FStop>(
    source: T,
    label: &'static str,
    deadline: Instant,
    stop: FStop,
) -> Result<T, TimedSourceStopError<T>>
where
    T: Send + 'static,
    E: ToString + Send + 'static,
    FStop: Fn(&mut T) -> Result<(), E> + Send + Copy + 'static,
{
    let Some(timeout) = remaining_deadline(deadline) else {
        return Err(TimedSourceStopError::Fatal {
            source: Some(source),
            cleanup: None,
            message: lifecycle_timeout_message("stopping", label, NATIVE_SOURCE_STOP_TIMEOUT),
        });
    };

    let completion: DeferredSourceResult<T> = Arc::new(Mutex::new(None));
    let completion_worker = Arc::clone(&completion);
    let (ready_tx, ready) = mpsc::sync_channel(1);

    thread::spawn(move || {
        let mut source = source;
        let result = match stop(&mut source) {
            Ok(()) => Ok(source),
            Err(err) => Err((source, err.to_string())),
        };

        *completion_worker
            .lock()
            .expect("deferred source result mutex should not be poisoned") = Some(result);
        let _ = ready_tx.send(());
    });

    match ready.recv_timeout(timeout) {
        Ok(()) => match take_deferred_source_result(&completion)
            .expect("source stop completion should be available after readiness signal")
        {
            Ok(source) => Ok(source),
            Err((source, message)) => Err(TimedSourceStopError::Recoverable { source, message }),
        },
        Err(RecvTimeoutError::Timeout) => Err(TimedSourceStopError::Fatal {
            source: None,
            cleanup: Some(PendingCleanup {
                phase: PendingCleanupPhase::Stopping,
                completion,
                ready,
            }),
            message: lifecycle_timeout_message("stopping", label, NATIVE_SOURCE_STOP_TIMEOUT),
        }),
        Err(RecvTimeoutError::Disconnected) => match take_deferred_source_result(&completion) {
            Some(Ok(source)) => Ok(source),
            Some(Err((source, message))) => {
                Err(TimedSourceStopError::Recoverable { source, message })
            }
            None => Err(TimedSourceStopError::Fatal {
                source: None,
                cleanup: None,
                message: format!("{label} stop worker disconnected unexpectedly"),
            }),
        },
    }
}

fn stop_stream_runtime_with_deadline(
    runtime: StreamRuntime,
    deadline: Instant,
) -> Result<StreamRuntime, StopStreamRuntimeError> {
    match runtime {
        StreamRuntime::AppAudio(source) => match stop_native_source_with_deadline(
            source,
            "application audio source",
            deadline,
            AppAudioSource::stop,
        ) {
            Ok(source) => Ok(StreamRuntime::AppAudio(source)),
            Err(TimedSourceStopError::Recoverable { source, message }) => {
                Err(StopStreamRuntimeError::Recoverable {
                    runtime: StreamRuntime::AppAudio(source),
                    message,
                })
            }
            Err(TimedSourceStopError::Fatal {
                source,
                cleanup,
                message,
            }) => Err(StopStreamRuntimeError::Fatal {
                runtime: source.map(StreamRuntime::AppAudio),
                cleanup: cleanup.map(PendingRuntimeCleanup::AppAudio),
                message,
            }),
        },
        StreamRuntime::SystemAudio(source) => match stop_native_source_with_deadline(
            source,
            "system audio source",
            deadline,
            SystemAudioSource::stop,
        ) {
            Ok(source) => Ok(StreamRuntime::SystemAudio(source)),
            Err(TimedSourceStopError::Recoverable { source, message }) => {
                Err(StopStreamRuntimeError::Recoverable {
                    runtime: StreamRuntime::SystemAudio(source),
                    message,
                })
            }
            Err(TimedSourceStopError::Fatal {
                source,
                cleanup,
                message,
            }) => Err(StopStreamRuntimeError::Fatal {
                runtime: source.map(StreamRuntime::SystemAudio),
                cleanup: cleanup.map(PendingRuntimeCleanup::SystemAudio),
                message,
            }),
        },
        StreamRuntime::Microphone(source) => match stop_native_source_with_deadline(
            source,
            "microphone source",
            deadline,
            MicrophoneSource::stop,
        ) {
            Ok(source) => Ok(StreamRuntime::Microphone(source)),
            Err(TimedSourceStopError::Recoverable { source, message }) => {
                Err(StopStreamRuntimeError::Recoverable {
                    runtime: StreamRuntime::Microphone(source),
                    message,
                })
            }
            Err(TimedSourceStopError::Fatal {
                source,
                cleanup,
                message,
            }) => Err(StopStreamRuntimeError::Fatal {
                runtime: source.map(StreamRuntime::Microphone),
                cleanup: cleanup.map(PendingRuntimeCleanup::Microphone),
                message,
            }),
        },
        StreamRuntime::Synthetic(mut source) => match source.stop() {
            Ok(()) => Ok(StreamRuntime::Synthetic(source)),
            Err(message) => Err(StopStreamRuntimeError::Recoverable {
                runtime: StreamRuntime::Synthetic(source),
                message: message.to_string(),
            }),
        },
    }
}

fn merge_timed_out_stream_ids(mut timed_out_stream_ids: Vec<String>) -> Vec<String> {
    timed_out_stream_ids.sort();
    timed_out_stream_ids.dedup();
    timed_out_stream_ids
}

fn rollback_started_states_with_deadline(
    states: &mut Vec<(String, StreamRuntimeState)>,
    started_stream_ids: &[String],
    deadline: Instant,
) -> Result<(), RollbackError> {
    for stream_id in started_stream_ids.iter().rev() {
        let Some(index) = states
            .iter()
            .position(|(existing_stream_id, _)| existing_stream_id == stream_id)
        else {
            continue;
        };

        let (_, state) = states.remove(index);
        match stop_stream_runtime_with_deadline(state.runtime, deadline) {
            Ok(runtime) => {
                states.insert(
                    index,
                    (
                        stream_id.clone(),
                        StreamRuntimeState {
                            runtime,
                            started: false,
                        },
                    ),
                );
            }
            Err(StopStreamRuntimeError::Recoverable { runtime, message }) => {
                states.insert(
                    index,
                    (
                        stream_id.clone(),
                        StreamRuntimeState {
                            runtime,
                            started: true,
                        },
                    ),
                );
                return Err(RollbackError {
                    poison: BackendPoisonState::new(
                        format!("failed to roll back source for stream '{stream_id}': {message}"),
                        Vec::new(),
                    ),
                    cleanup: HashMap::new(),
                });
            }
            Err(StopStreamRuntimeError::Fatal {
                runtime,
                cleanup,
                message,
            }) => {
                if let Some(runtime) = runtime {
                    states.insert(
                        index,
                        (
                            stream_id.clone(),
                            StreamRuntimeState {
                                runtime,
                                started: true,
                            },
                        ),
                    );
                }
                return Err(RollbackError {
                    poison: BackendPoisonState::new(
                        format!("failed to roll back source for stream '{stream_id}': {message}"),
                        if cleanup.is_some() {
                            vec![stream_id.clone()]
                        } else {
                            Vec::new()
                        },
                    ),
                    cleanup: cleanup
                        .into_iter()
                        .map(|cleanup| (stream_id.clone(), cleanup))
                        .collect::<HashMap<_, _>>(),
                });
            }
        }
    }

    Ok(())
}

fn start_stream_states_with_timeout(
    mut states: Vec<(String, StreamRuntimeState)>,
    timeout: Duration,
) -> Result<Vec<(String, StreamRuntimeState)>, StartStreamsError> {
    let deadline = Instant::now() + timeout;
    let mut started_stream_ids = Vec::<String>::new();
    let mut index = 0;

    while index < states.len() {
        if states[index].1.started {
            index += 1;
            continue;
        }

        let stream_id = states[index].0.clone();
        let (_, state) = states.remove(index);
        match start_stream_state_with_deadline(state, deadline) {
            Ok(state) => {
                states.insert(index, (stream_id.clone(), state));
                started_stream_ids.push(stream_id);
                index += 1;
            }
            Err(StartStreamStateError::Recoverable { state, message }) => {
                states.insert(index, (stream_id.clone(), state));
                let failure_message =
                    format!("failed to start source for stream '{stream_id}': {message}");

                match rollback_started_states_with_deadline(
                    &mut states,
                    &started_stream_ids,
                    deadline,
                ) {
                    Ok(()) => {
                        return Err(StartStreamsError {
                            message: failure_message,
                            states,
                            poison: None,
                            cleanup: HashMap::new(),
                        });
                    }
                    Err(rollback) => {
                        let combined_message =
                            format!("{failure_message}; {}", rollback.poison.reason);
                        return Err(StartStreamsError {
                            message: combined_message.clone(),
                            states,
                            poison: Some(BackendPoisonState::new(
                                combined_message,
                                rollback.poison.timed_out_stream_ids,
                            )),
                            cleanup: rollback.cleanup,
                        });
                    }
                }
            }
            Err(StartStreamStateError::Fatal {
                state,
                cleanup,
                message,
            }) => {
                if let Some(state) = state {
                    states.insert(index, (stream_id.clone(), state));
                }

                let failure_message =
                    format!("failed to start source for stream '{stream_id}': {message}");
                let mut cleanup_handles = cleanup
                    .into_iter()
                    .map(|cleanup| (stream_id.clone(), cleanup))
                    .collect::<HashMap<_, _>>();
                let mut timed_out_stream_ids = if cleanup_handles.is_empty() {
                    Vec::new()
                } else {
                    vec![stream_id.clone()]
                };

                if let Err(rollback) = rollback_started_states_with_deadline(
                    &mut states,
                    &started_stream_ids,
                    deadline,
                ) {
                    cleanup_handles.extend(rollback.cleanup);
                    timed_out_stream_ids.extend(rollback.poison.timed_out_stream_ids);
                    let combined_message = format!("{failure_message}; {}", rollback.poison.reason);
                    return Err(StartStreamsError {
                        message: combined_message.clone(),
                        states,
                        poison: Some(BackendPoisonState::new(
                            combined_message,
                            merge_timed_out_stream_ids(timed_out_stream_ids),
                        )),
                        cleanup: cleanup_handles,
                    });
                }

                return Err(StartStreamsError {
                    message: failure_message.clone(),
                    states,
                    poison: Some(BackendPoisonState::new(
                        failure_message,
                        merge_timed_out_stream_ids(timed_out_stream_ids),
                    )),
                    cleanup: cleanup_handles,
                });
            }
        }
    }

    Ok(states)
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

    #[pyo3(signature = (engine=None))]
    fn close(
        &mut self,
        py: Python<'_>,
        mut engine: Option<PyRefMut<'_, PyAudioEngineBackend>>,
    ) -> PyResult<()> {
        let Some(mut sink) = self.sink.take() else {
            return Ok(());
        };

        let (stop_result, final_stats) = py.detach(move || {
            let stop_result = sink.stop().map_err(|e| e.to_string());
            let final_stats = sink.stats();
            (stop_result, final_stats)
        });
        self.final_stats = Some(final_stats);
        let route_consumers = stop_result
            .map_err(|e| PyRuntimeError::new_err(format!("failed to stop asr sink: {e}")))?;

        if let Some(engine) = engine.as_mut() {
            engine
                .restore_route_consumers(
                    route_consumers
                        .into_iter()
                        .map(|input| (input.input_id, input.consumer))
                        .collect(),
                )
                .map_err(|e| PyRuntimeError::new_err(format!("failed to restore asr sink routes: {e}")))?;
        }
        Ok(())
    }

    fn close_no_restore(&mut self, py: Python<'_>) -> PyResult<()> {
        let Some(mut sink) = self.sink.take() else {
            return Ok(());
        };

        let (stop_result, final_stats) = py.detach(move || {
            let stop_result = sink.stop().map_err(|e| e.to_string());
            let final_stats = sink.stats();
            (stop_result, final_stats)
        });
        self.final_stats = Some(final_stats);
        let _ = stop_result;
        Ok(())
    }
}

impl Drop for PyAsrSinkBackend {
    fn drop(&mut self) {
        let _ = Python::try_attach(|py| self.close_no_restore(py));
    }
}

#[pyclass(name = "_WavSinkBackend", module = "macloop._macloop", unsendable)]
struct PyWavSinkBackend {
    sink: Option<WavFileOutput>,
    final_stats: Option<WavSinkMetricsSnapshot>,
    route_ids: Vec<String>,
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

    #[pyo3(signature = (engine=None))]
    fn close(
        &mut self,
        py: Python<'_>,
        mut engine: Option<PyRefMut<'_, PyAudioEngineBackend>>,
    ) -> PyResult<()> {
        let Some(mut sink) = self.sink.take() else {
            return Ok(());
        };
        let route_ids = self.route_ids.clone();

        let (stop_result, final_stats) = py.detach(move || {
            let stop_result = sink.stop().map_err(|e| e.to_string());
            let final_stats = sink.stats();
            (stop_result, final_stats)
        });
        self.final_stats = Some(final_stats);
        let consumers = stop_result
            .map_err(|e| PyRuntimeError::new_err(format!("failed to stop wav sink: {e}")))?;

        if let Some(engine) = engine.as_mut() {
            engine
                .restore_route_consumers(route_ids.into_iter().zip(consumers).collect())
                .map_err(|e| PyRuntimeError::new_err(format!("failed to restore wav sink routes: {e}")))?;
        }
        Ok(())
    }

    fn close_no_restore(&mut self, py: Python<'_>) -> PyResult<()> {
        let Some(mut sink) = self.sink.take() else {
            return Ok(());
        };

        let (stop_result, final_stats) = py.detach(move || {
            let stop_result = sink.stop().map_err(|e| e.to_string());
            let final_stats = sink.stats();
            (stop_result, final_stats)
        });
        self.final_stats = Some(final_stats);
        let _ = stop_result;
        Ok(())
    }
}

impl Drop for PyWavSinkBackend {
    fn drop(&mut self) {
        let _ = Python::try_attach(|py| self.close_no_restore(py));
    }
}

#[pyclass(name = "_AudioEngineBackend", module = "macloop._macloop", unsendable)]
struct PyAudioEngineBackend {
    controller: AudioEngineController,
    sources: HashMap<String, StreamRuntimeState>,
    route_streams: HashMap<String, String>,
    // Timed-out native cleanup may outlive the first close attempt. We retain one best-effort
    // cleanup handle per stream so later close/drop paths can retry without reopening the backend.
    pending_cleanups: HashMap<String, PendingRuntimeCleanup>,
    closed: bool,
    poisoned: Option<BackendPoisonState>,
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
            pending_cleanups: HashMap::new(),
            closed: false,
            poisoned: None,
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

                let state = match py.detach(move || {
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
                }) {
                    Ok(state) => state,
                    Err(err) => {
                        return Err(self.rollback_failed_stream_creation(
                            &stream_id,
                            PyRuntimeError::new_err(err),
                        ))
                    }
                };

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

                let state = match py.detach(move || {
                    AppAudioSource::new(pipeline, AppAudioSourceConfig { pids, display_id })
                        .map(|source| StreamRuntimeState {
                            runtime: StreamRuntime::AppAudio(source),
                            started: false,
                        })
                        .map_err(|e| format!("failed to create application audio source: {e}"))
                }) {
                    Ok(state) => state,
                    Err(err) => {
                        return Err(self.rollback_failed_stream_creation(
                            &stream_id,
                            PyRuntimeError::new_err(err),
                        ))
                    }
                };

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

                let state = match py.detach(move || {
                    SystemAudioSource::new(pipeline, SystemAudioSourceConfig { display_id })
                        .map(|source| StreamRuntimeState {
                            runtime: StreamRuntime::SystemAudio(source),
                            started: false,
                        })
                        .map_err(|e| format!("failed to create system audio source: {e}"))
                }) {
                    Ok(state) => state,
                    Err(err) => {
                        return Err(self.rollback_failed_stream_creation(
                            &stream_id,
                            PyRuntimeError::new_err(err),
                        ))
                    }
                };

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

                let state = match py.detach(move || {
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
                }) {
                    Ok(state) => state,
                    Err(err) => {
                        return Err(self.rollback_failed_stream_creation(
                            &stream_id,
                            PyValueError::new_err(err),
                        ))
                    }
                };

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
        // `closed` means no new public operations are allowed. If native cleanup timed out earlier,
        // repeated close attempts are still allowed so drop/explicit close can keep retrying the
        // retained best-effort cleanup handles.
        if self.closed && self.pending_cleanups.is_empty() {
            return Ok(());
        }

        let sources = if self.closed {
            HashMap::new()
        } else {
            std::mem::take(&mut self.sources)
        };
        let pending_cleanups = std::mem::take(&mut self.pending_cleanups);
        let was_poisoned = self.poisoned.is_some();
        let (stop_result, remaining_cleanups) = py.detach(move || {
            let mut first_error: Option<String> = None;
            let deadline = Instant::now() + NATIVE_SOURCE_STOP_TIMEOUT;

            let (mut remaining_cleanups, cleanup_error) =
                cleanup_pending_cleanups_with_deadline(pending_cleanups, deadline);
            if let Some(err) = cleanup_error {
                first_error = Some(err);
            }

            let mut source_entries = sources.into_iter().collect::<Vec<_>>();
            source_entries.sort_by_key(|(_, source)| runtime_stop_priority(&source.runtime));

            for (stream_id, source) in source_entries {
                if !source.started {
                    continue;
                }
                if let Err(err) = stop_stream_runtime_with_deadline(source.runtime, deadline) {
                    match err {
                        StopStreamRuntimeError::Recoverable { runtime, message } => {
                            if let Some(cleanup) = pending_cleanup_from_ready_runtime(runtime) {
                                remaining_cleanups.insert(stream_id.clone(), cleanup);
                            }
                            if first_error.is_none() {
                                first_error = Some(format!("failed to stop source: {message}"));
                            }
                        }
                        StopStreamRuntimeError::Fatal {
                            runtime,
                            cleanup,
                            message,
                        } => {
                            if let Some(runtime) = runtime {
                                if let Some(cleanup) = pending_cleanup_from_ready_runtime(runtime) {
                                    remaining_cleanups.insert(stream_id.clone(), cleanup);
                                }
                            }
                            if let Some(cleanup) = cleanup {
                                remaining_cleanups.insert(stream_id.clone(), cleanup);
                            }
                            if first_error.is_none() {
                                first_error = Some(format!("failed to stop source: {message}"));
                            }
                        }
                    }
                }
            }

            let result = if let Some(err) = first_error {
                Err(err)
            } else {
                Ok(())
            };
            (result, remaining_cleanups)
        });
        self.pending_cleanups = remaining_cleanups;

        if !self.closed {
            self.route_streams.clear();
            self.controller.tick_gc();
            self.closed = true;
        }

        if was_poisoned {
            Ok(())
        } else {
            stop_result.map_err(|message| {
                if Self::lifecycle_timeout_error(&message) {
                    PyTimeoutError::new_err(message)
                } else {
                    PyRuntimeError::new_err(message)
                }
            })
        }
    }
}

impl PyAudioEngineBackend {
    fn lifecycle_timeout_error(message: &str) -> bool {
        message.contains("timed out") && message.contains("lifecycle deadline")
    }

    fn rollback_failed_stream_creation(&mut self, stream_id: &str, err: PyErr) -> PyErr {
        match self.controller.remove_stream(&stream_id.to_string()) {
            Ok(()) => err,
            Err(rollback_err) => PyRuntimeError::new_err(format!(
                "{}; additionally failed to roll back controller stream '{stream_id}': {rollback_err}",
                err
            )),
        }
    }

    fn ensure_open(&self) -> PyResult<()> {
        if self.closed {
            Err(PyRuntimeError::new_err("audio engine backend is closed"))
        } else if let Some(poison) = &self.poisoned {
            Err(PyRuntimeError::new_err(poison.runtime_error_message()))
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

    fn store_pending_cleanups(&mut self, cleanups: HashMap<String, PendingRuntimeCleanup>) {
        self.pending_cleanups.extend(cleanups);
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

    fn restore_route_consumers(&mut self, consumers: Vec<(String, RouteConsumer)>) -> PyResult<()> {
        for (route_id, consumer) in consumers {
            self.controller
                .restore_output_consumer(route_id, consumer)
                .map_err(engine_error)?;
        }
        Ok(())
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
            start_stream_states_with_timeout(stream_states, NATIVE_SOURCE_START_TIMEOUT)
        }) {
            Ok(states) => states,
            Err(StartStreamsError {
                message,
                states,
                poison,
                cleanup,
            }) => {
                self.restore_stream_states(states);
                self.store_pending_cleanups(cleanup);
                self.poisoned = poison;
                return Err(PyRuntimeError::new_err(message));
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
                if let Err(restore_err) = self.restore_route_consumers(route_consumers) {
                    return Err(PyRuntimeError::new_err(format!(
                        "{err}; additionally failed to restore route consumers: {restore_err}"
                    )));
                }
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
            start_stream_states_with_timeout(stream_states, NATIVE_SOURCE_START_TIMEOUT)
        }) {
            Ok(states) => states,
            Err(StartStreamsError {
                message,
                states,
                poison,
                cleanup,
            }) => {
                self.restore_stream_states(states);
                self.store_pending_cleanups(cleanup);
                self.poisoned = poison;
                return Err(PyRuntimeError::new_err(message));
            }
        };
        self.restore_stream_states(started_states);

        let route_consumers = self.take_route_consumers(&route_ids)?;
        let route_ids_for_sink = route_ids.clone();
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
                route_ids: route_ids_for_sink,
            }),
            Err((err, route_consumers)) => {
                if let Err(restore_err) = self.restore_route_consumers(route_consumers) {
                    return Err(PyRuntimeError::new_err(format!(
                        "{err}; additionally failed to restore route consumers: {restore_err}"
                    )));
                }
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
    for display in displays.map_err(|e| PyRuntimeError::new_err(e.to_string()))? {
        append_display_info(&list, display)?;
    }
    Ok(list)
}

#[pyfunction]
fn list_applications(py: Python<'_>) -> PyResult<Bound<'_, PyList>> {
    let applications = py.detach(AppAudioSource::list_applications);
    let list = PyList::empty(py);
    for application in applications.map_err(|e| PyRuntimeError::new_err(e.to_string()))? {
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

    #[derive(Debug)]
    struct DummyTimedSource {
        start_gate: Option<mpsc::Receiver<()>>,
        stop_gate: Option<mpsc::Receiver<()>>,
        started: bool,
    }

    fn dummy_start(source: &mut DummyTimedSource) -> Result<(), &'static str> {
        if let Some(rx) = source.start_gate.take() {
            let _ = rx.recv();
        }
        source.started = true;
        Ok(())
    }

    fn dummy_stop(source: &mut DummyTimedSource) -> Result<(), &'static str> {
        if let Some(rx) = source.stop_gate.take() {
            let _ = rx.recv();
        }
        source.started = false;
        Ok(())
    }

    fn unresolved_app_cleanup() -> PendingRuntimeCleanup {
        let (_tx, ready) = mpsc::sync_channel(1);
        PendingRuntimeCleanup::AppAudio(PendingCleanup {
            phase: PendingCleanupPhase::Starting,
            completion: Arc::new(Mutex::new(None)),
            ready,
        })
    }

    #[test]
    fn start_native_source_timeout_before_spawn_preserves_source() {
        let err = start_native_source_with_deadline(
            DummyTimedSource {
                start_gate: None,
                stop_gate: None,
                started: false,
            },
            "dummy source",
            Instant::now(),
            dummy_start,
        )
        .unwrap_err();

        match err {
            TimedSourceStartError::Fatal {
                source: Some(source),
                cleanup: None,
                ..
            } => assert!(!source.started),
            _ => panic!("unexpected start timeout result"),
        }
    }

    #[test]
    fn start_native_source_timeout_returns_pending_cleanup_handle() {
        let (start_tx, start_rx) = mpsc::channel();
        let err = start_native_source_with_deadline(
            DummyTimedSource {
                start_gate: Some(start_rx),
                stop_gate: None,
                started: false,
            },
            "dummy source",
            Instant::now() + Duration::from_millis(10),
            dummy_start,
        )
        .unwrap_err();

        let cleanup = match err {
            TimedSourceStartError::Fatal {
                source: None,
                cleanup: Some(cleanup),
                ..
            } => cleanup,
            _ => panic!("unexpected timed start result"),
        };

        start_tx.send(()).unwrap();
        let result = wait_for_deferred_source_result(
            &cleanup.completion,
            &cleanup.ready,
            Instant::now() + Duration::from_secs(1),
        )
        .expect("deferred start result");

        match result {
            Ok(source) => assert!(source.started),
            Err((_source, message)) => panic!("unexpected deferred start failure: {message}"),
        }
    }

    #[test]
    fn stop_native_source_timeout_returns_pending_cleanup_handle() {
        let (stop_tx, stop_rx) = mpsc::channel();
        let err = stop_native_source_with_deadline(
            DummyTimedSource {
                start_gate: None,
                stop_gate: Some(stop_rx),
                started: true,
            },
            "dummy source",
            Instant::now() + Duration::from_millis(10),
            dummy_stop,
        )
        .unwrap_err();

        let cleanup = match err {
            TimedSourceStopError::Fatal {
                source: None,
                cleanup: Some(cleanup),
                ..
            } => cleanup,
            _ => panic!("unexpected timed stop result"),
        };

        stop_tx.send(()).unwrap();
        let result = wait_for_deferred_source_result(
            &cleanup.completion,
            &cleanup.ready,
            Instant::now() + Duration::from_secs(1),
        )
        .expect("deferred stop result");

        match result {
            Ok(source) => assert!(!source.started),
            Err((_source, message)) => panic!("unexpected deferred stop failure: {message}"),
        }
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
    fn create_stream_failure_rolls_back_controller_state() {
        with_python(|py| {
            let mut backend = PyAudioEngineBackend::new();
            let config = PyDict::new(py);
            config.set_item("frames_per_callback", 0).unwrap();

            let err = backend
                .create_stream(
                    py,
                    "stream_bad".to_string(),
                    "synthetic".to_string(),
                    config,
                )
                .unwrap_err();
            assert!(err
                .to_string()
                .contains("failed to create synthetic source"));
            assert!(!backend.sources.contains_key("stream_bad"));
            assert!(!backend.controller.get_stats().contains_key("stream_bad"));

            let retry_config = PyDict::new(py);
            backend
                .create_stream(
                    py,
                    "stream_bad".to_string(),
                    "synthetic".to_string(),
                    retry_config,
                )
                .expect("retry create_stream after constructor failure should succeed");
            assert!(backend.sources.contains_key("stream_bad"));
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

    #[test]
    fn backend_close_retries_retained_pending_cleanup() {
        with_python(|py| {
            let mut backend = PyAudioEngineBackend::new();
            backend
                .pending_cleanups
                .insert("stream_1".to_string(), unresolved_app_cleanup());

            let err = backend.close(py).unwrap_err();
            assert!(err
                .to_string()
                .contains("timed out waiting for cleanup of stream 'stream_1'"));
            assert!(backend.closed);
            assert!(backend.pending_cleanups.contains_key("stream_1"));

            let err = backend.close(py).unwrap_err();
            assert!(err
                .to_string()
                .contains("timed out waiting for cleanup of stream 'stream_1'"));
            assert!(backend.pending_cleanups.contains_key("stream_1"));
        });
    }

    #[test]
    fn poisoned_backend_close_suppresses_cleanup_error_but_retains_cleanup() {
        with_python(|py| {
            let mut backend = PyAudioEngineBackend::new();
            backend.poisoned = Some(BackendPoisonState::new(
                "failed to start source for stream 'stream_1': timed out starting system audio source"
                    .to_string(),
                vec!["stream_1".to_string()],
            ));
            backend
                .pending_cleanups
                .insert("stream_1".to_string(), unresolved_app_cleanup());

            backend.close(py).unwrap();
            assert!(backend.closed);
            assert!(backend.pending_cleanups.contains_key("stream_1"));

            backend.close(py).unwrap();
            assert!(backend.pending_cleanups.contains_key("stream_1"));
        });
    }

    #[test]
    fn backend_poison_state_marks_engine_unusable() {
        let backend = PyAudioEngineBackend {
            controller: AudioEngineController::new(
                DEFAULT_COMMAND_CAPACITY,
                DEFAULT_GARBAGE_CAPACITY,
                DEFAULT_ROUTE_CAPACITY,
            ),
            sources: HashMap::new(),
            route_streams: HashMap::new(),
            pending_cleanups: HashMap::new(),
            closed: false,
            poisoned: Some(BackendPoisonState::new(
                "failed to start source for stream 'stream_1': timed out starting system audio source"
                    .to_string(),
                vec!["stream_1".to_string()],
            )),
        };

        let err = backend.ensure_open().unwrap_err();
        assert!(err
            .to_string()
            .contains("audio engine backend is unusable after a native startup timeout"));
        assert!(err.to_string().contains("stream_1"));
    }
}
