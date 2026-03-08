pub type StreamId = String;
pub type OutputId = String;
pub type NodeId = String;

use crate::metrics::NodeMetrics;
use std::sync::Arc;

pub trait AudioProcessor: Send {
    fn id(&self) -> &str;
    fn set_metrics(&mut self, metrics: Arc<NodeMetrics>);
    fn process(&mut self, buffer: &mut [f32]);
}
