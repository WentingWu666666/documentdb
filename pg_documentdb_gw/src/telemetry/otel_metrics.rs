/*-------------------------------------------------------------------------
 * Copyright (c) Microsoft Corporation.  All rights reserved.
 *
 * src/telemetry/otel_metrics.rs
 *
 *-------------------------------------------------------------------------
 */

use crate::context::ConnectionContext;
use crate::protocol::header::Header;
use crate::requests::{Request, RequestInfo};
use crate::responses::{CommandError, Response};
use crate::telemetry::{error_code_to_status_code, TelemetryProvider};
use async_trait::async_trait;
use either::Either::{self, Left, Right};
use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter, MeterProvider};
use opentelemetry::{global, KeyValue};
use std::time::Duration;
use ntex::http::StatusCode;


/// OpenTelemetry metrics implementation of TelemetryProvider
#[derive(Clone)]
pub struct OpenTelemetryMetricsProvider {
    meter: Meter,
    request_duration_ms: Gauge<u64>,
}

impl OpenTelemetryMetricsProvider {
    pub fn new() -> Self {
        let meter = global::meter_provider().meter("request");

        // MongoRequestDurationMs
        let request_duration_ms = meter
            .u64_gauge("MongoRequestDurationMs")
            .with_unit("ms")
            .with_description("The duration of MongoDB requests in milliseconds")
            .build();

        Self {
            meter,
            request_duration_ms,
        }
    }

    fn build_common_attributes(
        &self,
    ) -> Vec<KeyValue> {
        let mut attributes = Vec::new();

        attributes
    }

    fn send_request_metric(
        &self,
        duration: Duration,
        request: &Option<&Request<'_>>,
        response: &Either<&Response, (&CommandError, usize)>,
        collection: String,
    ) {
        let mut attributes = self.build_common_attributes();
            let db = request.and_then(|r| r.db().ok()).unwrap_or("");
        
        let error_code = match response {
            Left(_) => 0,
            Right((e, _)) => e.code,
        };
        let operation_name = request.map_or("unknown".to_string(), |r| r.request_type().to_string());
        let status_code = error_code_to_status_code(error_code);
        let status_code_class = status_code_to_class(&status_code);
        let status_text = status_code.canonical_reason().unwrap_or_default();
        
        attributes.push(KeyValue::new("Authentication", "SASL"));
        attributes.push(KeyValue::new("CollectionName", collection));
        attributes.push(KeyValue::new("Protocol", "TCP"));
        attributes.push(KeyValue::new("DatabaseName", db.to_string()));
        attributes.push(KeyValue::new("ErrorCode", error_code as i64));
        attributes.push(KeyValue::new("Operation", operation_name));
        attributes.push(KeyValue::new("StatusCode", status_code.as_u16() as i64));
        attributes.push(KeyValue::new("StatusCodeClass", status_code_class));
        attributes.push(KeyValue::new("StatusText", status_text.to_string()));

        self.request_duration_ms.record(
            duration.as_millis() as u64,
            &attributes,
        );
    }
}

#[async_trait]
impl TelemetryProvider for OpenTelemetryMetricsProvider {
    async fn emit_request_event(
        &self,
        connection_context: &ConnectionContext,
        header: &Header,
        request: Option<&Request<'_>>,
        response: Either<&Response, (&CommandError, usize)>,
        collection: String,
        request_info: &mut RequestInfo<'_>,
    ) {
        // Record basic request metrics
        self.send_request_metric(
            connection_context.start_time.elapsed(),
            &request,
            &response,
            collection.clone(),
        );
    }
}

fn status_code_to_class(status: &StatusCode) -> &'static str {
    let trunc = status.as_u16() / 100;
    match trunc {
        1 => "1xx",
        2 => "2xx",
        3 => "3xx",
        4 => "4xx",
        5 => "5xx",
        _ => "6xx",
    }
}