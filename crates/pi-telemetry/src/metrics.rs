//! Optional metric instruments (upstream `metrics.ts`) and schema-checked metric handles.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::attributes::SpanAttributes;
use crate::schema::{SchemaViolation, TelemetryAttributeDefinition, TelemetrySchemaDefinition, check_attributes};
use crate::span::TelemetryContext;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct InstrumentOptions {
    pub description: Option<String>,
    pub unit: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HistogramOptions {
    pub description: Option<String>,
    pub unit: Option<String>,
    pub boundaries: Option<Vec<f64>>,
}

pub trait TelemetryCounter: Send + Sync {
    fn add(&self, value: f64, attributes: SpanAttributes);
}

pub trait TelemetryUpDownCounter: Send + Sync {
    fn add(&self, value: f64, attributes: SpanAttributes);
}

pub trait TelemetryHistogram: Send + Sync {
    fn record(&self, value: f64, attributes: SpanAttributes, context: Option<&dyn TelemetryContext>);
}

/// Optional adapter capability. Factories and recording methods are synchronous and must not fail.
pub trait TelemetryMetrics: Send + Sync {
    fn counter(&self, name: &str, options: InstrumentOptions) -> Arc<dyn TelemetryCounter>;
    fn up_down_counter(&self, name: &str, options: InstrumentOptions) -> Arc<dyn TelemetryUpDownCounter>;
    fn histogram(&self, name: &str, options: HistogramOptions) -> Arc<dyn TelemetryHistogram>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TelemetryMetricKind {
    #[serde(rename = "counter")]
    Counter,
    #[serde(rename = "upDownCounter")]
    UpDownCounter,
    #[serde(rename = "histogram")]
    Histogram,
}

/// One schema metric. Metric attributes must be low-cardinality.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TelemetryMetricDefinition {
    pub kind: TelemetryMetricKind,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    pub attributes: BTreeMap<String, TelemetryAttributeDefinition>,
    /// Histograms only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundaries: Option<Vec<f64>>,
}

type Record = Arc<dyn Fn(f64, SpanAttributes, Option<&dyn TelemetryContext>) + Send + Sync>;

/// A schema counter or up-down counter; measurements are checked against its attributes.
#[derive(Clone)]
pub struct SchemaTelemetryCounter {
    record: Record,
    definition: Arc<TelemetryMetricDefinition>,
}

impl SchemaTelemetryCounter {
    pub fn add(&self, value: f64, attributes: SpanAttributes) -> Result<(), SchemaViolation> {
        check_attributes(&self.definition.attributes, &attributes, true)?;
        (self.record)(value, attributes, None);
        Ok(())
    }
}

/// A schema histogram; measurements are checked against its attributes.
#[derive(Clone)]
pub struct SchemaTelemetryHistogram {
    record: Record,
    definition: Arc<TelemetryMetricDefinition>,
}

impl SchemaTelemetryHistogram {
    pub fn record(
        &self,
        value: f64,
        attributes: SpanAttributes,
        context: Option<&dyn TelemetryContext>,
    ) -> Result<(), SchemaViolation> {
        check_attributes(&self.definition.attributes, &attributes, true)?;
        (self.record)(value, attributes, context);
        Ok(())
    }
}

/// Instruments bound to the metric names, kinds, and metadata of one or more schemas.
#[derive(Clone)]
pub struct TypedTelemetryMetrics {
    metrics: Arc<dyn TelemetryMetrics>,
    definitions: Arc<BTreeMap<String, Arc<TelemetryMetricDefinition>>>,
}

/// Bind `metrics` to the combined metric vocabulary of `schemas`. Duplicate metric names fail.
pub fn create_typed_metrics(
    metrics: Arc<dyn TelemetryMetrics>,
    schemas: &[TelemetrySchemaDefinition],
) -> Result<TypedTelemetryMetrics, SchemaViolation> {
    let mut definitions = BTreeMap::new();
    for schema in schemas {
        for (name, definition) in schema.metrics.iter().flatten() {
            if definitions.insert(name.clone(), Arc::new(definition.clone())).is_some() {
                return Err(SchemaViolation(format!("Duplicate telemetry metric name: {name}")));
            }
        }
    }
    Ok(TypedTelemetryMetrics { metrics, definitions: Arc::new(definitions) })
}

impl TypedTelemetryMetrics {
    fn definition(
        &self,
        name: &str,
        kind: TelemetryMetricKind,
    ) -> Result<Arc<TelemetryMetricDefinition>, SchemaViolation> {
        match self.definitions.get(name) {
            Some(definition) if definition.kind == kind => Ok(definition.clone()),
            Some(_) => Err(SchemaViolation(format!("Telemetry metric {name} is not a {kind:?}"))),
            None => Err(SchemaViolation(format!("Unknown telemetry metric: {name}"))),
        }
    }

    pub fn counter(&self, name: &str) -> Result<SchemaTelemetryCounter, SchemaViolation> {
        let definition = self.definition(name, TelemetryMetricKind::Counter)?;
        let counter = self.metrics.counter(name, instrument_options(&definition));
        let record: Record = Arc::new(move |value, attributes, _| counter.add(value, attributes));
        Ok(SchemaTelemetryCounter { record, definition })
    }

    pub fn up_down_counter(&self, name: &str) -> Result<SchemaTelemetryCounter, SchemaViolation> {
        let definition = self.definition(name, TelemetryMetricKind::UpDownCounter)?;
        let counter = self.metrics.up_down_counter(name, instrument_options(&definition));
        let record: Record = Arc::new(move |value, attributes, _| counter.add(value, attributes));
        Ok(SchemaTelemetryCounter { record, definition })
    }

    pub fn histogram(&self, name: &str) -> Result<SchemaTelemetryHistogram, SchemaViolation> {
        let definition = self.definition(name, TelemetryMetricKind::Histogram)?;
        let options = HistogramOptions {
            description: Some(definition.description.clone()),
            unit: definition.unit.clone(),
            boundaries: definition.boundaries.clone(),
        };
        let histogram = self.metrics.histogram(name, options);
        let record: Record = Arc::new(move |value, attributes, context| histogram.record(value, attributes, context));
        Ok(SchemaTelemetryHistogram { record, definition })
    }
}

fn instrument_options(definition: &TelemetryMetricDefinition) -> InstrumentOptions {
    InstrumentOptions { description: Some(definition.description.clone()), unit: definition.unit.clone() }
}
