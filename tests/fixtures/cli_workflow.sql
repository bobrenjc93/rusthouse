CREATE TABLE events (id Int64, label String);
INSERT INTO events VALUES (1, 'first'), (2, 'semi;colon'), (3, 'it''s UTF-8: 東京');
SELECT * FROM events;
