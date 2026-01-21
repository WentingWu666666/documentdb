/*-------------------------------------------------------------------------
 * Copyright (c) Microsoft Corporation.  All rights reserved.
 *
 * src/changestream/mod.rs
 *
 * Change stream support for DocumentDB using PostgreSQL logical replication
 * with wal2json plugin.
 *
 *-------------------------------------------------------------------------
 */

pub mod cursor;
pub mod handler;
pub mod resume_token;
pub mod translator;
pub mod wal_reader;

pub use cursor::{ChangeStreamCursor, ChangeStreamCursorStore};
pub use handler::process_change_stream;
pub use resume_token::ResumeToken;
pub use translator::ChangeEventTranslator;
pub use wal_reader::{WalChange, WalReader};
