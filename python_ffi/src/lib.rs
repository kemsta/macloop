#[cfg(all(not(test), feature = "capture"))]
mod capture;
mod config;
mod stats;

#[cfg(all(not(test), feature = "capture"))]
use core_engine::messages::AudioSourceType;
#[cfg(all(not(test), feature = "capture"))]
use core_engine::modular_pipeline::ModularPipeline;
#[cfg(all(not(test), feature = "capture"))]
use core_engine::stats::RuntimeStatsHandle;
#[cfg(all(not(test), feature = "capture"))]
use crossbeam_channel::Sender;
#[cfg(all(not(test), feature = "capture"))]
use numpy::ToPyArray;
#[cfg(all(not(test), feature = "capture"))]
use pyo3::prelude::*;
#[cfg(all(not(test), feature = "capture"))]
use pyo3::types::{PyAny, PyDict, PyList};
#[cfg(all(not(test), feature = "capture"))]
use std::sync::{Arc, Mutex};
#[cfg(all(not(test), feature = "capture"))]
use std::thread::JoinHandle;

pub use config::PyAudioProcessingConfig;
pub use stats::PyPipelineStats;

#[cfg(all(not(test), feature = "capture"))]
#[pyfunction]
fn list_audio_sources(py: Python<'_>) -> PyResult<Bound<'_, PyList>> {
    use screencapturekit::prelude::*;

    let content = SCShareableContent::get().map_err(|e| {
        pyo3::exceptions::PyRuntimeError::new_err(format!(
            "Failed to get shareable content: {}",
            e
        ))
    })?;

    let list = PyList::empty(py);

    for display in content.displays() {
        let dict = PyDict::new(py);
        dict.set_item("type", "display")?;
        dict.set_item("name", format!("Display {}", display.display_id()))?;
        dict.set_item("display_id", display.display_id())?;
        list.append(dict)?;
    }

    for app in content.applications() {
        let dict = PyDict::new(py);
        dict.set_item("type", "app")?;
        dict.set_item("name", app.application_name())?;
        dict.set_item("pid", app.process_id())?;
        dict.set_item("bundle_id", app.bundle_identifier())?;
        list.append(dict)?;
    }

    Ok(list)
}

#[cfg(all(not(test), feature = "capture"))]
#[pyclass(name = "AudioEngine", module = "macloop._macloop")]
struct AudioEngine {
    target: Option<capture::CaptureTarget>,
    config: PyAudioProcessingConfig,
    stream: Option<screencapturekit::stream::sc_stream::SCStream>,
    thread: Option<JoinHandle<()>>,
    stop_tx: Option<Sender<()>>,
    stats: RuntimeStatsHandle,
    gil_acquire_failures: Arc<Mutex<u64>>,
}

#[cfg(all(not(test), feature = "capture"))]
#[pymethods]
impl AudioEngine {
    #[new]
    #[pyo3(signature = (display_id=None, pid=None, config=None))]
    fn new(
        display_id: Option<u32>,
        pid: Option<i32>,
        config: Option<PyAudioProcessingConfig>,
    ) -> PyResult<Self> {
        let target = match (display_id, pid) {
            (Some(did), None) => Some(capture::CaptureTarget::Display(did)),
            (None, Some(p)) => Some(capture::CaptureTarget::Process(p)),
            (Some(_), Some(_)) => {
                return Err(pyo3::exceptions::PyValueError::new_err(
                    "Provide EITHER display_id OR pid, not both",
                ));
            }
            (None, None) => None,
        };

        Ok(Self {
            target,
            config: config.unwrap_or_default(),
            stream: None,
            thread: None,
            stop_tx: None,
            stats: RuntimeStatsHandle::new(),
            gil_acquire_failures: Arc::new(Mutex::new(0)),
        })
    }

    #[pyo3(signature = (callback, capture_system=true, capture_mic=false))]
    fn start(&mut self, callback: Py<PyAny>, capture_system: bool, capture_mic: bool) -> PyResult<()> {
        self.stop();
        self.stats.reset();
        if let Ok(mut failures) = self.gil_acquire_failures.lock() {
            *failures = 0;
        }

        let (tx, rx) = crossbeam_channel::unbounded();
        let (stop_tx, stop_rx) = crossbeam_channel::bounded(1);

        let core_cfg = core_engine::config::AudioProcessingConfig::from(&self.config);

        let stream = capture::spawn_capture_engine(
            tx,
            self.target,
            core_cfg.clone(),
            capture_system,
            capture_mic,
        )
        .map_err(|e| {
            pyo3::exceptions::PyRuntimeError::new_err(format!("Failed to start capture: {}", e))
        })?;

        self.stream = Some(stream);

        let callback_owned = callback;
        let sample_format = self.config.sample_format.clone();
        let stats = self.stats.clone();
        let gil_failures = Arc::clone(&self.gil_acquire_failures);

        let thread = std::thread::spawn(move || {
            let mut pipeline = ModularPipeline::new(rx, stop_rx, core_cfg, stats.clone());
            pipeline.run_with_handler(|frame| {
                let source_name = match frame.source {
                    AudioSourceType::Microphone => "mic",
                    AudioSourceType::System => "system",
                };

                match Python::try_attach(|py| {
                    let frame_np = to_numpy(py, &frame.samples, &sample_format);
                    callback_owned.call1(py, (source_name, frame_np))
                }) {
                    None => {
                        if let Ok(mut failures) = gil_failures.lock() {
                            *failures += 1;
                        }
                        eprintln!("Warning: Could not acquire Python GIL");
                    }
                    Some(Err(err)) => {
                        stats.update(|s| s.callback_errors += 1);
                        eprintln!("Warning: Python callback error: {}", err);
                    }
                    Some(Ok(_)) => {}
                }
            });
        });

        self.thread = Some(thread);
        self.stop_tx = Some(stop_tx);
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }

        if let Some(stream) = self.stream.take() {
            let _ = stream.stop_capture();
        }

        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    fn get_stats(&self) -> PyPipelineStats {
        let gil_failures = self.gil_acquire_failures.lock().map(|v| *v).unwrap_or_default();
        PyPipelineStats::from_runtime(self.stats.snapshot(), gil_failures)
    }
}

#[cfg(all(not(test), feature = "capture"))]
impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(all(not(test), feature = "capture"))]
fn to_numpy<'py>(py: Python<'py>, samples: &[f32], format: &str) -> Py<PyAny> {
    if format == "i16" {
        let i16_samples = samples
            .iter()
            .map(|&s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
        numpy::PyArray1::from_iter(py, i16_samples)
            .into_any()
            .unbind()
    } else {
        samples.to_pyarray(py).into_any().unbind()
    }
}

#[cfg(all(not(test), feature = "capture"))]
#[pymodule]
fn _macloop(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<AudioEngine>()?;
    m.add_class::<PyAudioProcessingConfig>()?;
    m.add_class::<PyPipelineStats>()?;
    m.add_function(wrap_pyfunction!(list_audio_sources, m)?)?;
    Ok(())
}
