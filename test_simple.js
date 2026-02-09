db = db.getSiblingDB("testdb");
db.testcoll.insertOne({test: 1});
print("Insert OK");
