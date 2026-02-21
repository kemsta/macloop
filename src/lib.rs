#[cfg(all(not(test), feature = "capture"))]
use pyo3::prelude::*;
#[cfg(all(not(test), feature = "capture"))]
use pyo3::types::{PyAny, PyDict, PyList};
#[cfg(all(not(test), feature = "capture"))]
use std::thread::JoinHandle;
#[cfg(all(not(test), feature = "capture"))]
use crossbeam_channel::Sender;
#[cfg(all(not(test), feature = "capture"))]
use screencapturekit::prelude::*;

mod messages;
mod config;
#[cfg(all(not(test), feature = "capture"))]
mod capture;
mod processors;
mod modular_pipeline;
mod delay_measurement;
mod stats;

#[cfg(all(not(test), feature = "capture"))]
#[pyfunction]
fn list_audio_sources(py: Python<'_>) -> PyResult<Bound<'_, PyList>> {
    let content = SCShareableContent::get()
        .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(format!("Failed to get shareable content: {}", e)))?;
    
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
#[pyclass]
struct AudioEngine {
    target: Option<capture::CaptureTarget>,
    config: config::AudioProcessingConfig,
    stream: Option<screencapturekit::stream::sc_stream::SCStream>,
    thread: Option<JoinHandle<()>>,
    stop_tx: Option<Sender<()>>,
    stats: stats::RuntimeStatsHandle,
}

#[cfg(all(not(test), feature = "capture"))]
#[pymethods]
impl AudioEngine {
    #[new]
    #[pyo3(signature = (display_id=None, pid=None, config=None))]
    fn new(display_id: Option<u32>, pid: Option<i32>, config: Option<config::AudioProcessingConfig>) -> PyResult<Self> {
        let target = match (display_id, pid) {
            (Some(did), None) => Some(capture::CaptureTarget::Display(did)),
            (None, Some(p)) => Some(capture::CaptureTarget::Process(p)),
            (Some(_), Some(_)) => return Err(pyo3::exceptions::PyValueError::new_err("Provide EITHER display_id OR pid, not both")),
            (None, None) => None, // Mic only mode
        };

        let config = config.unwrap_or_else(|| {
            config::AudioProcessingConfig {
                sample_rate: 48000,
                channels: 2,
                enable_aec: false,
                enable_ns: false,
                sample_format: "f32".to_string(),
                aec_stream_delay_ms: 0,
                aec_auto_delay_tuning: false,
                aec_max_delay_ms: 140,
            }
        });

        Ok(Self {
            target,
            config,
            stream: None,
            thread: None,
            stop_tx: None,
            stats: stats::RuntimeStatsHandle::new(),
        })
    }

    #[pyo3(signature = (callback, capture_system=true, capture_mic=false))]
    fn start(&mut self, callback: Py<PyAny>, capture_system: bool, capture_mic: bool) -> PyResult<()> {
        // Ensure previous run is fully stopped before starting a new one.
        self.stop();
        self.stats.reset();

        // Use unbounded channel to ensure no packets are dropped, which is critical for AEC sync.
        let (tx, rx) = crossbeam_channel::unbounded();
        let (stop_tx, stop_rx) = crossbeam_channel::bounded(1);
        
        // 1. Start Capture (Producers)
        let stream = capture::spawn_capture_engine(tx, self.target, self.config.clone(), capture_system, capture_mic)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(format!("Failed to start capture: {}", e)))?;
            
        self.stream = Some(stream);

        // 2. Start Modular Pipeline (Consumer & Processor)
        let mut pipeline = modular_pipeline::ModularPipeline::new(
            rx,
            stop_rx,
            callback,
            self.config.clone(),
            self.stats.clone(),
        );
        let thread = std::thread::spawn(move || {
            pipeline.run();
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

    fn get_stats(&self) -> stats::PipelineStats {
        stats::PipelineStats::from_runtime(self.stats.snapshot())
    }
}

#[cfg(all(not(test), feature = "capture"))]
impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(all(not(test), feature = "capture"))]
#[pymodule]
fn _macloop(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<AudioEngine>()?;
    m.add_class::<config::AudioProcessingConfig>()?;
    m.add_class::<stats::PipelineStats>()?;
    m.add_function(wrap_pyfunction!(list_audio_sources, m)?)?;
    Ok(())
}
