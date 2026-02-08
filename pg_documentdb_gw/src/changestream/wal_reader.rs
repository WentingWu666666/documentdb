/*-------------------------------------------------------------------------
 * Copyright (c) Microsoft Corporation.  All rights reserved.
 *
 * src/changestream/wal_reader.rs
 *
 * PostgreSQL WAL reader using logical replication with test_decoding plugin.
 * Streams changes from DocumentDB data tables.
 *
 *-------------------------------------------------------------------------
 */

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bson::doc;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, RwLock};
use tokio::task::JoinHandle;
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

use crate::error::{DocumentDBError, Result};

/// Represents a single change from test_decoding
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalChange {
    /// Type of change: INSERT, UPDATE, DELETE
    pub kind: String,
    /// Schema name (documentdb_data)
    pub schema: String,
    /// Table name (documents_<collection_id>)
    pub table: String,
    /// Column names
    #[serde(default)]
    pub columnnames: Vec<String>,
    /// Column types
    #[serde(default)]
    pub columntypes: Vec<String>,
    /// Column values (as strings)
    #[serde(default)]
    pub columnvalues: Vec<serde_json::Value>,
    /// Old key values (for updates/deletes with replica identity)
    #[serde(default)]
    pub oldkeys: Option<OldKeys>,
    /// Raw data string from test_decoding (for BSON extraction)
    #[serde(default)]
    pub raw_data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OldKeys {
    #[serde(default)]
    pub keynames: Vec<String>,
    #[serde(default)]
    pub keytypes: Vec<String>,
    #[serde(default)]
    pub keyvalues: Vec<serde_json::Value>,
}

/// Configuration for the WAL reader
#[derive(Debug, Clone)]
pub struct WalReaderConfig {
    /// PostgreSQL connection string for replication
    pub connection_string: String,
    /// Name of the replication slot
    pub slot_name: String,
    /// Publication name (if using pgoutput instead of wal2json)
    pub publication_name: Option<String>,
    /// Poll interval when no changes are available
    pub poll_interval: Duration,
    /// Maximum changes to fetch per poll
    pub max_changes_per_poll: i32,
}

impl Default for WalReaderConfig {
    fn default() -> Self {
        Self {
            connection_string: String::new(),
            slot_name: "documentdb_changestream".to_string(),
            publication_name: None,
            poll_interval: Duration::from_millis(100),
            max_changes_per_poll: 1000,
        }
    }
}

/// WAL Reader state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalReaderState {
    /// Not yet started
    Stopped,
    /// Starting up, connecting to PostgreSQL
    Starting,
    /// Connected and actively polling for changes
    Connected,
    /// Currently running (legacy, same as Connected)
    Running,
    /// Error state
    Error,
}

/// WAL Reader for streaming changes from PostgreSQL
pub struct WalReader {
    config: WalReaderConfig,
    state: Arc<RwLock<WalReaderState>>,
    /// Current LSN position
    current_lsn: Arc<RwLock<u64>>,
    /// Broadcast channel for distributing changes to subscribers
    change_tx: broadcast::Sender<Arc<WalChange>>,
    /// Handle to the reader task
    reader_task: Arc<RwLock<Option<JoinHandle<()>>>>,
    /// Collection metadata cache: collection_id -> (db_name, collection_name)
    collection_cache: Arc<RwLock<HashMap<i64, (String, String)>>>,
    /// Ready signal: notifies when connection is established
    ready_notify: Arc<tokio::sync::Notify>,
}

impl WalReader {
    /// Create a new WAL reader
    pub fn new(config: WalReaderConfig) -> Self {
        let (change_tx, _) = broadcast::channel(10000);

        Self {
            config,
            state: Arc::new(RwLock::new(WalReaderState::Stopped)),
            current_lsn: Arc::new(RwLock::new(0)),
            change_tx,
            reader_task: Arc::new(RwLock::new(None)),
            collection_cache: Arc::new(RwLock::new(HashMap::new())),
            ready_notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Subscribe to changes
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<WalChange>> {
        self.change_tx.subscribe()
    }

    /// Get current LSN
    pub async fn current_lsn(&self) -> u64 {
        *self.current_lsn.read().await
    }

    /// Get current state
    pub async fn state(&self) -> WalReaderState {
        *self.state.read().await
    }

    /// Start the WAL reader and wait for connection to be established
    pub async fn start(&self) -> Result<()> {
        let current_state = *self.state.read().await;
        if current_state == WalReaderState::Running || current_state == WalReaderState::Connected {
            return Ok(());
        }

        *self.state.write().await = WalReaderState::Starting;

        let config = self.config.clone();
        let state = Arc::clone(&self.state);
        let current_lsn = Arc::clone(&self.current_lsn);
        let change_tx = self.change_tx.clone();
        let collection_cache = Arc::clone(&self.collection_cache);
        let ready_notify = Arc::clone(&self.ready_notify);

        let task = tokio::spawn(async move {
            if let Err(e) = Self::reader_loop(
                config,
                state.clone(),
                current_lsn,
                change_tx,
                collection_cache,
                ready_notify,
            )
            .await
            {
                log::error!("WAL reader error: {:?}", e);
                *state.write().await = WalReaderState::Error;
            }
        });

        *self.reader_task.write().await = Some(task);

        // Wait for ready signal with timeout (max 5 seconds)
        let wait_result = tokio::time::timeout(
            Duration::from_secs(5),
            self.ready_notify.notified()
        ).await;

        match wait_result {
            Ok(_) => {
                log::info!("WAL reader connected and ready");
                Ok(())
            }
            Err(_) => {
                log::warn!("WAL reader start timed out, continuing anyway");
                Ok(()) // Don't fail, the reader might still connect
            }
        }
    }

    /// Stop the WAL reader
    pub async fn stop(&self) {
        *self.state.write().await = WalReaderState::Stopped;
        if let Some(task) = self.reader_task.write().await.take() {
            task.abort();
        }
    }

    /// Main reader loop
    async fn reader_loop(
        config: WalReaderConfig,
        state: Arc<RwLock<WalReaderState>>,
        current_lsn: Arc<RwLock<u64>>,
        change_tx: broadcast::Sender<Arc<WalChange>>,
        collection_cache: Arc<RwLock<HashMap<i64, (String, String)>>>,
        ready_notify: Arc<tokio::sync::Notify>,
    ) -> Result<()> {
        // Use regular connection (not replication mode) for polling
        // pg_logical_slot_get_changes works with regular connections
        let conn_str = config.connection_string.clone();
        let mut notified = false;

        loop {
            let current_state = *state.read().await;
            if current_state == WalReaderState::Stopped || current_state == WalReaderState::Error {
                break;
            }

            // Try to connect and read changes
            match Self::connect_and_read(
                &conn_str,
                &config,
                &current_lsn,
                &change_tx,
                &collection_cache,
                &state,
                &ready_notify,
                &mut notified,
            )
            .await
            {
                Ok(_) => {}
                Err(e) => {
                    log::warn!("WAL reader connection error, retrying: {:?}", e);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }

        Ok(())
    }

    /// Connect to PostgreSQL and read changes
    async fn connect_and_read(
        conn_str: &str,
        config: &WalReaderConfig,
        _current_lsn: &Arc<RwLock<u64>>,
        change_tx: &broadcast::Sender<Arc<WalChange>>,
        collection_cache: &Arc<RwLock<HashMap<i64, (String, String)>>>,
        state: &Arc<RwLock<WalReaderState>>,
        ready_notify: &Arc<tokio::sync::Notify>,
        notified: &mut bool,
    ) -> Result<()> {
        // Note: In production, use TLS. For prototype, using NoTls.
        let (client, connection) = tokio_postgres::connect(conn_str, NoTls)
            .await
            .map_err(|e| DocumentDBError::internal_error(format!("Failed to connect: {}", e)))?;

        // Spawn connection handler
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                log::error!("PostgreSQL connection error: {}", e);
            }
        });

        // Ensure replication slot exists
        Self::ensure_replication_slot(&client, &config.slot_name).await?;

        // Refresh collection cache
        Self::refresh_collection_cache(&client, collection_cache).await?;

        // Mark as connected and notify waiters (only once)
        *state.write().await = WalReaderState::Connected;
        if !*notified {
            log::info!("WAL reader connected, signaling ready");
            ready_notify.notify_waiters();
            *notified = true;
        }

        // Poll for changes (on-demand cache refresh only - no periodic refresh)
        loop {
            let changes =
                Self::poll_changes(&client, &config.slot_name, config.max_changes_per_poll).await?;

            if changes.is_empty() {
                tokio::time::sleep(config.poll_interval).await;
                continue;
            }

            for change in changes {
                // Filter for documentdb_data schema and documents_ tables
                if change.schema == "documentdb_data" && change.table.starts_with("documents_") {
                    // Extract collection_id from table name (documents_<id>)
                    let collection_id_str = change.table.strip_prefix("documents_").unwrap_or("0");
                    if let Ok(collection_id) = collection_id_str.parse::<i64>() {
                        // Check if collection is in cache, if not refresh immediately
                        let needs_refresh = {
                            let cache = collection_cache.read().await;
                            !cache.contains_key(&collection_id)
                        };
                        
                        if needs_refresh {
                            log::info!(
                                "WAL reader: unknown collection_id {}, refreshing cache on-demand",
                                collection_id
                            );
                            if let Err(e) = Self::refresh_collection_cache(&client, collection_cache).await {
                                log::warn!("Failed to refresh collection cache on-demand: {:?}", e);
                            }
                        }
                    }

                    log::debug!(
                        "WAL reader: processing change for {}.{} (kind: {})",
                        change.schema,
                        change.table,
                        change.kind
                    );
                    // Broadcast the change - subscribers will resolve collection names
                    let change_arc = Arc::new(change);
                    if change_tx.send(change_arc).is_err() {
                        // No active receivers, that's okay
                        log::trace!("WAL reader: no active receivers for change event");
                    }
                }
            }
        }
    }

    /// Ensure the replication slot exists, create if not
    async fn ensure_replication_slot(client: &Client, slot_name: &str) -> Result<()> {
        // Check if slot exists
        let check_query = format!(
            "SELECT slot_name FROM pg_replication_slots WHERE slot_name = '{}'",
            slot_name
        );

        let rows = client
            .simple_query(&check_query)
            .await
            .map_err(|e| DocumentDBError::internal_error(format!("Failed to check slot: {}", e)))?;

        let slot_exists = rows
            .iter()
            .any(|msg| matches!(msg, SimpleQueryMessage::Row(_)));

        if !slot_exists {
            // Create the replication slot with test_decoding (built-in)
            let create_query = format!(
                "SELECT pg_create_logical_replication_slot('{}', 'test_decoding')",
                slot_name
            );

            client.simple_query(&create_query).await.map_err(|e| {
                DocumentDBError::internal_error(format!("Failed to create replication slot: {}", e))
            })?;

            log::info!("Created replication slot: {}", slot_name);
        }

        Ok(())
    }

    /// Refresh the collection metadata cache
    async fn refresh_collection_cache(
        client: &Client,
        cache: &Arc<RwLock<HashMap<i64, (String, String)>>>,
    ) -> Result<()> {
        let query = "SELECT collection_id, database_name, collection_name 
                     FROM documentdb_api_catalog.collections 
                     WHERE collection_id IS NOT NULL";

        let rows = client.simple_query(query).await.map_err(|e| {
            DocumentDBError::internal_error(format!("Failed to query collections: {}", e))
        })?;

        let mut new_cache = HashMap::new();

        for msg in rows {
            if let SimpleQueryMessage::Row(row) = msg {
                if let (Some(id_str), Some(db), Some(coll)) = (row.get(0), row.get(1), row.get(2)) {
                    if let Ok(collection_id) = id_str.parse::<i64>() {
                        new_cache.insert(collection_id, (db.to_string(), coll.to_string()));
                    }
                }
            }
        }

        *cache.write().await = new_cache;
        Ok(())
    }

    /// Poll for changes from the replication slot
    async fn poll_changes(
        client: &Client,
        slot_name: &str,
        max_changes: i32,
    ) -> Result<Vec<WalChange>> {
        // Use pg_logical_slot_get_changes to consume changes with test_decoding
        let query = format!(
            "SELECT data FROM pg_logical_slot_get_changes('{}', NULL, {})",
            slot_name, max_changes
        );

        let rows = client.simple_query(&query).await.map_err(|e| {
            DocumentDBError::internal_error(format!("Failed to poll changes: {}", e))
        })?;

        let mut changes = Vec::new();

        for msg in rows {
            if let SimpleQueryMessage::Row(row) = msg {
                if let Some(data) = row.get(0) {
                    // Parse test_decoding output format:
                    // "table documentdb_data.documents_2: INSERT: shard_key_value[bigint]:2 object_id[documentdb_core.bson]:'BSONHEX...' document[documentdb_core.bson]:'BSONHEX...'"
                    if let Some(change) = Self::parse_test_decoding_output(data) {
                        changes.push(change);
                    }
                }
            }
        }

        Ok(changes)
    }

    /// Parse test_decoding output into a WalChange
    fn parse_test_decoding_output(data: &str) -> Option<WalChange> {
        // Format: "table schema.table: OPERATION: column[type]:value ..."
        // Example: "table documentdb_data.documents_2: INSERT: shard_key_value[bigint]:2 object_id[documentdb_core.bson]:'BSONHEX...' document[documentdb_core.bson]:'BSONHEX...'"

        // Skip non-table entries (BEGIN, COMMIT)
        if !data.starts_with("table ") {
            return None;
        }

        // Parse: "table schema.table: OPERATION: ..."
        let parts: Vec<&str> = data.splitn(3, ": ").collect();
        if parts.len() < 3 {
            return None;
        }

        // Extract schema.table
        let table_part = parts[0].strip_prefix("table ")?;
        let (schema, table) = table_part.split_once('.')?;

        // Extract operation type
        let operation = parts[1];
        let kind = match operation {
            "INSERT" => "INSERT",
            "UPDATE" => "UPDATE",
            "DELETE" => "DELETE",
            _ => return None,
        };

        Some(WalChange {
            kind: kind.to_string(),
            schema: schema.to_string(),
            table: table.to_string(),
            columnnames: Vec::new(),
            columntypes: Vec::new(),
            columnvalues: Vec::new(),
            oldkeys: None,
            raw_data: data.to_string(),
        })
    }

    /// Get collection info from cache
    pub async fn get_collection_info(&self, collection_id: i64) -> Option<(String, String)> {
        self.collection_cache
            .read()
            .await
            .get(&collection_id)
            .cloned()
    }

    /// Extract collection_id from table name (documents_<id>)
    pub fn extract_collection_id(table_name: &str) -> Option<i64> {
        if table_name.starts_with("documents_") {
            table_name
                .strip_prefix("documents_")
                .and_then(|s| s.parse().ok())
        } else {
            None
        }
    }
}

impl Clone for WalReader {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            state: Arc::clone(&self.state),
            current_lsn: Arc::clone(&self.current_lsn),
            change_tx: self.change_tx.clone(),
            reader_task: Arc::clone(&self.reader_task),
            collection_cache: Arc::clone(&self.collection_cache),
            ready_notify: Arc::clone(&self.ready_notify),
        }
    }
}
