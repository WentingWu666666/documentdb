# Change Stream Support for DocumentDB using wal2json

## Overview

This document proposes adding MongoDB-compatible change stream support to DocumentDB by leveraging PostgreSQL's logical replication with the wal2json plugin.

**Status: Prototype Implemented** - Gateway integration code is in `pg_documentdb_gw/src/changestream/`

## Problem Statement

DocumentDB currently returns `CommandNotSupported` (code 115) when clients attempt to use MongoDB's `$changeStream` aggregation stage. Change streams are a critical feature for:
- Real-time event-driven applications
- Data synchronization between systems
- Audit logging
- Cache invalidation

## Background

### DocumentDB Storage Model

DocumentDB stores MongoDB documents in PostgreSQL tables:

```
documentdb_data.documents_<collection_id>
├── shard_key_value  (bigint)     - Hash of shard key
├── object_id        (bson)       - MongoDB _id field
├── document         (bson)       - Full BSON document
└── creation_time    (timestamptz) - Optional timestamp
```

Collection metadata is stored in:
```
documentdb_api_catalog.collections
├── database_name    (text)
├── collection_name  (text)
├── collection_id    (bigint)     - Maps to table suffix
├── shard_key        (bson)
└── collection_uuid  (uuid)
```

### PostgreSQL Logical Replication

PostgreSQL supports logical decoding of the Write-Ahead Log (WAL), allowing applications to stream changes in real-time. The `wal2json` plugin outputs changes as JSON, making it easy to parse.

## Proposed Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        Client Application                        │
│                  (MongoDB Driver / mongosh)                      │
└─────────────────────────────────────────────────────────────────┘
                                │
                                │ MongoDB Wire Protocol
                                ▼
┌─────────────────────────────────────────────────────────────────┐
│                    DocumentDB Gateway                            │
│                                                                  │
│  ┌─────────────────────────────────────────────────────────┐   │
│  │              Change Stream Handler (NEW)                 │   │
│  │                                                          │   │
│  │  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  │   │
│  │  │ WAL Reader   │──│  Translator  │──│ Event Queue  │  │   │
│  │  └──────────────┘  └──────────────┘  └──────────────┘  │   │
│  └─────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────┘
                                │
                                │ Logical Replication Protocol
                                ▼
┌─────────────────────────────────────────────────────────────────┐
│                     PostgreSQL + wal2json                        │
│                                                                  │
│    WAL ──► Logical Decoding ──► wal2json ──► JSON Stream        │
└─────────────────────────────────────────────────────────────────┘
```

## Component Design

### 1. WAL Reader

**Responsibility**: Connect to PostgreSQL logical replication and consume wal2json output.

**Key Operations**:
- Create/manage replication slot
- Maintain streaming replication connection
- Parse wal2json JSON output
- Track LSN (Log Sequence Number) for resume tokens

**PostgreSQL Setup Required**:
```sql
-- Enable logical replication in postgresql.conf
-- wal_level = logical
-- max_replication_slots = 10

-- Create replication slot
SELECT pg_create_logical_replication_slot('documentdb_changestream', 'wal2json');
```

**wal2json Output Format**:
```json
{
  "change": [
    {
      "kind": "insert",
      "schema": "documentdb_data",
      "table": "documents_12345",
      "columnnames": ["shard_key_value", "object_id", "document"],
      "columntypes": ["bigint", "bson", "bson"],
      "columnvalues": [123456789, "<bson_hex>", "<bson_hex>"]
    }
  ]
}
```

### 2. Change Translator

**Responsibility**: Convert wal2json events to MongoDB change stream format.

**Mapping Logic**:

| PostgreSQL Operation | MongoDB operationType |
|---------------------|----------------------|
| INSERT              | insert               |
| UPDATE              | update / replace     |
| DELETE              | delete               |

**Collection Resolution**:
- Extract `collection_id` from table name (`documents_<id>`)
- Query `documentdb_api_catalog.collections` to get database/collection names
- Cache mappings for performance

**BSON Handling**:
- Parse `object_id` column to extract `_id` field
- Parse `document` column for `fullDocument`
- For updates, compute `updateDescription` if possible

### 3. Event Queue

**Responsibility**: Buffer and deliver events to subscribed clients.

**Features**:
- Per-collection subscription management
- Resume token tracking (based on PostgreSQL LSN)
- Filtering support (match expressions)
- Backpressure handling

### 4. MongoDB Change Event Format

Output events must conform to MongoDB's change stream format:

```json
{
  "_id": {
    "_data": "826F6FEFEA000000012B022C0100296E5A10..."
  },
  "operationType": "insert",
  "clusterTime": {"$timestamp": {"t": 1706824556, "i": 1}},
  "wallTime": {"$date": "2024-02-01T20:15:56.123Z"},
  "fullDocument": {
    "_id": {"$oid": "6f6fefea78a496f67a6681d1"},
    "name": "test",
    "value": 42
  },
  "ns": {
    "db": "mydb",
    "coll": "mycollection"
  },
  "documentKey": {
    "_id": {"$oid": "6f6fefea78a496f67a6681d1"}
  }
}
```

## Resume Token Design

Resume tokens allow clients to resume a change stream after disconnection.

**Token Structure** (encoded as hex string):
```
<version><timestamp><lsn><collection_id><flags>
```

| Field         | Size    | Description                          |
|---------------|---------|--------------------------------------|
| version       | 1 byte  | Token format version                 |
| timestamp     | 8 bytes | Unix timestamp (seconds)             |
| lsn           | 8 bytes | PostgreSQL LSN                       |
| collection_id | 8 bytes | Collection identifier                |
| flags         | 1 byte  | Reserved for future use              |

**Resume Behavior**:
1. Parse resume token to extract LSN
2. Start replication from that LSN position
3. Filter events to requested collection(s)
4. Skip events already delivered (using timestamp)

## API Compatibility

### Supported Operations

| MongoDB Feature                    | Support Level |
|------------------------------------|---------------|
| `collection.watch()`               | Full          |
| `db.watch()`                       | Full          |
| `client.watch()`                   | Full          |
| `fullDocument: "updateLookup"`     | Full          |
| `fullDocumentBeforeChange`         | Partial*      |
| Pipeline filtering (`$match`)      | Full          |
| Resume tokens                      | Full          |
| `startAtOperationTime`             | Full          |
| `startAfter` / `resumeAfter`       | Full          |

*Requires `REPLICA IDENTITY FULL` on tables

### Example Usage

```python
# Python (pymongo)
with collection.watch() as stream:
    for change in stream:
        print(change)

# With pipeline filter
pipeline = [{'$match': {'operationType': 'insert'}}]
with collection.watch(pipeline) as stream:
    for change in stream:
        print(change)

# Resume after disconnect
with collection.watch(resume_after=last_token) as stream:
    for change in stream:
        print(change)
```

## Implementation Options

### Option A: Gateway Integration (Recommended)

Implement change stream handling directly in the DocumentDB Gateway (Rust).

**Pros**:
- Native MongoDB wire protocol support
- Best performance
- Seamless client experience

**Cons**:
- More complex implementation
- Requires Rust wal2json/replication client

### Option B: Sidecar Service

Implement as a separate service that proxies change stream requests.

**Pros**:
- Simpler, isolated implementation
- Can use Python/Node.js for rapid development
- Easier to test and iterate

**Cons**:
- Additional deployment component
- Extra network hop
- Protocol translation overhead

### Option C: PostgreSQL Extension

Implement as a PostgreSQL extension that exposes change streams via SQL functions.

**Pros**:
- Close to data
- Can leverage existing BSON infrastructure

**Cons**:
- Still needs gateway integration
- Complex extension development

## PostgreSQL Configuration

Required `postgresql.conf` changes:
```ini
wal_level = logical
max_replication_slots = 10
max_wal_senders = 10
```

Required extensions:
```sql
CREATE EXTENSION IF NOT EXISTS wal2json;
```

Table configuration for full document before change:
```sql
ALTER TABLE documentdb_data.documents_<id> REPLICA IDENTITY FULL;
```

## Performance Considerations

1. **Replication Slot Management**: Unused slots prevent WAL cleanup; implement slot cleanup for inactive streams

2. **Filtering**: Apply filters early (in SQL or immediately after parsing) to reduce processing

3. **Batching**: Batch multiple changes into single responses when possible

4. **Connection Pooling**: Reuse replication connections across multiple watchers on same collection

5. **Caching**: Cache collection_id → (db, collection) mappings

## Security Considerations

1. **Replication Privileges**: Change stream service needs `REPLICATION` privilege

2. **Row-Level Security**: Ensure change streams respect DocumentDB's access controls

3. **Sensitive Data**: Consider filtering sensitive fields from change events

## Testing Strategy

1. **Unit Tests**: Mock wal2json output, verify translation logic

2. **Integration Tests**: 
   - Insert/update/delete operations trigger correct events
   - Resume tokens work correctly
   - Filtering works as expected

3. **Compatibility Tests**: Run MongoDB driver test suites for change streams

4. **Performance Tests**: Measure latency and throughput under load

## Open Questions

1. **Cluster Time**: How to generate MongoDB-compatible cluster timestamps from PostgreSQL?

2. **Transactions**: How to handle multi-document transactions in change events?

3. **Sharding**: How does this work with Citus-distributed DocumentDB?

4. **Pre-images**: Is `REPLICA IDENTITY FULL` acceptable for all use cases?

## Next Steps

1. [ ] Review and approve design
2. [ ] Choose implementation option (A, B, or C)
3. [ ] Create proof-of-concept prototype
4. [ ] Implement MVP with basic insert/update/delete support
5. [ ] Add resume token support
6. [ ] Add pipeline filtering
7. [ ] Integration testing
8. [ ] Performance optimization
9. [ ] Documentation

## References

- [MongoDB Change Streams](https://www.mongodb.com/docs/manual/changeStreams/)
- [PostgreSQL Logical Decoding](https://www.postgresql.org/docs/current/logicaldecoding.html)
- [wal2json](https://github.com/eulerto/wal2json)
- [DocumentDB Architecture](./docs/)
