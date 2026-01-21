/*-------------------------------------------------------------------------
 * Copyright (c) Microsoft Corporation.  All rights reserved.
 *
 * src/changestream/resume_token.rs
 *
 * Resume token for change streams - allows clients to resume from a
 * specific position in the WAL.
 *
 *-------------------------------------------------------------------------
 */

use bson::{doc, Bson, Document};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{DocumentDBError, Result};

/// Resume token structure for change streams.
/// Encodes the position in the PostgreSQL WAL for resumption.
///
/// Token format (binary):
/// - version: 1 byte
/// - timestamp: 8 bytes (Unix timestamp in millis)
/// - lsn: 8 bytes (PostgreSQL LSN)
/// - collection_id: 8 bytes
/// - flags: 1 byte (reserved)
#[derive(Debug, Clone)]
pub struct ResumeToken {
    /// Token format version
    pub version: u8,
    /// Timestamp when the change occurred
    pub timestamp_millis: u64,
    /// PostgreSQL Log Sequence Number
    pub lsn: u64,
    /// Collection ID (from documentdb_api_catalog.collections)
    pub collection_id: i64,
    /// Reserved flags for future use
    pub flags: u8,
}

impl ResumeToken {
    /// Current token format version
    const VERSION: u8 = 1;

    /// Create a new resume token
    pub fn new(lsn: u64, collection_id: i64) -> Self {
        let timestamp_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        Self {
            version: Self::VERSION,
            timestamp_millis,
            lsn,
            collection_id,
            flags: 0,
        }
    }

    /// Create a resume token from a specific timestamp
    pub fn with_timestamp(lsn: u64, collection_id: i64, timestamp_millis: u64) -> Self {
        Self {
            version: Self::VERSION,
            timestamp_millis,
            lsn,
            collection_id,
            flags: 0,
        }
    }

    /// Encode the token as a hex string (MongoDB format)
    pub fn to_hex_string(&self) -> String {
        let mut bytes = Vec::with_capacity(26);
        bytes.push(self.version);
        bytes.extend_from_slice(&self.timestamp_millis.to_be_bytes());
        bytes.extend_from_slice(&self.lsn.to_be_bytes());
        bytes.extend_from_slice(&self.collection_id.to_be_bytes());
        bytes.push(self.flags);
        hex::encode(bytes)
    }

    /// Decode a resume token from a hex string
    pub fn from_hex_string(hex_str: &str) -> Result<Self> {
        let bytes = hex::decode(hex_str).map_err(|e| {
            DocumentDBError::bad_value(format!("Invalid resume token hex: {}", e))
        })?;

        if bytes.len() < 26 {
            return Err(DocumentDBError::bad_value(
                "Resume token too short".to_string(),
            ));
        }

        let version = bytes[0];
        if version != Self::VERSION {
            return Err(DocumentDBError::bad_value(format!(
                "Unsupported resume token version: {}",
                version
            )));
        }

        let timestamp_millis = u64::from_be_bytes(bytes[1..9].try_into().unwrap());
        let lsn = u64::from_be_bytes(bytes[9..17].try_into().unwrap());
        let collection_id = i64::from_be_bytes(bytes[17..25].try_into().unwrap());
        let flags = bytes[25];

        Ok(Self {
            version,
            timestamp_millis,
            lsn,
            collection_id,
            flags,
        })
    }

    /// Convert to BSON document format (for _id field in change events)
    pub fn to_bson(&self) -> Document {
        doc! {
            "_data": self.to_hex_string()
        }
    }

    /// Parse from BSON document
    pub fn from_bson(doc: &Document) -> Result<Self> {
        let data = doc.get_str("_data").map_err(|_| {
            DocumentDBError::bad_value("Resume token missing _data field".to_string())
        })?;
        Self::from_hex_string(data)
    }

    /// Parse from RawDocumentBuf
    pub fn from_raw_document(doc: &bson::RawDocument) -> Result<Self> {
        let data = doc.get_str("_data").map_err(|_| {
            DocumentDBError::bad_value("Resume token missing _data field".to_string())
        })?;
        Self::from_hex_string(data)
    }

    /// Get the cluster time as a BSON timestamp
    pub fn cluster_time(&self) -> Bson {
        let secs = (self.timestamp_millis / 1000) as u32;
        let inc = (self.timestamp_millis % 1000) as u32;
        Bson::Timestamp(bson::Timestamp { time: secs, increment: inc })
    }

    /// Get wall time as ISO 8601 string
    pub fn wall_time(&self) -> String {
        let secs = self.timestamp_millis / 1000;
        let nanos = ((self.timestamp_millis % 1000) * 1_000_000) as u32;
        if let Some(dt) = chrono::DateTime::from_timestamp(secs as i64, nanos) {
            dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
        } else {
            "1970-01-01T00:00:00.000Z".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resume_token_roundtrip() {
        let token = ResumeToken::new(12345678, 42);
        let hex = token.to_hex_string();
        let decoded = ResumeToken::from_hex_string(&hex).unwrap();

        assert_eq!(token.version, decoded.version);
        assert_eq!(token.lsn, decoded.lsn);
        assert_eq!(token.collection_id, decoded.collection_id);
        assert_eq!(token.flags, decoded.flags);
    }

    #[test]
    fn test_resume_token_bson_roundtrip() {
        let token = ResumeToken::new(12345678, 42);
        let bson_doc = token.to_bson();
        let decoded = ResumeToken::from_bson(&bson_doc).unwrap();

        assert_eq!(token.lsn, decoded.lsn);
        assert_eq!(token.collection_id, decoded.collection_id);
    }
}
