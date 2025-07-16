/*-------------------------------------------------------------------------
 * Copyright (c) Microsoft Corporation.  All rights reserved.
 *
 * src/telemetry/metrics_config.rs
 *
 *-------------------------------------------------------------------------
 */

use opentelemetry::global;
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::metrics::PeriodicReader;

pub struct MetricsConfig {
    pub otlp_endpoint: String,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            otlp_endpoint: "http://localhost:4318/v1/metrics".to_string(),
        }
    }
}

impl MetricsConfig {
    pub fn new() -> Self {
        Self {
            ..Default::default()
        }
    }

    pub fn with_otlp_endpoint(mut self, endpoint: &str) -> Self {
        self.otlp_endpoint = endpoint.to_string();
        self
    }

    pub fn init_metrics(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Initialize OTLP exporter
        let exporter = opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_endpoint(&self.otlp_endpoint)
            .build()?;

        // Create a meter provider with the OTLP Metric exporter
        let meter_provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
            .with_periodic_exporter(exporter)
            .build();
        global::set_meter_provider(meter_provider.clone());

        let meter = global::meter("request");

        // TODO: this is just an example of how to create a counter
        let counter = meter.u64_counter("my_counter").build();
        counter.add(1, &[KeyValue::new("key", "value")]);

        Ok(())
    }
}