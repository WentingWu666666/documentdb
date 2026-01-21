/*-------------------------------------------------------------------------
 * Copyright (c) Microsoft Corporation.  All rights reserved.
 *
 * src/changestream/handler.rs
 *
 * Request handler for change stream commands ($changeStream aggregation).
 *
 *-------------------------------------------------------------------------
 */

use bson::{Document, RawDocument};

use super::cursor::{ChangeStreamCursor, ChangeStreamCursorStore, ChangeStreamOptions};
use super::resume_token::ResumeToken;
use super::translator::{FullDocumentBeforeChangeOption, FullDocumentOption};
use super::wal_reader::{WalReader, WalReaderConfig};
use crate::context::{ConnectionContext, RequestContext};
use crate::error::{DocumentDBError, ErrorCode, Result};
use crate::responses::{Response, RawResponse};

/// Global WAL reader instance (lazily initialized)
static WAL_READER: std::sync::OnceLock<WalReader> = std::sync::OnceLock::new();

/// Global change stream cursor store
static CURSOR_STORE: std::sync::OnceLock<ChangeStreamCursorStore> = std::sync::OnceLock::new();

/// Initialize the global WAL reader
pub fn init_wal_reader(connection_string: &str) {
    let config = WalReaderConfig {
        connection_string: connection_string.to_string(),
        ..Default::default()
    };
    let reader = WalReader::new(config);
    let _ = WAL_READER.set(reader);
}

/// Get or initialize the WAL reader
fn get_wal_reader(connection_context: &ConnectionContext) -> Result<&'static WalReader> {
    WAL_READER.get_or_init(|| {
        // Build connection string from service context
        // Use documentdb superuser with socket auth for replication
        let setup_config = connection_context.service_context.setup_configuration();
        let conn_str = format!(
            "host=/run/postgresql port={} dbname={} user=documentdb",
            setup_config.postgres_port(),
            setup_config.postgres_database(),
        );
        log::info!("WAL reader connection string: {}", conn_str);
        let config = WalReaderConfig {
            connection_string: conn_str,
            ..Default::default()
        };
        WalReader::new(config)
    });
    WAL_READER.get().ok_or_else(|| {
        DocumentDBError::internal_error("WAL reader not initialized".to_string())
    })
}

/// Get or initialize the cursor store
fn get_cursor_store(connection_context: &ConnectionContext) -> &'static ChangeStreamCursorStore {
    CURSOR_STORE.get_or_init(|| {
        let timeout_secs = connection_context
            .service_context
            .setup_configuration()
            .cursor_timeout_secs();
        ChangeStreamCursorStore::new(timeout_secs)
    })
}

/// Process a change stream request (aggregate with $changeStream)
pub async fn process_change_stream(
    request_context: &mut RequestContext<'_>,
    connection_context: &ConnectionContext,
) -> Result<Response> {
    let doc = request_context.payload.document();

    // Parse the aggregate command
    let db = request_context.info.db()?;
    let collection = get_collection_from_aggregate(doc)?;

    // Check if this is a change stream (first pipeline stage is $changeStream)
    let pipeline = doc
        .get_array("pipeline")
        .map_err(|_| DocumentDBError::bad_value("Missing pipeline array".to_string()))?;

    let first_stage = pipeline
        .into_iter()
        .next()
        .ok_or_else(|| DocumentDBError::bad_value("Empty pipeline".to_string()))?
        .map_err(|e| DocumentDBError::bad_value(format!("Invalid pipeline: {}", e)))?;

    let first_stage_doc = first_stage
        .as_document()
        .ok_or_else(|| DocumentDBError::bad_value("Pipeline stage must be a document".to_string()))?;

    // Check for $changeStream stage
    let change_stream_opts = first_stage_doc
        .get_document("$changeStream")
        .map_err(|_| {
            DocumentDBError::documentdb_error(
                ErrorCode::CommandNotSupported,
                "First pipeline stage must be $changeStream".to_string(),
            )
        })?;

    // Parse change stream options
    let options = parse_change_stream_options(
        change_stream_opts,
        doc,
        db,
        &collection,
        &pipeline_without_first(&pipeline)?,
    )?;

    // Get or start the WAL reader
    let wal_reader = get_wal_reader(connection_context)?;

    // Start WAL reader if not running
    if let Err(e) = wal_reader.start().await {
        log::error!("Failed to start WAL reader: {:?}", e);
        return Err(DocumentDBError::internal_error(format!(
            "Failed to start change stream: {}",
            e
        )));
    }

    // Get username for cursor ownership
    let username = connection_context
        .auth_state
        .username()
        .map(|s| s.to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    // Create the change stream cursor
    let mut cursor = ChangeStreamCursor::new(options, username, wal_reader.clone());

    // Get initial batch (may be empty if no changes yet)
    let max_time_ms = request_context.info.max_time_ms;
    let initial_batch = cursor.get_next_batch(max_time_ms).await?;

    // Build response
    let response_doc = cursor.build_initial_response(initial_batch)?;

    // Store cursor for getMore requests
    let cursor_store = get_cursor_store(connection_context);
    cursor_store.add_cursor(cursor).await;

    // Convert to raw response
    let raw_doc = bson::RawDocumentBuf::from_document(&response_doc)
        .map_err(|e| DocumentDBError::internal_error(format!("Failed to serialize response: {}", e)))?;

    Ok(Response::Raw(RawResponse(raw_doc)))
}

/// Process getMore for a change stream cursor
pub async fn process_change_stream_get_more(
    cursor_id: i64,
    connection_context: &ConnectionContext,
    max_time_ms: Option<i64>,
) -> Result<Response> {
    let username = connection_context
        .auth_state
        .username()
        .map(|s| s.to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    let cursor_store = get_cursor_store(connection_context);

    // Get the cursor
    let mut cursor = cursor_store
        .get_cursor(cursor_id, &username)
        .await
        .ok_or_else(|| {
            DocumentDBError::documentdb_error(
                ErrorCode::CursorNotFound,
                format!("Cursor not found: {}", cursor_id),
            )
        })?;

    // Get next batch
    let batch = cursor.get_next_batch(max_time_ms).await?;
    let response_doc = cursor.build_get_more_response(batch)?;

    // Return cursor to store
    cursor_store.return_cursor(cursor).await;

    let raw_doc = bson::RawDocumentBuf::from_document(&response_doc)
        .map_err(|e| DocumentDBError::internal_error(format!("Failed to serialize response: {}", e)))?;

    Ok(Response::Raw(RawResponse(raw_doc)))
}

/// Check if an aggregate pipeline is a change stream
pub fn is_change_stream_pipeline(doc: &RawDocument) -> bool {
    if let Ok(pipeline) = doc.get_array("pipeline") {
        if let Some(Ok(first_stage)) = pipeline.into_iter().next() {
            if let Some(stage_doc) = first_stage.as_document() {
                return stage_doc.get("$changeStream").is_ok();
            }
        }
    }
    false
}

/// Get collection name from aggregate command
fn get_collection_from_aggregate(doc: &RawDocument) -> Result<String> {
    // aggregate can be a string (collection name) or number (1 for db-level)
    if let Ok(Some(val)) = doc.get("aggregate") {
        match val {
            bson::RawBsonRef::String(s) => return Ok(s.to_string()),
            bson::RawBsonRef::Int32(1) | bson::RawBsonRef::Int64(1) => return Ok("".to_string()),
            _ => {}
        }
    }
    Err(DocumentDBError::bad_value(
        "Invalid aggregate target".to_string(),
    ))
}

/// Parse change stream options from the $changeStream stage
fn parse_change_stream_options(
    stage_doc: &RawDocument,
    cmd_doc: &RawDocument,
    db: &str,
    collection: &str,
    remaining_pipeline: &[Document],
) -> Result<ChangeStreamOptions> {
    let mut options = ChangeStreamOptions {
        database: if db.is_empty() { None } else { Some(db.to_string()) },
        collection: if collection.is_empty() {
            None
        } else {
            Some(collection.to_string())
        },
        pipeline: remaining_pipeline.to_vec(),
        ..Default::default()
    };

    // Parse fullDocument option
    if let Ok(fd) = stage_doc.get_str("fullDocument") {
        options.full_document = match fd {
            "default" => FullDocumentOption::Default,
            "updateLookup" => FullDocumentOption::UpdateLookup,
            "whenAvailable" => FullDocumentOption::WhenAvailable,
            "required" => FullDocumentOption::Required,
            _ => FullDocumentOption::Default,
        };
    }

    // Parse fullDocumentBeforeChange option
    if let Ok(fdbc) = stage_doc.get_str("fullDocumentBeforeChange") {
        options.full_document_before_change = match fdbc {
            "off" => FullDocumentBeforeChangeOption::Off,
            "whenAvailable" => FullDocumentBeforeChangeOption::WhenAvailable,
            "required" => FullDocumentBeforeChangeOption::Required,
            _ => FullDocumentBeforeChangeOption::Off,
        };
    }

    // Parse resumeAfter
    if let Ok(resume_doc) = stage_doc.get_document("resumeAfter") {
        options.resume_after = Some(ResumeToken::from_raw_document(resume_doc)?);
    }

    // Parse startAfter (same as resumeAfter for our purposes)
    if let Ok(start_doc) = stage_doc.get_document("startAfter") {
        options.resume_after = Some(ResumeToken::from_raw_document(start_doc)?);
    }

    // Parse startAtOperationTime
    if let Ok(ts) = stage_doc.get_timestamp("startAtOperationTime") {
        options.start_at_operation_time = Some(bson::Timestamp {
            time: ts.time,
            increment: ts.increment,
        });
    }

    // Parse batch size from cursor options
    if let Ok(cursor_doc) = cmd_doc.get_document("cursor") {
        if let Ok(bs) = cursor_doc.get_i32("batchSize") {
            options.batch_size = bs;
        }
    }

    // Parse maxAwaitTimeMS
    if let Ok(max_await) = cmd_doc.get_i64("maxAwaitTimeMS") {
        options.max_await_time_ms = Some(max_await);
    } else if let Ok(max_await) = cmd_doc.get_i32("maxAwaitTimeMS") {
        options.max_await_time_ms = Some(max_await as i64);
    }

    Ok(options)
}

/// Extract remaining pipeline stages (after $changeStream)
fn pipeline_without_first(pipeline: &bson::raw::RawArray) -> Result<Vec<Document>> {
    let mut stages = Vec::new();
    let mut first = true;

    for item in pipeline.into_iter() {
        if first {
            first = false;
            continue;
        }
        let raw_bson = item.map_err(|e| DocumentDBError::bad_value(format!("Invalid pipeline: {}", e)))?;
        if let Some(doc) = raw_bson.as_document() {
            let parsed: Document = bson::from_slice(doc.as_bytes())
                .map_err(|e| DocumentDBError::bad_value(format!("Invalid pipeline stage: {}", e)))?;
            stages.push(parsed);
        }
    }

    Ok(stages)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_change_stream_pipeline() {
        let doc = bson::rawdoc! {
            "aggregate": "test",
            "pipeline": [
                { "$changeStream": {} }
            ],
            "$db": "testdb"
        };
        assert!(is_change_stream_pipeline(&doc));

        let doc2 = bson::rawdoc! {
            "aggregate": "test",
            "pipeline": [
                { "$match": { "status": "active" } }
            ],
            "$db": "testdb"
        };
        assert!(!is_change_stream_pipeline(&doc2));
    }
}
