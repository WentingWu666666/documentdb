/*-------------------------------------------------------------------------
 * Copyright (c) Microsoft Corporation.  All rights reserved.
 *
 * src/changestream/cursor.rs
 *
 * Change stream cursor implementation for streaming events to clients.
 *
 *-------------------------------------------------------------------------
 */

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bson::{doc, Document};
use tokio::sync::{broadcast, RwLock};
use tokio::task::JoinHandle;

use super::resume_token::ResumeToken;
use super::translator::{ChangeEvent, ChangeEventTranslator, TranslatorOptions};
use super::wal_reader::{WalChange, WalReader};
use crate::error::{DocumentDBError, Result};

/// Global cursor ID generator
static CURSOR_ID_COUNTER: AtomicI64 = AtomicI64::new(1);

/// Generate a new unique cursor ID
fn generate_cursor_id() -> i64 {
    CURSOR_ID_COUNTER.fetch_add(1, Ordering::SeqCst)
}

/// Options for a change stream cursor
#[derive(Debug, Clone)]
pub struct ChangeStreamOptions {
    /// Database to watch (None = all databases)
    pub database: Option<String>,
    /// Collection to watch (None = all collections in database)
    pub collection: Option<String>,
    /// Pipeline stages for filtering/transforming events
    pub pipeline: Vec<Document>,
    /// Batch size for getMore
    pub batch_size: i32,
    /// Resume token to start from
    pub resume_after: Option<ResumeToken>,
    /// Start at a specific operation time
    pub start_at_operation_time: Option<bson::Timestamp>,
    /// Full document option
    pub full_document: super::translator::FullDocumentOption,
    /// Full document before change option
    pub full_document_before_change: super::translator::FullDocumentBeforeChangeOption,
    /// Max await time for getMore
    pub max_await_time_ms: Option<i64>,
}

impl Default for ChangeStreamOptions {
    fn default() -> Self {
        Self {
            database: None,
            collection: None,
            pipeline: Vec::new(),
            batch_size: 101, // MongoDB default
            resume_after: None,
            start_at_operation_time: None,
            full_document: super::translator::FullDocumentOption::Default,
            full_document_before_change: super::translator::FullDocumentBeforeChangeOption::Off,
            max_await_time_ms: None,
        }
    }
}

/// A change stream cursor that streams events to a client
pub struct ChangeStreamCursor {
    /// Unique cursor ID
    pub cursor_id: i64,
    /// Options for this cursor
    pub options: ChangeStreamOptions,
    /// Username that owns this cursor
    pub username: String,
    /// Event translator
    translator: ChangeEventTranslator,
    /// Channel receiver for WAL changes
    change_rx: broadcast::Receiver<Arc<WalChange>>,
    /// Reference to WAL reader for collection lookups
    wal_reader: WalReader,
    /// Buffered events ready to send
    event_buffer: Vec<ChangeEvent>,
    /// Last resume token sent
    pub last_resume_token: Option<ResumeToken>,
    /// Creation time for TTL
    pub created_at: Instant,
    /// Last access time for TTL
    pub last_accessed: Instant,
    /// Whether the cursor is closed
    pub closed: bool,
    /// Last LSN from replay buffer - used to skip duplicates from broadcast
    replay_last_lsn: Option<u64>,
    /// Set of LSNs already emitted - prevents duplicates from replay + broadcast
    seen_lsns: HashSet<u64>,
}

impl ChangeStreamCursor {
    /// Create a new change stream cursor
    /// If options.resume_after is provided, missed changes will be replayed from that LSN
    pub async fn new(options: ChangeStreamOptions, username: String, wal_reader: WalReader) -> Self {
        let cursor_id = generate_cursor_id();
        let change_rx = wal_reader.subscribe();

        let translator_options = TranslatorOptions {
            full_document: options.full_document,
            full_document_before_change: options.full_document_before_change,
        };

        // If resume_after is provided, replay missed changes from that LSN
        let mut replay_buffer: Vec<ChangeEvent> = Vec::new();
        let mut replay_last_lsn: Option<u64> = None;
        
        if let Some(ref resume_token) = options.resume_after {
            let start_lsn = resume_token.lsn;
            log::info!(
                "Cursor {} resuming from LSN {} (collection_id: {}, db filter: {:?}, coll filter: {:?})",
                cursor_id,
                start_lsn,
                resume_token.collection_id,
                options.database,
                options.collection
            );

            // Replay changes from the WAL starting at the resume token's LSN
            match wal_reader.replay_from_lsn(start_lsn).await {
                Ok(changes) => {
                    log::info!("Replayed {} changes from LSN {}", changes.len(), start_lsn);
                    
                    // Create a temporary translator to convert changes to events
                    let temp_translator = ChangeEventTranslator::with_options(translator_options.clone());
                    
                    for change in changes {
                        // Track max LSN for deduplication against broadcast
                        if change.lsn > replay_last_lsn.unwrap_or(0) {
                            replay_last_lsn = Some(change.lsn);
                        }
                        
                        // Extract collection_id from table name
                        if let Some(collection_id) = WalReader::extract_collection_id(&change.table) {
                            // Get collection info for this change
                            if let Some((db_name, coll_name)) = wal_reader.get_collection_info(collection_id).await {
                                // Filter by database/collection if options specify them
                                let db_matches = options.database.as_ref()
                                    .map(|d| d == &db_name)
                                    .unwrap_or(true);
                                let coll_matches = options.collection.as_ref()
                                    .map(|c| c == &coll_name)
                                    .unwrap_or(true);
                                
                                if db_matches && coll_matches {
                                    // Use the LSN from the change itself
                                    let change_lsn = change.lsn;
                                    log::info!(
                                        "Replay: including change for {}.{} (lsn: {})",
                                        db_name, coll_name, change_lsn
                                    );
                                    match temp_translator.translate(&change, &db_name, &coll_name, collection_id, change_lsn) {
                                        Ok(event) => {
                                            replay_buffer.push(event);
                                        }
                                        Err(e) => {
                                            log::warn!("Failed to translate replayed change: {:?}", e);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    log::info!("Buffered {} events for replay to cursor {} (replay_last_lsn: {:?})", 
                              replay_buffer.len(), cursor_id, replay_last_lsn);
                }
                Err(e) => {
                    log::warn!("Failed to replay changes from LSN {}: {:?}", start_lsn, e);
                }
            }
        }

        // Build set of LSNs from replay buffer to prevent duplicates from broadcast
        let seen_lsns: HashSet<u64> = replay_buffer.iter()
            .map(|e| e.resume_token.lsn)
            .collect();

        Self {
            cursor_id,
            options,
            username,
            translator: ChangeEventTranslator::with_options(translator_options),
            change_rx,
            wal_reader,
            event_buffer: replay_buffer,
            last_resume_token: None,
            created_at: Instant::now(),
            last_accessed: Instant::now(),
            closed: false,
            replay_last_lsn,
            seen_lsns,
        }
    }

    /// Get the next batch of change events
    pub async fn get_next_batch(&mut self, max_time_ms: Option<i64>) -> Result<Vec<ChangeEvent>> {
        if self.closed {
            return Err(DocumentDBError::internal_error(
                "Cursor is closed".to_string(),
            ));
        }

        self.last_accessed = Instant::now();
        let batch_size = self.options.batch_size as usize;
        let max_wait = Duration::from_millis(
            max_time_ms
                .or(self.options.max_await_time_ms)
                .unwrap_or(1000) as u64,
        );

        let deadline = Instant::now() + max_wait;
        let mut batch = Vec::new();

        // First, drain any buffered events
        while !self.event_buffer.is_empty() && batch.len() < batch_size {
            batch.push(self.event_buffer.remove(0));
        }

        // Then try to receive more events until batch is full or timeout
        while batch.len() < batch_size && Instant::now() < deadline {
            let remaining = deadline - Instant::now();

            match tokio::time::timeout(remaining, self.change_rx.recv()).await {
                Ok(Ok(change)) => {
                    // Skip events that were already emitted (from replay buffer)
                    // This prevents duplicates when resumeAfter is used
                    if self.seen_lsns.contains(&change.lsn) {
                        log::trace!(
                            "Skipping change with LSN {} (already in seen_lsns)",
                            change.lsn
                        );
                        continue;
                    }
                    
                    if let Some(event) = self.process_change(&change).await {
                        if self.matches_filter(&event) {
                            // Track this LSN as seen
                            self.seen_lsns.insert(change.lsn);
                            batch.push(event);
                        }
                    }
                }
                Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                    log::warn!(
                        "Change stream cursor {} lagged by {} events",
                        self.cursor_id,
                        n
                    );
                    // Continue receiving
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => {
                    // WAL reader stopped
                    self.closed = true;
                    break;
                }
                Err(_) => {
                    // Timeout, return what we have
                    break;
                }
            }
        }

        // Update last resume token
        if let Some(last_event) = batch.last() {
            self.last_resume_token = Some(last_event.resume_token.clone());
        }

        Ok(batch)
    }

    /// Process a WAL change into a change event
    async fn process_change(&self, change: &WalChange) -> Option<ChangeEvent> {
        // Extract collection_id from table name
        let collection_id = WalReader::extract_collection_id(&change.table)?;

        // Look up collection metadata
        let collection_info = self.wal_reader.get_collection_info(collection_id).await;
        if collection_info.is_none() {
            log::debug!(
                "Change stream: collection_id {} not found in cache (table: {}), event dropped",
                collection_id,
                change.table
            );
            return None;
        }
        let (db_name, collection_name) = collection_info.unwrap();

        // Use the LSN from the change itself (populated by poll_changes)
        let lsn = change.lsn;

        // Translate the change
        match self
            .translator
            .translate(change, &db_name, &collection_name, collection_id, lsn)
        {
            Ok(event) => Some(event),
            Err(e) => {
                log::warn!("Failed to translate WAL change: {:?}", e);
                None
            }
        }
    }

    /// Check if an event matches the cursor's filters
    fn matches_filter(&self, event: &ChangeEvent) -> bool {
        // Check database filter
        if let Some(ref db) = self.options.database {
            if &event.namespace.db != db {
                return false;
            }
        }

        // Check collection filter
        if let Some(ref coll) = self.options.collection {
            if &event.namespace.coll != coll {
                return false;
            }
        }

        // Apply pipeline filters (simplified - only supports $match at first position)
        if let Some(first_stage) = self.options.pipeline.first() {
            if let Ok(match_doc) = first_stage.get_document("$match") {
                // Simple field matching
                let event_doc = event.to_document();
                for (key, value) in match_doc.iter() {
                    if let Some(event_value) = event_doc.get(key) {
                        if event_value != value {
                            return false;
                        }
                    } else {
                        return false;
                    }
                }
            }
        }

        true
    }

    /// Close the cursor
    pub fn close(&mut self) {
        self.closed = true;
    }

    /// Build the initial response for a change stream (first batch)
    pub fn build_initial_response(&self, first_batch: Vec<ChangeEvent>) -> Result<Document> {
        let cursor_doc = self.build_cursor_document(first_batch, true)?;
        // operationTime is required by MongoDB drivers
        let operation_time = bson::Timestamp {
            time: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as u32,
            increment: 1,
        };
        Ok(doc! {
            "cursor": cursor_doc,
            "operationTime": operation_time,
            "ok": 1.0,
        })
    }

    /// Build the getMore response
    pub fn build_get_more_response(&self, batch: Vec<ChangeEvent>) -> Result<Document> {
        let cursor_doc = self.build_cursor_document(batch, false)?;
        let operation_time = bson::Timestamp {
            time: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as u32,
            increment: 1,
        };
        Ok(doc! {
            "cursor": cursor_doc,
            "operationTime": operation_time,
            "ok": 1.0,
        })
    }

    /// Build cursor document with batch
    fn build_cursor_document(
        &self,
        batch: Vec<ChangeEvent>,
        is_first_batch: bool,
    ) -> Result<Document> {
        let batch_docs: Vec<Document> = batch.iter().map(|e| e.to_document()).collect();

        let batch_key = if is_first_batch {
            "firstBatch"
        } else {
            "nextBatch"
        };

        let mut cursor_doc = doc! {
            "id": if self.closed { 0i64 } else { self.cursor_id },
            "ns": format!("{}.{}",
                self.options.database.as_deref().unwrap_or("admin"),
                self.options.collection.as_deref().unwrap_or("$cmd.aggregate")
            ),
        };
        cursor_doc.insert(batch_key, batch_docs);

        // Include post batch resume token
        if let Some(ref token) = self.last_resume_token {
            cursor_doc.insert("postBatchResumeToken", token.to_bson());
        }

        Ok(cursor_doc)
    }
}

/// Store for managing change stream cursors
pub struct ChangeStreamCursorStore {
    cursors: Arc<RwLock<HashMap<(i64, String), ChangeStreamCursor>>>,
    /// Reaper task handle
    _reaper: Option<JoinHandle<()>>,
}

impl ChangeStreamCursorStore {
    /// Create a new cursor store
    pub fn new(cursor_timeout_secs: u64) -> Self {
        let cursor_timeout = Duration::from_secs(cursor_timeout_secs);
        let cursors: Arc<RwLock<HashMap<(i64, String), ChangeStreamCursor>>> =
            Arc::new(RwLock::new(HashMap::new()));

        let cursors_clone = Arc::clone(&cursors);
        let reaper = tokio::spawn(async move {
            let mut interval = tokio::time::interval(cursor_timeout / 10);
            loop {
                interval.tick().await;
                let mut store = cursors_clone.write().await;
                store.retain(|_, cursor| {
                    cursor.last_accessed.elapsed() < cursor_timeout && !cursor.closed
                });
            }
        });

        Self {
            cursors,
            _reaper: Some(reaper),
        }
    }

    /// Add a cursor to the store
    pub async fn add_cursor(&self, cursor: ChangeStreamCursor) {
        let key = (cursor.cursor_id, cursor.username.clone());
        self.cursors.write().await.insert(key, cursor);
    }

    /// Get a cursor from the store (removes it for exclusive access)
    pub async fn get_cursor(&self, cursor_id: i64, username: &str) -> Option<ChangeStreamCursor> {
        self.cursors
            .write()
            .await
            .remove(&(cursor_id, username.to_string()))
    }

    /// Return a cursor to the store after use
    pub async fn return_cursor(&self, cursor: ChangeStreamCursor) {
        if !cursor.closed {
            let key = (cursor.cursor_id, cursor.username.clone());
            self.cursors.write().await.insert(key, cursor);
        }
    }

    /// Kill specific cursors
    pub async fn kill_cursors(&self, username: &str, cursor_ids: &[i64]) -> (Vec<i64>, Vec<i64>) {
        let mut killed = Vec::new();
        let mut not_found = Vec::new();

        let mut store = self.cursors.write().await;
        for &cursor_id in cursor_ids {
            let key = (cursor_id, username.to_string());
            if store.remove(&key).is_some() {
                killed.push(cursor_id);
            } else {
                not_found.push(cursor_id);
            }
        }

        (killed, not_found)
    }
}
