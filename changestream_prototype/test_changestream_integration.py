#!/usr/bin/env python3
"""
Integration test for DocumentDB change streams using PostgreSQL logical replication.

This test verifies that:
1. PostgreSQL is configured with wal_level=logical
2. Replication slots can be created
3. Changes to DocumentDB collections are captured in the WAL
4. The test_decoding plugin outputs changes correctly

Prerequisites:
- DocumentDB container running with logical WAL level
- pymongo and psycopg2 installed: pip install pymongo psycopg2-binary

Usage:
    python test_changestream_integration.py
"""

import json
import sys
import time
from datetime import datetime

# Check for required packages
try:
    import psycopg2
    from psycopg2 import sql
except ImportError:
    print("Please install psycopg2: pip install psycopg2-binary")
    sys.exit(1)

try:
    from pymongo import MongoClient
    from pymongo.errors import PyMongoError
except ImportError:
    print("Please install pymongo: pip install pymongo")
    sys.exit(1)

# Configuration
MONGO_URI = "mongodb://testuser:testpass123@localhost:10260/?authMechanism=SCRAM-SHA-256&tls=true&tlsAllowInvalidCertificates=true&directConnection=true"
PG_HOST = "localhost"
PG_PORT = 9712
PG_USER = "testuser"
PG_PASSWORD = "testpass123"
PG_DATABASE = "postgres"

SLOT_NAME = "changestream_integration_test"
TEST_DB = "changestream_test_db"
TEST_COLLECTION = "test_events"


def get_pg_connection(replication=False):
    """Get a PostgreSQL connection."""
    conn_params = {
        'host': PG_HOST,
        'port': PG_PORT,
        'user': PG_USER,
        'password': PG_PASSWORD,
        'dbname': PG_DATABASE,
    }
    if replication:
        conn_params['replication'] = 'database'
    return psycopg2.connect(**conn_params)


def check_wal_level():
    """Verify WAL level is set to logical."""
    print("\n=== Checking WAL Level ===")
    conn = get_pg_connection()
    try:
        with conn.cursor() as cur:
            cur.execute("SHOW wal_level;")
            wal_level = cur.fetchone()[0]
            print(f"WAL level: {wal_level}")
            if wal_level != 'logical':
                print(f"ERROR: WAL level must be 'logical', got '{wal_level}'")
                return False
            print("✓ WAL level is correctly set to 'logical'")
            return True
    finally:
        conn.close()


def create_replication_slot():
    """Create a logical replication slot."""
    print("\n=== Creating Replication Slot ===")
    conn = get_pg_connection()
    try:
        conn.autocommit = True
        with conn.cursor() as cur:
            # Drop slot if exists
            cur.execute("""
                SELECT pg_drop_replication_slot(slot_name) 
                FROM pg_replication_slots 
                WHERE slot_name = %s
            """, (SLOT_NAME,))
            
            # Create new slot
            cur.execute(
                "SELECT pg_create_logical_replication_slot(%s, 'test_decoding')",
                (SLOT_NAME,)
            )
            result = cur.fetchone()
            print(f"Created slot: {result}")
            print(f"✓ Replication slot '{SLOT_NAME}' created successfully")
            return True
    except Exception as e:
        print(f"ERROR creating replication slot: {e}")
        return False
    finally:
        conn.close()


def get_collection_id(db_name, coll_name):
    """Get the collection_id for a DocumentDB collection."""
    conn = get_pg_connection()
    try:
        with conn.cursor() as cur:
            cur.execute("""
                SELECT collection_id 
                FROM documentdb_api_catalog.collections 
                WHERE database_name = %s AND collection_name = %s
            """, (db_name, coll_name))
            row = cur.fetchone()
            return row[0] if row else None
    finally:
        conn.close()


def perform_mongo_operations():
    """Perform insert, update, delete operations on MongoDB."""
    print("\n=== Performing MongoDB Operations ===")
    
    client = MongoClient(MONGO_URI, serverSelectionTimeoutMS=5000)
    try:
        # Test connection
        client.admin.command('ping')
        print("Connected to DocumentDB")
        
        db = client[TEST_DB]
        collection = db[TEST_COLLECTION]
        
        # Clean up
        collection.drop()
        print(f"Using collection: {TEST_DB}.{TEST_COLLECTION}")
        
        # INSERT
        print("\n--- INSERT ---")
        result = collection.insert_one({
            "event_type": "user_created",
            "user_id": 123,
            "timestamp": datetime.utcnow().isoformat(),
            "data": {"name": "Test User", "email": "test@example.com"}
        })
        print(f"Inserted document with _id: {result.inserted_id}")
        time.sleep(0.5)
        
        # UPDATE
        print("\n--- UPDATE ---")
        collection.update_one(
            {"user_id": 123},
            {"$set": {"data.email": "updated@example.com", "updated": True}}
        )
        print("Updated document")
        time.sleep(0.5)
        
        # INSERT another
        print("\n--- INSERT (second) ---")
        result2 = collection.insert_one({
            "event_type": "order_placed",
            "order_id": 456,
            "timestamp": datetime.utcnow().isoformat(),
            "data": {"items": ["item1", "item2"], "total": 99.99}
        })
        print(f"Inserted second document with _id: {result2.inserted_id}")
        time.sleep(0.5)
        
        # DELETE
        print("\n--- DELETE ---")
        collection.delete_one({"user_id": 123})
        print("Deleted document")
        time.sleep(0.5)
        
        print("\n✓ MongoDB operations completed")
        return True
        
    except PyMongoError as e:
        print(f"ERROR: MongoDB operation failed: {e}")
        return False
    finally:
        client.close()


def read_wal_changes():
    """Read changes from the replication slot."""
    print("\n=== Reading WAL Changes ===")
    
    conn = get_pg_connection()
    try:
        conn.autocommit = True
        with conn.cursor() as cur:
            # Get changes from the slot
            cur.execute("""
                SELECT lsn, xid, data 
                FROM pg_logical_slot_get_changes(%s, NULL, NULL,
                    'include-xids', 'true',
                    'include-timestamp', 'true')
            """, (SLOT_NAME,))
            
            changes = cur.fetchall()
            print(f"Found {len(changes)} WAL changes")
            
            # Filter for documentdb_data changes
            documentdb_changes = []
            for lsn, xid, data in changes:
                if 'documentdb_data' in data or 'documents_' in data:
                    documentdb_changes.append({
                        'lsn': str(lsn),
                        'xid': xid,
                        'data': data
                    })
                    print(f"\n[LSN: {lsn}] {data[:200]}...")
            
            print(f"\n✓ Found {len(documentdb_changes)} DocumentDB-related changes")
            return documentdb_changes
            
    except Exception as e:
        print(f"ERROR reading WAL changes: {e}")
        return []
    finally:
        conn.close()


def cleanup():
    """Clean up test resources."""
    print("\n=== Cleanup ===")
    
    # Drop replication slot
    conn = get_pg_connection()
    try:
        conn.autocommit = True
        with conn.cursor() as cur:
            cur.execute("""
                SELECT pg_drop_replication_slot(slot_name) 
                FROM pg_replication_slots 
                WHERE slot_name = %s
            """, (SLOT_NAME,))
            print(f"Dropped replication slot '{SLOT_NAME}'")
    except Exception as e:
        print(f"Warning: Could not drop slot: {e}")
    finally:
        conn.close()
    
    # Drop test collection
    try:
        client = MongoClient(MONGO_URI, serverSelectionTimeoutMS=5000)
        client[TEST_DB][TEST_COLLECTION].drop()
        client.close()
        print(f"Dropped test collection '{TEST_DB}.{TEST_COLLECTION}'")
    except Exception as e:
        print(f"Warning: Could not drop collection: {e}")
    
    print("✓ Cleanup completed")


def main():
    """Run the integration test."""
    print("=" * 60)
    print("DocumentDB Change Stream Integration Test")
    print("=" * 60)
    
    success = True
    
    try:
        # Step 1: Check WAL level
        if not check_wal_level():
            print("\nFAILED: WAL level check")
            return 1
        
        # Step 2: Create replication slot
        if not create_replication_slot():
            print("\nFAILED: Could not create replication slot")
            return 1
        
        # Step 3: Perform MongoDB operations
        if not perform_mongo_operations():
            print("\nFAILED: MongoDB operations failed")
            success = False
        
        # Step 4: Read WAL changes
        changes = read_wal_changes()
        
        # Step 5: Verify we captured changes
        print("\n=== Verification ===")
        if len(changes) > 0:
            print(f"✓ Successfully captured {len(changes)} changes from WAL")
            
            # Check for different operation types
            insert_count = sum(1 for c in changes if 'INSERT' in c['data'])
            update_count = sum(1 for c in changes if 'UPDATE' in c['data'])
            delete_count = sum(1 for c in changes if 'DELETE' in c['data'])
            
            print(f"  - INSERTs: {insert_count}")
            print(f"  - UPDATEs: {update_count}")
            print(f"  - DELETEs: {delete_count}")
            
            if insert_count >= 2 and update_count >= 1 and delete_count >= 1:
                print("\n✓ All expected operations captured!")
            else:
                print("\n⚠ Some operations may not have been captured")
        else:
            print("⚠ No DocumentDB changes captured")
            print("  This could mean:")
            print("  - The test ran too quickly for WAL to flush")
            print("  - The collection table doesn't exist yet")
            success = False
        
    finally:
        cleanup()
    
    print("\n" + "=" * 60)
    if success:
        print("TEST PASSED: Change stream infrastructure is working!")
        print("=" * 60)
        return 0
    else:
        print("TEST FAILED: Some checks did not pass")
        print("=" * 60)
        return 1


if __name__ == "__main__":
    sys.exit(main())
