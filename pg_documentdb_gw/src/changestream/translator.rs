/*-------------------------------------------------------------------------
 * Copyright (c) Microsoft Corporation.  All rights reserved.
 *
 * src/changestream/translator.rs
 *
 * Translates PostgreSQL WAL changes to MongoDB change stream event format.
 *
 *-------------------------------------------------------------------------
 */

use bson::{doc, Document, RawDocumentBuf};
use std::collections::HashMap;

use super::resume_token::ResumeToken;
use super::wal_reader::WalChange;
use crate::error::{DocumentDBError, Result};

/// MongoDB change stream operation types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationType {
    Insert,
    Update,
    Replace,
    Delete,
    Drop,
    Rename,
    DropDatabase,
    Invalidate,
}

impl OperationType {
    pub fn as_str(&self) -> &'static str {
        match self {
            OperationType::Insert => "insert",
            OperationType::Update => "update",
            OperationType::Replace => "replace",
            OperationType::Delete => "delete",
            OperationType::Drop => "drop",
            OperationType::Rename => "rename",
            OperationType::DropDatabase => "dropDatabase",
            OperationType::Invalidate => "invalidate",
        }
    }
}

/// A MongoDB change stream event
#[derive(Debug, Clone)]
pub struct ChangeEvent {
    /// Resume token for this event
    pub resume_token: ResumeToken,
    /// Type of operation
    pub operation_type: OperationType,
    /// Namespace (database and collection)
    pub namespace: Namespace,
    /// Document key (_id)
    pub document_key: Option<Document>,
    /// Full document (for inserts, updates with fullDocument option)
    pub full_document: Option<Document>,
    /// Full document before change (if configured)
    pub full_document_before_change: Option<Document>,
    /// Update description (for updates)
    pub update_description: Option<UpdateDescription>,
}

/// Namespace containing database and collection names
#[derive(Debug, Clone)]
pub struct Namespace {
    pub db: String,
    pub coll: String,
}

/// Description of fields updated in an update operation
#[derive(Debug, Clone)]
pub struct UpdateDescription {
    /// Fields that were updated
    pub updated_fields: Document,
    /// Fields that were removed
    pub removed_fields: Vec<String>,
    /// Array element changes (for advanced array updates)
    pub truncated_arrays: Vec<Document>,
}

/// Options for change event translation
#[derive(Debug, Clone)]
pub struct TranslatorOptions {
    /// Include full document in change events
    pub full_document: FullDocumentOption,
    /// Include full document before change
    pub full_document_before_change: FullDocumentBeforeChangeOption,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FullDocumentOption {
    /// Don't include full document (default for updates/deletes)
    #[default]
    Default,
    /// Always include the most recent document
    UpdateLookup,
    /// Include full document when available from oplog
    WhenAvailable,
    /// Require full document, error if not available
    Required,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FullDocumentBeforeChangeOption {
    /// Don't include pre-image
    #[default]
    Off,
    /// Include if available
    WhenAvailable,
    /// Require pre-image
    Required,
}

impl Default for TranslatorOptions {
    fn default() -> Self {
        Self {
            full_document: FullDocumentOption::Default,
            full_document_before_change: FullDocumentBeforeChangeOption::Off,
        }
    }
}

/// Translator for converting WAL changes to MongoDB change events
pub struct ChangeEventTranslator {
    #[allow(dead_code)]
    options: TranslatorOptions,
}

impl ChangeEventTranslator {
    /// Create a new translator with default options
    pub fn new() -> Self {
        Self {
            options: TranslatorOptions::default(),
        }
    }

    /// Create a new translator with custom options
    pub fn with_options(options: TranslatorOptions) -> Self {
        Self { options }
    }

    /// Translate a WAL change to a MongoDB change event
    pub fn translate(
        &self,
        change: &WalChange,
        db_name: &str,
        collection_name: &str,
        collection_id: i64,
        lsn: u64,
    ) -> Result<ChangeEvent> {
        let operation_type = match change.kind.to_uppercase().as_str() {
            "INSERT" => OperationType::Insert,
            "UPDATE" => OperationType::Update,
            "DELETE" => OperationType::Delete,
            other => {
                return Err(DocumentDBError::internal_error(format!(
                    "Unknown WAL change kind: {}",
                    other
                )));
            }
        };

        let namespace = Namespace {
            db: db_name.to_string(),
            coll: collection_name.to_string(),
        };

        let resume_token = ResumeToken::new(lsn, collection_id);

        // Extract document data from test_decoding raw output
        let (document_key, full_document, full_document_before_change) =
            self.extract_documents_from_test_decoding(change, operation_type)?;

        // For updates, try to compute update description
        let update_description = if operation_type == OperationType::Update {
            self.compute_update_description(change, &full_document_before_change, &full_document)
        } else {
            None
        };

        Ok(ChangeEvent {
            resume_token,
            operation_type,
            namespace,
            document_key,
            full_document,
            full_document_before_change,
            update_description,
        })
    }

    /// Extract documents from test_decoding raw data
    fn extract_documents_from_test_decoding(
        &self,
        change: &WalChange,
        _operation_type: OperationType,
    ) -> Result<(Option<Document>, Option<Document>, Option<Document>)> {
        let mut document_key = None;
        let mut full_document = None;
        let full_document_before_change = None; // Not available in test_decoding without REPLICA IDENTITY FULL

        // Parse test_decoding format to extract BSON data
        // Format: "... object_id[documentdb_core.bson]:'BSONHEX...' document[documentdb_core.bson]:'BSONHEX...'"
        let raw = &change.raw_data;

        // Extract object_id BSON
        if let Some(object_id_bson) = self.extract_bson_hex(raw, "object_id") {
            if let Ok(id_doc) = bson::from_slice::<Document>(&object_id_bson) {
                document_key = Some(id_doc);
            }
        }

        // Extract document BSON (for INSERT and UPDATE)
        if let Some(doc_bson) = self.extract_bson_hex(raw, "document") {
            if let Ok(doc) = bson::from_slice::<Document>(&doc_bson) {
                full_document = Some(doc);
            }
        }

        Ok((document_key, full_document, full_document_before_change))
    }

    /// Extract BSON hex data from test_decoding output
    fn extract_bson_hex(&self, raw: &str, field_name: &str) -> Option<Vec<u8>> {
        // Look for pattern: field_name[documentdb_core.bson]:'BSONHEX...'
        let pattern = format!("{}[documentdb_core.bson]:'BSONHEX", field_name);
        let start = raw.find(&pattern)?;
        let after_pattern = start + pattern.len();
        
        // Find the closing quote
        let rest = &raw[after_pattern..];
        let end = rest.find('\'')?;
        
        let hex_str = &rest[..end];
        hex::decode(hex_str).ok()
    }

    /// Extract documents from WAL change columns (legacy wal2json format)
    #[allow(dead_code)]
    fn extract_documents(
        &self,
        change: &WalChange,
        operation_type: OperationType,
    ) -> Result<(Option<Document>, Option<Document>, Option<Document>)> {
        let mut document_key = None;
        let mut full_document = None;
        let mut full_document_before_change = None;

        // Build a map of column name -> value
        let column_map: HashMap<&str, &serde_json::Value> = change
            .columnnames
            .iter()
            .zip(change.columnvalues.iter())
            .map(|(name, value)| (name.as_str(), value))
            .collect();

        // Extract object_id for document key
        if let Some(object_id_val) = column_map.get("object_id") {
            if let Some(bson_doc) = self.parse_bson_column(object_id_val) {
                // The object_id column contains the _id as a BSON document
                document_key = Some(bson_doc);
            }
        }

        // Extract full document
        if let Some(doc_val) = column_map.get("document") {
            if let Some(bson_doc) = self.parse_bson_column(doc_val) {
                full_document = Some(bson_doc);
            }
        }

        // For deletes with replica identity, try to get old values
        if operation_type == OperationType::Delete || operation_type == OperationType::Update {
            if let Some(old_keys) = &change.oldkeys {
                let old_map: HashMap<&str, &serde_json::Value> = old_keys
                    .keynames
                    .iter()
                    .zip(old_keys.keyvalues.iter())
                    .map(|(name, value)| (name.as_str(), value))
                    .collect();

                // Extract old document if available (requires REPLICA IDENTITY FULL)
                if let Some(old_doc_val) = old_map.get("document") {
                    if let Some(bson_doc) = self.parse_bson_column(old_doc_val) {
                        full_document_before_change = Some(bson_doc);
                    }
                }

                // If we don't have document_key yet, try from old keys
                if document_key.is_none() {
                    if let Some(old_id_val) = old_map.get("object_id") {
                        if let Some(bson_doc) = self.parse_bson_column(old_id_val) {
                            document_key = Some(bson_doc);
                        }
                    }
                }
            }
        }

        Ok((document_key, full_document, full_document_before_change))
    }

    /// Parse a BSON column value from wal2json
    fn parse_bson_column(&self, value: &serde_json::Value) -> Option<Document> {
        match value {
            // wal2json may output BSON as hex-encoded bytes
            serde_json::Value::String(s) => {
                // Try to decode as hex BSON
                if let Ok(bytes) = hex::decode(s.trim_start_matches("\\x")) {
                    if let Ok(doc) = bson::from_slice::<Document>(&bytes) {
                        return Some(doc);
                    }
                }
                // Try as JSON
                if let Ok(doc) = serde_json::from_str::<Document>(s) {
                    return Some(doc);
                }
                None
            }
            serde_json::Value::Object(map) => {
                // Already a JSON object, convert to BSON Document
                serde_json::from_value(serde_json::Value::Object(map.clone())).ok()
            }
            _ => None,
        }
    }

    /// Compute update description by comparing old and new documents
    fn compute_update_description(
        &self,
        _change: &WalChange,
        old_doc: &Option<Document>,
        new_doc: &Option<Document>,
    ) -> Option<UpdateDescription> {
        let (old, new) = match (old_doc, new_doc) {
            (Some(old), Some(new)) => (old, new),
            _ => return None,
        };

        let mut updated_fields = Document::new();
        let mut removed_fields = Vec::new();

        // Find updated and new fields
        for (key, new_value) in new.iter() {
            if key == "_id" {
                continue; // Skip _id field
            }
            match old.get(key) {
                Some(old_value) if old_value != new_value => {
                    updated_fields.insert(key, new_value.clone());
                }
                None => {
                    updated_fields.insert(key, new_value.clone());
                }
                _ => {}
            }
        }

        // Find removed fields
        for (key, _) in old.iter() {
            if key == "_id" {
                continue;
            }
            if !new.contains_key(key) {
                removed_fields.push(key.to_string());
            }
        }

        if updated_fields.is_empty() && removed_fields.is_empty() {
            None
        } else {
            Some(UpdateDescription {
                updated_fields,
                removed_fields,
                truncated_arrays: Vec::new(),
            })
        }
    }
}

impl Default for ChangeEventTranslator {
    fn default() -> Self {
        Self::new()
    }
}

impl ChangeEvent {
    /// Convert the change event to a BSON document
    pub fn to_document(&self) -> Document {
        let mut doc = doc! {
            "_id": self.resume_token.to_bson(),
            "operationType": self.operation_type.as_str(),
            "clusterTime": self.resume_token.cluster_time(),
            "wallTime": self.resume_token.wall_time(),
            "ns": {
                "db": &self.namespace.db,
                "coll": &self.namespace.coll,
            },
        };

        if let Some(ref dk) = self.document_key {
            doc.insert("documentKey", dk.clone());
        }

        if let Some(ref fd) = self.full_document {
            doc.insert("fullDocument", fd.clone());
        }

        if let Some(ref fdbc) = self.full_document_before_change {
            doc.insert("fullDocumentBeforeChange", fdbc.clone());
        }

        if let Some(ref ud) = self.update_description {
            doc.insert(
                "updateDescription",
                doc! {
                    "updatedFields": ud.updated_fields.clone(),
                    "removedFields": ud.removed_fields.clone(),
                    "truncatedArrays": &ud.truncated_arrays,
                },
            );
        }

        doc
    }

    /// Convert to RawDocumentBuf for wire protocol
    pub fn to_raw_document(&self) -> Result<RawDocumentBuf> {
        let doc = self.to_document();
        RawDocumentBuf::from_document(&doc)
            .map_err(|e| DocumentDBError::internal_error(format!("Failed to serialize change event: {}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_change_event_to_document() {
        let resume_token = ResumeToken::new(12345, 1);
        let event = ChangeEvent {
            resume_token,
            operation_type: OperationType::Insert,
            namespace: Namespace {
                db: "testdb".to_string(),
                coll: "testcoll".to_string(),
            },
            document_key: Some(doc! { "_id": "test123" }),
            full_document: Some(doc! { "_id": "test123", "name": "test" }),
            full_document_before_change: None,
            update_description: None,
        };

        let doc = event.to_document();
        assert_eq!(doc.get_str("operationType").unwrap(), "insert");
        assert!(doc.get_document("ns").is_ok());
        assert!(doc.get_document("fullDocument").is_ok());
    }
}
