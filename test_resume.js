// Test change stream with resumeAfter
print("Test: Change stream with resumeAfter");
db = db.getSiblingDB("cstest");
db.resumetest.drop();
db.resumetest.insertOne({name: "init"});

// Create change stream
var cursor1 = db.resumetest.watch();
print("Cursor1 created");

// Insert doc1
db.resumetest.insertOne({name: "doc1", seq: 1});
print("Inserted doc1");

// Get the event
var event1 = cursor1.tryNext();
if (event1) {
  print("Got event1: " + event1.operationType + " for " + JSON.stringify(event1.fullDocument));
  var token1 = event1._id;
  print("Token1: " + JSON.stringify(token1));
  
  // Close cursor1
  cursor1.close();
  print("Closed cursor1");
  
  // Insert doc2 and doc3 while cursor is closed
  db.resumetest.insertOne({name: "doc2", seq: 2});
  db.resumetest.insertOne({name: "doc3", seq: 3});
  print("Inserted doc2 and doc3 while cursor was closed");
  
  // Create cursor2 with resumeAfter
  print("Creating cursor2 with resumeAfter token1...");
  var cursor2 = db.resumetest.watch([], {resumeAfter: token1});
  
  // Try to get doc2 and doc3
  var event2 = cursor2.tryNext();
  var event3 = cursor2.tryNext();
  
  print("event2: " + (event2 ? event2.operationType + " for " + JSON.stringify(event2.fullDocument) : "null"));
  print("event3: " + (event3 ? event3.operationType + " for " + JSON.stringify(event3.fullDocument) : "null"));
  
  cursor2.close();
  
  if (event2 && event2.fullDocument && event2.fullDocument.name === "doc2") {
    print("SUCCESS: resumeAfter correctly replayed doc2!");
  } else {
    print("FAIL: resumeAfter did NOT replay doc2");
  }
} else {
  print("FAIL: Did not get event1");
}
