//! Serializable telemetry schemas and schema-checked span starters.
//! Upstream checks span names and attributes at compile time through TypeScript inference;
//! here the typed starter checks them when a span starts, before the callback runs.

use std::collections::BTreeMap;
use std::fmt::Display;
use std::future::Future;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::attributes::{AttributeValue, SpanAttributes};
use crate::span::{SpanFuture, SpanOptions, SpanStatus, TelemetryContext, TelemetryContextExt, TelemetrySpan};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TelemetryAttributeType {
    #[serde(rename = "string")]
    String,
    #[serde(rename = "number")]
    Number,
    #[serde(rename = "boolean")]
    Boolean,
    #[serde(rename = "string[]")]
    StringArray,
    #[serde(rename = "number[]")]
    NumberArray,
    #[serde(rename = "boolean[]")]
    BooleanArray,
}

impl TelemetryAttributeType {
    fn name(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::StringArray => "string[]",
            Self::NumberArray => "number[]",
            Self::BooleanArray => "boolean[]",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TelemetryAttributeCardinality {
    Low,
    High,
}

/// One attribute. `values` closes a scalar type's set; `element_values` closes an array's elements.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryAttributeDefinition {
    #[serde(rename = "type")]
    pub attribute_type: TelemetryAttributeType,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensitive: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cardinality: Option<TelemetryAttributeCardinality>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<AttributeValue>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element_values: Option<Vec<AttributeValue>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub examples: Option<Vec<AttributeValue>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TelemetryEventDefinition {
    pub description: String,
    pub attributes: BTreeMap<String, TelemetryAttributeDefinition>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TelemetryParentDefinition {
    Any,
    RootOrExternal,
    Spans { spans: Vec<String> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryStatusDefinition {
    /// Always `ok`.
    pub default: String,
    pub error_when: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetrySpanDefinition {
    pub description: String,
    pub parents: TelemetryParentDefinition,
    pub start_attributes: BTreeMap<String, TelemetryAttributeDefinition>,
    pub end_attributes: BTreeMap<String, TelemetryAttributeDefinition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events: Option<BTreeMap<String, TelemetryEventDefinition>>,
    pub status: TelemetryStatusDefinition,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TelemetrySchemaDefinition {
    pub version: u32,
    pub spans: BTreeMap<String, TelemetrySpanDefinition>,
}

/// Identity helper kept for parity with upstream `defineTelemetrySchema`.
pub fn define_telemetry_schema(schema: TelemetrySchemaDefinition) -> TelemetrySchemaDefinition {
    schema
}

/// A span name, attribute, or event that the combined schemas do not allow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaViolation(pub String);

impl Display for SchemaViolation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for SchemaViolation {}

fn violation<T>(message: String) -> Result<T, SchemaViolation> {
    Err(SchemaViolation(message))
}

fn check_value(
    name: &str,
    definition: &TelemetryAttributeDefinition,
    value: &AttributeValue,
) -> Result<(), SchemaViolation> {
    if value.type_name() != definition.attribute_type.name() {
        return violation(format!(
            "Attribute {name} must be {}, got {}",
            definition.attribute_type.name(),
            value.type_name()
        ));
    }
    if let Some(values) = &definition.values
        && !values.contains(value)
    {
        return violation(format!("Attribute {name} is outside its closed value set"));
    }
    if let Some(allowed) = &definition.element_values {
        let elements: Vec<AttributeValue> = match value {
            AttributeValue::StringArray(items) => items.iter().cloned().map(AttributeValue::from).collect(),
            AttributeValue::NumberArray(items) => items.iter().copied().map(AttributeValue::from).collect(),
            AttributeValue::BooleanArray(items) => items.iter().copied().map(AttributeValue::from).collect(),
            _ => Vec::new(),
        };
        if elements.iter().any(|element| !allowed.contains(element)) {
            return violation(format!("Attribute {name} has an element outside its closed value set"));
        }
    }
    Ok(())
}

/// Check `attributes` against `definitions`; `enforce_required` honors `required` flags.
fn check_attributes(
    definitions: &BTreeMap<String, TelemetryAttributeDefinition>,
    attributes: &SpanAttributes,
    enforce_required: bool,
) -> Result<(), SchemaViolation> {
    for (name, value) in attributes.iter() {
        let Some(definition) = definitions.get(name) else {
            return violation(format!("Undeclared attribute: {name}"));
        };
        check_value(name, definition, value)?;
    }
    if enforce_required {
        for (name, definition) in definitions {
            if definition.required == Some(true) && attributes.get(name).is_none() {
                return violation(format!("Missing required attribute: {name}"));
            }
        }
    }
    Ok(())
}

impl TelemetrySpanDefinition {
    pub fn check_start_attributes(&self, attributes: &SpanAttributes) -> Result<(), SchemaViolation> {
        check_attributes(&self.start_attributes, attributes, true)
    }

    pub fn check_end_attributes(&self, attributes: &SpanAttributes) -> Result<(), SchemaViolation> {
        check_attributes(&self.end_attributes, attributes, false)
    }

    pub fn check_event(&self, name: &str, attributes: &SpanAttributes) -> Result<(), SchemaViolation> {
        let Some(event) = self.events.as_ref().and_then(|events| events.get(name)) else {
            return violation(format!("Undeclared event: {name}"));
        };
        check_attributes(&event.attributes, attributes, true)
    }
}

/// A span restricted to one schema span's end attributes and events.
#[derive(Clone)]
pub struct SchemaTelemetrySpan {
    span: Arc<dyn TelemetrySpan>,
    definition: Arc<TelemetrySpanDefinition>,
}

impl SchemaTelemetrySpan {
    pub fn add_event(&self, name: &str, attributes: SpanAttributes) -> Result<(), SchemaViolation> {
        self.definition.check_event(name, &attributes)?;
        self.span.add_event(name, attributes);
        Ok(())
    }

    pub fn set_attributes(&self, attributes: SpanAttributes) -> Result<(), SchemaViolation> {
        self.definition.check_end_attributes(&attributes)?;
        self.span.set_attributes(attributes);
        Ok(())
    }

    pub fn set_status(&self, status: SpanStatus) {
        self.span.set_status(status);
    }

    /// The untyped span, for adapters and pass-through parents.
    pub fn raw(&self) -> &Arc<dyn TelemetrySpan> {
        &self.span
    }
}

/// A span used as the parent context of a child starter.
struct ParentSpan(Arc<dyn TelemetrySpan>);

impl TelemetryContext for ParentSpan {
    fn open_span(&self, options: SpanOptions) -> Arc<dyn TelemetrySpan> {
        self.0.open_span(options)
    }
}

/// Starts schema spans under one explicit parent; children bind to their parent span.
#[derive(Clone)]
pub struct TypedSpanStarter {
    context: Arc<dyn TelemetryContext>,
    spans: Arc<BTreeMap<String, Arc<TelemetrySpanDefinition>>>,
}

/// Bind `context` to the combined span vocabulary of `schemas`. Duplicate span names fail.
pub fn create_typed_span_starter(
    context: Arc<dyn TelemetryContext>,
    schemas: &[TelemetrySchemaDefinition],
) -> Result<TypedSpanStarter, SchemaViolation> {
    let mut spans = BTreeMap::new();
    for schema in schemas {
        for (name, definition) in &schema.spans {
            if spans.insert(name.clone(), Arc::new(definition.clone())).is_some() {
                return violation(format!("Duplicate telemetry span name: {name}"));
            }
        }
    }
    Ok(TypedSpanStarter { context, spans: Arc::new(spans) })
}

impl TypedSpanStarter {
    /// Check the span name and start attributes, then admit `callback` with the schema span and
    /// a starter bound to it. A violation is returned before the callback runs.
    pub fn start_span<F, Fut, T, E>(
        &self,
        name: &str,
        attributes: SpanAttributes,
        callback: F,
    ) -> Result<SpanFuture<Fut>, SchemaViolation>
    where
        F: FnOnce(SchemaTelemetrySpan, TypedSpanStarter) -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: Display,
    {
        let Some(definition) = self.spans.get(name).cloned() else {
            return violation(format!("Unknown telemetry span: {name}"));
        };
        definition.check_start_attributes(&attributes)?;
        let spans = self.spans.clone();
        let options = SpanOptions::new(name).with_attributes(attributes);
        Ok(self.context.start_span(options, move |span| {
            let child = TypedSpanStarter { context: Arc::new(ParentSpan(span.clone())), spans };
            callback(SchemaTelemetrySpan { span, definition }, child)
        }))
    }
}
