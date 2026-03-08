use crate::metrics::NodeMetrics;
use std::sync::Arc;

pub trait AudioAnalyzer: Send {
    fn id(&self) -> &str;
    fn set_metrics(&mut self, metrics: Arc<NodeMetrics>);
    fn analyze(&mut self, capture: &[f32], reference: Option<&[f32]>);
}
