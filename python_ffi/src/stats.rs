use core_engine::{
    AsrInputMetricsSnapshot, LatencyHistogramSnapshot, NodeMetricsSnapshot,
    PipelineMetricsSnapshot, StreamMetricsSnapshot, WavSinkMetricsSnapshot,
};
use pyo3::prelude::*;
use pyo3::types::PyDict;

#[pyclass(name = "LatencyStats", module = "macloop._macloop")]
pub struct PyLatencyStats {
    #[pyo3(get)]
    pub last_us: u32,
    #[pyo3(get)]
    pub max_us: u32,
    #[pyo3(get)]
    pub count: u64,
    #[pyo3(get)]
    pub bucket_bounds_us: Vec<u32>,
    #[pyo3(get)]
    pub buckets: Vec<u64>,
    #[pyo3(get)]
    pub p50_us: u32,
    #[pyo3(get)]
    pub p90_us: u32,
    #[pyo3(get)]
    pub p95_us: u32,
    #[pyo3(get)]
    pub p99_us: u32,
}

impl From<LatencyHistogramSnapshot> for PyLatencyStats {
    fn from(value: LatencyHistogramSnapshot) -> Self {
        Self {
            last_us: value.last_us,
            max_us: value.max_us,
            count: value.count,
            bucket_bounds_us: value.bucket_bounds_us,
            buckets: value.buckets,
            p50_us: value.p50_us,
            p90_us: value.p90_us,
            p95_us: value.p95_us,
            p99_us: value.p99_us,
        }
    }
}

#[pyclass(name = "PipelineStats", module = "macloop._macloop")]
pub struct PyPipelineStats {
    #[pyo3(get)]
    pub total_callback_time_us: u32,
    #[pyo3(get)]
    pub dropped_frames: u32,
    #[pyo3(get)]
    pub buffer_size: u32,
    #[pyo3(get)]
    pub latency: Py<PyLatencyStats>,
}

impl PyPipelineStats {
    pub fn from_snapshot(py: Python<'_>, value: PipelineMetricsSnapshot) -> PyResult<Self> {
        Ok(Self {
            total_callback_time_us: value.total_callback_time_us,
            dropped_frames: value.dropped_frames,
            buffer_size: value.buffer_size,
            latency: Py::new(py, PyLatencyStats::from(value.latency))?,
        })
    }
}

#[pyclass(name = "ProcessorStats", module = "macloop._macloop")]
pub struct PyProcessorStats {
    #[pyo3(get)]
    pub processing_time_us: u32,
    #[pyo3(get)]
    pub max_processing_time_us: u32,
    #[pyo3(get)]
    pub latency: Py<PyLatencyStats>,
}

impl PyProcessorStats {
    pub fn from_snapshot(py: Python<'_>, value: NodeMetricsSnapshot) -> PyResult<Self> {
        Ok(Self {
            processing_time_us: value.processing_time_us,
            max_processing_time_us: value.max_processing_time_us,
            latency: Py::new(py, PyLatencyStats::from(value.latency))?,
        })
    }
}

#[pyclass(name = "StreamStats", module = "macloop._macloop")]
pub struct PyStreamStats {
    #[pyo3(get)]
    pub pipeline: Py<PyPipelineStats>,
    #[pyo3(get)]
    pub processors: Py<PyDict>,
}

impl PyStreamStats {
    pub fn from_snapshot(py: Python<'_>, value: StreamMetricsSnapshot) -> PyResult<Self> {
        let processors = PyDict::new(py);
        for (node_id, stats) in value.processors {
            processors.set_item(
                node_id,
                Py::new(py, PyProcessorStats::from_snapshot(py, stats)?)?,
            )?;
        }

        Ok(Self {
            pipeline: Py::new(py, PyPipelineStats::from_snapshot(py, value.pipeline)?)?,
            processors: processors.unbind(),
        })
    }
}

#[pyclass(name = "AsrInputStats", module = "macloop._macloop")]
pub struct PyAsrInputStats {
    #[pyo3(get)]
    pub chunks_emitted: u64,
    #[pyo3(get)]
    pub frames_emitted: u64,
    #[pyo3(get)]
    pub pending_frames: u32,
    #[pyo3(get)]
    pub poll: Py<PyLatencyStats>,
    #[pyo3(get)]
    pub callback: Py<PyLatencyStats>,
}

impl PyAsrInputStats {
    pub fn from_snapshot(py: Python<'_>, value: AsrInputMetricsSnapshot) -> PyResult<Self> {
        Ok(Self {
            chunks_emitted: value.chunks_emitted,
            frames_emitted: value.frames_emitted,
            pending_frames: value.pending_frames,
            poll: Py::new(py, PyLatencyStats::from(value.poll))?,
            callback: Py::new(py, PyLatencyStats::from(value.callback))?,
        })
    }
}

#[pyclass(name = "WavSinkStats", module = "macloop._macloop")]
pub struct PyWavSinkStats {
    #[pyo3(get)]
    pub write_calls: u64,
    #[pyo3(get)]
    pub samples_written: u64,
    #[pyo3(get)]
    pub frames_written: u64,
    #[pyo3(get)]
    pub write: Py<PyLatencyStats>,
    #[pyo3(get)]
    pub finalize: Py<PyLatencyStats>,
}

impl PyWavSinkStats {
    pub fn from_snapshot(py: Python<'_>, value: WavSinkMetricsSnapshot) -> PyResult<Self> {
        Ok(Self {
            write_calls: value.write_calls,
            samples_written: value.samples_written,
            frames_written: value.frames_written,
            write: Py::new(py, PyLatencyStats::from(value.write))?,
            finalize: Py::new(py, PyLatencyStats::from(value.finalize))?,
        })
    }
}
