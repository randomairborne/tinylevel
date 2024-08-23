-- Add migration script here
CREATE TABLE users
(
    id             INTEGER PRIMARY KEY,
    active_minutes INTEGER NOT NULL,
    last_message   INTEGER NOT NULL
);