use crate::format::{StreamFormat, MASTER_FORMAT};
use crate::metrics::{EngineMetricsSnapshot, NodeMetrics, PipelineMetrics, StreamMetricsSnapshot};
use crate::processor::{AudioProcessor, NodeId, OutputId, StreamId};
use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError, TrySendError};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use std::collections::HashMap;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

pub type RouteProducer = HeapProd<f32>;
pub type RouteConsumer = HeapCons<f32>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceType {
    Microphone { device_id: Option<u32> },
    SystemAudio,
    ApplicationAudio,
    Synthetic,
}

pub enum PipelineCommand {
    AddProcessor(Box<dyn AudioProcessor>, Arc<NodeMetrics>),
    RemoveProcessor(NodeId),
    AddRoute(OutputId, RouteProducer),
    RemoveRoute(OutputId),
}

#[derive(Debug)]
pub enum EngineError {
    StreamNotFound(StreamId),
    StreamAlreadyExists(StreamId),
    ProcessorAlreadyExists { stream: StreamId, node: NodeId },
    CommandQueueFull(StreamId),
    CommandQueueDisconnected(StreamId),
    RouteAlreadyExists(OutputId),
    RouteNotFound(OutputId),
}

impl Display for EngineError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StreamNotFound(stream) => write!(f, "stream '{stream}' is not registered"),
            Self::StreamAlreadyExists(stream) => write!(f, "stream '{stream}' already exists"),
            Self::ProcessorAlreadyExists { stream, node } => {
                write!(f, "processor '{node}' already exists for stream '{stream}'")
            }
            Self::CommandQueueFull(stream) => {
                write!(f, "command queue for stream '{stream}' is full")
            }
            Self::CommandQueueDisconnected(stream) => {
                write!(f, "command queue for stream '{stream}' is disconnected")
            }
            Self::RouteAlreadyExists(output) => {
                write!(f, "route for output '{output}' already exists")
            }
            Self::RouteNotFound(output) => write!(f, "route for output '{output}' not found"),
        }
    }
}

impl Error for EngineError {}

struct ProcessorSlot {
    id: NodeId,
    processor: Box<dyn AudioProcessor>,
    metrics: Arc<NodeMetrics>,
}

pub struct RealTimePipeline {
    processors: Vec<ProcessorSlot>,
    outputs: HashMap<OutputId, RouteProducer>,
    command_rx: Receiver<PipelineCommand>,
    garbage_tx: Sender<Box<dyn AudioProcessor>>,
    metrics: Arc<PipelineMetrics>,
    format: StreamFormat,
}

impl RealTimePipeline {
    pub fn new(
        command_rx: Receiver<PipelineCommand>,
        garbage_tx: Sender<Box<dyn AudioProcessor>>,
        metrics: Arc<PipelineMetrics>,
        format: StreamFormat,
        max_processors: usize,
        max_outputs: usize,
    ) -> Self {
        Self {
            processors: Vec::with_capacity(max_processors),
            outputs: HashMap::with_capacity(max_outputs),
            command_rx,
            garbage_tx,
            metrics,
            format,
        }
    }

    pub fn process_callback(&mut self, buffer: &mut [f32]) {
        let start = Instant::now();

        self.apply_control_commands();

        for slot in &mut self.processors {
            let node_start = Instant::now();
            slot.processor.process(buffer);
            let elapsed_us = micros_u32(node_start.elapsed().as_micros());

            slot.metrics
                .processing_time_us
                .store(elapsed_us, Ordering::Relaxed);
            slot.metrics.latency.record(elapsed_us);
        }

        for output in self.outputs.values_mut() {
            let pushed = output.push_slice(buffer);
            if pushed < buffer.len() {
                self.metrics.dropped_frames.fetch_add(
                    (buffer.len() - pushed).min(u32::MAX as usize) as u32,
                    Ordering::Relaxed,
                );
            }
        }

        self.metrics.buffer_size.store(
            buffer.len().min(u32::MAX as usize) as u32,
            Ordering::Relaxed,
        );
        let elapsed_us = micros_u32(start.elapsed().as_micros());
        self.metrics
            .total_callback_time_us
            .store(elapsed_us, Ordering::Relaxed);
        self.metrics.latency.record(elapsed_us);
    }

    fn apply_control_commands(&mut self) {
        loop {
            match self.command_rx.try_recv() {
                Ok(PipelineCommand::AddProcessor(processor, node_metrics)) => {
                    self.processors.push(ProcessorSlot {
                        id: processor.id().to_owned(),
                        processor,
                        metrics: node_metrics,
                    });
                }
                Ok(PipelineCommand::RemoveProcessor(node_id)) => {
                    if let Some(index) = self.processors.iter().position(|slot| slot.id == node_id)
                    {
                        let removed = self.processors.swap_remove(index);
                        let _ = self.garbage_tx.try_send(removed.processor);
                    }
                }
                Ok(PipelineCommand::AddRoute(output_id, producer)) => {
                    self.outputs.insert(output_id, producer);
                }
                Ok(PipelineCommand::RemoveRoute(output_id)) => {
                    self.outputs.remove(&output_id);
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    pub fn format(&self) -> StreamFormat {
        self.format
    }
}

fn micros_u32(v: u128) -> u32 {
    if v > u32::MAX as u128 {
        u32::MAX
    } else {
        v as u32
    }
}

pub struct AudioEngineController {
    stream_routers: HashMap<StreamId, Sender<PipelineCommand>>,
    garbage_rx: Receiver<Box<dyn AudioProcessor>>,
    garbage_tx: Sender<Box<dyn AudioProcessor>>,
    pipeline_stats: HashMap<StreamId, Arc<PipelineMetrics>>,
    processor_stats: HashMap<StreamId, HashMap<NodeId, Arc<NodeMetrics>>>,
    stream_sources: HashMap<StreamId, SourceType>,
    output_consumers: HashMap<OutputId, RouteConsumer>,
    default_command_capacity: usize,
    default_route_capacity: usize,
}

impl AudioEngineController {
    pub fn new(
        default_command_capacity: usize,
        garbage_capacity: usize,
        default_route_capacity: usize,
    ) -> Self {
        let (garbage_tx, garbage_rx) = bounded(garbage_capacity);

        Self {
            stream_routers: HashMap::new(),
            garbage_rx,
            garbage_tx,
            pipeline_stats: HashMap::new(),
            processor_stats: HashMap::new(),
            stream_sources: HashMap::new(),
            output_consumers: HashMap::new(),
            default_command_capacity: default_command_capacity.max(1),
            default_route_capacity: default_route_capacity.max(2),
        }
    }

    pub fn create_stream(
        &mut self,
        id: StreamId,
        source: SourceType,
        max_processors: usize,
        max_outputs: usize,
    ) -> Result<RealTimePipeline, EngineError> {
        if self.stream_routers.contains_key(&id) {
            return Err(EngineError::StreamAlreadyExists(id));
        }

        let (command_tx, command_rx) = bounded(self.default_command_capacity);
        let metrics = Arc::new(PipelineMetrics::default());

        self.stream_routers.insert(id.clone(), command_tx);
        self.pipeline_stats.insert(id.clone(), metrics.clone());
        self.processor_stats.insert(id.clone(), HashMap::new());
        self.stream_sources.insert(id, source);

        Ok(RealTimePipeline::new(
            command_rx,
            self.garbage_tx.clone(),
            metrics,
            MASTER_FORMAT,
            max_processors,
            max_outputs,
        ))
    }

    pub fn remove_stream(&mut self, stream: &StreamId) -> Result<(), EngineError> {
        if !self.stream_routers.contains_key(stream) {
            return Err(EngineError::StreamNotFound(stream.clone()));
        }

        self.stream_routers.remove(stream);
        self.pipeline_stats.remove(stream);
        self.processor_stats.remove(stream);
        self.stream_sources.remove(stream);
        Ok(())
    }

    pub fn route(&mut self, stream: &StreamId, output: &OutputId) -> Result<(), EngineError> {
        if self.output_consumers.contains_key(output) {
            return Err(EngineError::RouteAlreadyExists(output.clone()));
        }

        let router = self
            .stream_routers
            .get(stream)
            .ok_or_else(|| EngineError::StreamNotFound(stream.clone()))?;

        let ring = HeapRb::<f32>::new(self.default_route_capacity);
        let (producer, consumer) = ring.split();

        match router.try_send(PipelineCommand::AddRoute(output.clone(), producer)) {
            Ok(()) => {
                self.output_consumers.insert(output.clone(), consumer);
                Ok(())
            }
            Err(TrySendError::Full(_)) => Err(EngineError::CommandQueueFull(stream.clone())),
            Err(TrySendError::Disconnected(_)) => {
                Err(EngineError::CommandQueueDisconnected(stream.clone()))
            }
        }
    }

    pub fn unroute(&mut self, stream: &StreamId, output: &OutputId) -> Result<(), EngineError> {
        let router = self
            .stream_routers
            .get(stream)
            .ok_or_else(|| EngineError::StreamNotFound(stream.clone()))?;

        match router.try_send(PipelineCommand::RemoveRoute(output.clone())) {
            Ok(()) => {
                self.output_consumers.remove(output);
                Ok(())
            }
            Err(TrySendError::Full(_)) => Err(EngineError::CommandQueueFull(stream.clone())),
            Err(TrySendError::Disconnected(_)) => {
                Err(EngineError::CommandQueueDisconnected(stream.clone()))
            }
        }
    }

    pub fn add_processor(
        &mut self,
        stream: &StreamId,
        mut processor: Box<dyn AudioProcessor>,
    ) -> Result<(), EngineError> {
        let router = self
            .stream_routers
            .get(stream)
            .ok_or_else(|| EngineError::StreamNotFound(stream.clone()))?;

        let node_id = processor.id().to_owned();
        if self
            .processor_stats
            .get(stream)
            .and_then(|nodes| nodes.get(&node_id))
            .is_some()
        {
            return Err(EngineError::ProcessorAlreadyExists {
                stream: stream.clone(),
                node: node_id,
            });
        }

        let metrics = Arc::new(NodeMetrics::default());
        processor.set_metrics(metrics.clone());

        match router.try_send(PipelineCommand::AddProcessor(processor, metrics.clone())) {
            Ok(()) => {
                if let Some(nodes) = self.processor_stats.get_mut(stream) {
                    nodes.insert(node_id, metrics);
                }
                Ok(())
            }
            Err(TrySendError::Full(_)) => Err(EngineError::CommandQueueFull(stream.clone())),
            Err(TrySendError::Disconnected(_)) => {
                Err(EngineError::CommandQueueDisconnected(stream.clone()))
            }
        }
    }

    pub fn remove_processor(
        &mut self,
        stream: &StreamId,
        node_id: &str,
    ) -> Result<(), EngineError> {
        let router = self
            .stream_routers
            .get(stream)
            .ok_or_else(|| EngineError::StreamNotFound(stream.clone()))?;

        match router.try_send(PipelineCommand::RemoveProcessor(node_id.to_owned())) {
            Ok(()) => {
                if let Some(nodes) = self.processor_stats.get_mut(stream) {
                    nodes.remove(node_id);
                }
                Ok(())
            }
            Err(TrySendError::Full(_)) => Err(EngineError::CommandQueueFull(stream.clone())),
            Err(TrySendError::Disconnected(_)) => {
                Err(EngineError::CommandQueueDisconnected(stream.clone()))
            }
        }
    }

    pub fn take_output_consumer(&mut self, output: &OutputId) -> Option<RouteConsumer> {
        self.output_consumers.remove(output)
    }

    pub fn has_output_consumer(&self, output: &OutputId) -> bool {
        self.output_consumers.contains_key(output)
    }

    pub fn restore_output_consumer(
        &mut self,
        output: OutputId,
        consumer: RouteConsumer,
    ) -> Result<(), EngineError> {
        if self.output_consumers.contains_key(&output) {
            return Err(EngineError::RouteAlreadyExists(output));
        }

        self.output_consumers.insert(output, consumer);
        Ok(())
    }

    pub fn tick_gc(&self) {
        while let Ok(garbage) = self.garbage_rx.try_recv() {
            drop(garbage);
        }
    }

    pub fn get_stats(&self) -> EngineMetricsSnapshot {
        self.pipeline_stats
            .iter()
            .map(|(stream_id, metrics)| {
                let processors = self
                    .processor_stats
                    .get(stream_id)
                    .map(|nodes| {
                        nodes
                            .iter()
                            .map(|(node_id, metrics)| (node_id.clone(), metrics.snapshot()))
                            .collect()
                    })
                    .unwrap_or_default();

                (
                    stream_id.clone(),
                    StreamMetricsSnapshot {
                        pipeline: metrics.snapshot(),
                        processors,
                    },
                )
            })
            .collect()
    }

    pub fn master_format(&self) -> StreamFormat {
        MASTER_FORMAT
    }
}

pub fn run_mock_callback_demo() -> Result<usize, EngineError> {
    let mut engine = AudioEngineController::new(32, 32, 1024);

    let stream_id = "mic".to_string();
    let output_id = "analyzer".to_string();

    let mut pipeline = engine.create_stream(
        stream_id.clone(),
        SourceType::Microphone { device_id: None },
        16,
        8,
    )?;
    engine.route(&stream_id, &output_id)?;

    let mut frame = [0.25_f32; 256];
    for _ in 0..4 {
        pipeline.process_callback(&mut frame);
    }

    engine.tick_gc();

    let mut consumer = engine
        .take_output_consumer(&output_id)
        .ok_or_else(|| EngineError::RouteNotFound(output_id.clone()))?;

    let mut drained = 0_usize;
    while consumer.try_pop().is_some() {
        drained += 1;
    }

    Ok(drained)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    struct NoopProcessor {
        id: String,
        _metrics: Option<Arc<NodeMetrics>>,
    }

    impl AudioProcessor for NoopProcessor {
        fn id(&self) -> &str {
            &self.id
        }

        fn set_metrics(&mut self, metrics: Arc<NodeMetrics>) {
            self._metrics = Some(metrics);
        }

        fn process(&mut self, _buffer: &mut [f32]) {}
    }

    struct MultiplyProcessor {
        id: String,
        _metrics: Option<Arc<NodeMetrics>>,
        factor: f32,
    }

    impl AudioProcessor for MultiplyProcessor {
        fn id(&self) -> &str {
            &self.id
        }

        fn set_metrics(&mut self, metrics: Arc<NodeMetrics>) {
            self._metrics = Some(metrics);
        }

        fn process(&mut self, buffer: &mut [f32]) {
            for sample in buffer {
                *sample *= self.factor;
            }
        }
    }

    struct DropCounterProcessor {
        id: String,
        _metrics: Option<Arc<NodeMetrics>>,
        drops: Arc<AtomicUsize>,
    }

    impl AudioProcessor for DropCounterProcessor {
        fn id(&self) -> &str {
            &self.id
        }

        fn set_metrics(&mut self, metrics: Arc<NodeMetrics>) {
            self._metrics = Some(metrics);
        }

        fn process(&mut self, _buffer: &mut [f32]) {}
    }

    impl Drop for DropCounterProcessor {
        fn drop(&mut self) {
            self.drops.fetch_add(1, AtomicOrdering::Relaxed);
        }
    }

    #[test]
    fn callback_processes_and_fans_out() {
        let mut engine = AudioEngineController::new(32, 32, 64);
        let stream = "capture".to_string();
        let output = "worker".to_string();

        let mut pipeline = engine
            .create_stream(stream.clone(), SourceType::SystemAudio, 8, 4)
            .expect("create stream");
        engine
            .add_processor(
                &stream,
                Box::new(NoopProcessor {
                    id: "noop-1".to_string(),
                    _metrics: None,
                }),
            )
            .expect("add processor");
        engine.route(&stream, &output).expect("route");

        let mut frame = [1.0_f32; 16];
        pipeline.process_callback(&mut frame);

        let mut out = engine
            .take_output_consumer(&output)
            .expect("consumer present");
        let mut count = 0;
        while out.try_pop().is_some() {
            count += 1;
        }

        assert!(count > 0);
    }

    #[test]
    fn remove_processor_stops_mutating_buffer() {
        let mut engine = AudioEngineController::new(32, 32, 64);
        let stream = "capture".to_string();
        let mut pipeline = engine
            .create_stream(stream.clone(), SourceType::SystemAudio, 8, 4)
            .expect("create stream");

        engine
            .add_processor(
                &stream,
                Box::new(MultiplyProcessor {
                    id: "mul".to_string(),
                    _metrics: None,
                    factor: 2.0,
                }),
            )
            .expect("add processor");

        let mut frame = [1.0_f32; 8];
        pipeline.process_callback(&mut frame);
        assert_eq!(frame[0], 2.0);

        engine.remove_processor(&stream, "mul").expect("remove");
        let mut frame2 = [1.0_f32; 8];
        pipeline.process_callback(&mut frame2);
        assert_eq!(frame2[0], 1.0);

        engine.tick_gc();
    }

    #[test]
    fn stats_are_updated_from_callback() {
        let mut engine = AudioEngineController::new(32, 32, 64);
        let stream = "capture".to_string();
        let output = "worker".to_string();
        let mut pipeline = engine
            .create_stream(stream.clone(), SourceType::SystemAudio, 8, 4)
            .expect("create stream");

        engine
            .add_processor(
                &stream,
                Box::new(NoopProcessor {
                    id: "noop-1".to_string(),
                    _metrics: None,
                }),
            )
            .expect("add processor");
        engine.route(&stream, &output).expect("route");

        let mut frame = [0.5_f32; 32];
        pipeline.process_callback(&mut frame);

        let stats = engine.get_stats();
        let snapshot = stats.get(&stream).expect("stream stats");
        assert_eq!(snapshot.pipeline.buffer_size, 32);
        assert!(snapshot.pipeline.total_callback_time_us <= u32::MAX);
        assert_eq!(snapshot.pipeline.latency.count, 1);
        assert!(snapshot.pipeline.latency.p99_us >= snapshot.pipeline.latency.p50_us);
        assert_eq!(snapshot.processors.len(), 1);
        let processor = snapshot.processors.get("noop-1").expect("processor stats");
        assert_eq!(processor.latency.count, 1);
        assert_eq!(pipeline.format(), crate::format::MASTER_FORMAT);
    }

    #[test]
    fn create_stream_rejects_duplicate_id() {
        let mut engine = AudioEngineController::new(4, 8, 16);
        let stream = "capture".to_string();

        let _ = engine
            .create_stream(stream.clone(), SourceType::SystemAudio, 4, 4)
            .expect("first create");
        match engine.create_stream(stream.clone(), SourceType::SystemAudio, 4, 4) {
            Err(EngineError::StreamAlreadyExists(_)) => {}
            _ => panic!("duplicate must fail with StreamAlreadyExists"),
        }
    }

    #[test]
    fn route_rejects_duplicate_output_id() {
        let mut engine = AudioEngineController::new(8, 8, 16);
        let stream = "capture".to_string();
        let output = "mix".to_string();

        let _pipeline = engine
            .create_stream(stream.clone(), SourceType::SystemAudio, 4, 4)
            .expect("create stream");
        engine.route(&stream, &output).expect("first route");
        let err = engine
            .route(&stream, &output)
            .expect_err("duplicate route must fail");

        assert!(matches!(err, EngineError::RouteAlreadyExists(_)));
    }

    #[test]
    fn backpressure_increments_dropped_frames_metric() {
        let mut engine = AudioEngineController::new(8, 8, 4);
        let stream = "capture".to_string();
        let output = "tiny".to_string();

        let mut pipeline = engine
            .create_stream(stream.clone(), SourceType::SystemAudio, 4, 4)
            .expect("create stream");
        engine.route(&stream, &output).expect("route");

        let mut frame = [1.0_f32; 64];
        pipeline.process_callback(&mut frame);

        let stats = engine.get_stats();
        let snapshot = stats.get(&stream).expect("stats");
        assert!(snapshot.pipeline.dropped_frames > 0);
    }

    #[test]
    fn removed_processor_is_dropped_in_tick_gc() {
        let mut engine = AudioEngineController::new(16, 16, 16);
        let stream = "capture".to_string();
        let drops = Arc::new(AtomicUsize::new(0));
        let mut pipeline = engine
            .create_stream(stream.clone(), SourceType::SystemAudio, 8, 4)
            .expect("create stream");

        engine
            .add_processor(
                &stream,
                Box::new(DropCounterProcessor {
                    id: "dropper".to_string(),
                    _metrics: None,
                    drops: drops.clone(),
                }),
            )
            .expect("add");

        let mut frame = [0.0_f32; 8];
        pipeline.process_callback(&mut frame);
        engine.remove_processor(&stream, "dropper").expect("remove");
        pipeline.process_callback(&mut frame);

        assert_eq!(drops.load(AtomicOrdering::Relaxed), 0);
        engine.tick_gc();
        assert_eq!(drops.load(AtomicOrdering::Relaxed), 1);
    }
}
