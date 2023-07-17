-- Add migration script here
CREATE TABLE users (
    user INTEGER PRIMARY KEY,
    active_minutes INTEGER NOT NULL
);