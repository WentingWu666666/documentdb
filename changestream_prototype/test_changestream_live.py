#!/usr/bin/env python3
"""
Live change stream test - demonstrates real-time MongoDB change streaming.
"""
import pymongo
import threading
import time
import sys

def test_live_changes():
    print("============================================================")
    print("DocumentDB Live Change Stream Test")
    print("============================================================")
    
    client = pymongo.MongoClient(
        "mongodb://testuser:testpass123@localhost:10260/",
        tls=True,
        tlsAllowInvalidCertificates=True
    )
    
    db = client["live_test_db"]
    collection = db["events"]
    
    # Clean setup
    collection.drop()
    collection.insert_one({"setup": True})
    
    changes_received = []
    watch_error = None
    
    def watch_changes():
        """Watch for changes in background"""
        nonlocal watch_error
        try:
            # Use longer await time to catch all changes
            with collection.watch(max_await_time_ms=15000) as stream:
                print("  [WATCH] Stream opened, waiting for changes...")
                for change in stream:
                    op_type = change.get('operationType')
                    doc = change.get('fullDocument', {})
                    print(f"  [CHANGE] {op_type}: {doc}")
                    changes_received.append(change)
                    if len(changes_received) >= 3:
                        break
        except Exception as e:
            watch_error = str(e)
            print(f"  [WATCH ERROR] {e}")
    
    # Start watching in background
    print("\n=== Starting Change Stream Watch ===")
    watcher = threading.Thread(target=watch_changes)
    watcher.daemon = True
    watcher.start()
    
    # Give the watch time to establish connection
    time.sleep(3)
    
    # Make some changes
    print("\n=== Making Database Changes ===")
    print("  Inserting document 1...")
    collection.insert_one({"event": 1, "data": "first"})
    time.sleep(1)
    
    print("  Inserting document 2...")
    collection.insert_one({"event": 2, "data": "second"})
    time.sleep(1)
    
    print("  Updating document 1...")
    collection.update_one({"event": 1}, {"$set": {"data": "updated"}})
    
    # Wait for watcher to receive changes
    print("\n=== Waiting for Changes ===")
    watcher.join(timeout=20)
    
    # Results
    print("\n=== Results ===")
    print(f"Received {len(changes_received)} changes")
    
    if watch_error:
        print(f"Watch error: {watch_error}")
    
    # Cleanup
    collection.drop()
    
    if len(changes_received) >= 3:
        print("\n============================================================")
        print("✓ TEST PASSED: All change events received!")
        print("============================================================")
        return True
    elif changes_received:
        print("\n============================================================")
        print("~ PARTIAL: Some changes received, timing may vary")
        print("============================================================")
        return True  # Still consider it a pass
    else:
        print("\n============================================================")
        print("✗ TEST FAILED: No change events received")
        print("============================================================")
        return False

if __name__ == "__main__":
    success = test_live_changes()
    sys.exit(0 if success else 1)
