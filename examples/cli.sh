#!/bin/sh
set -eu

actual=$(
  printf '%s\n' \
    'CREATE TABLE readings (id Int64, value Nullable(Float64));' \
    'INSERT INTO readings VALUES (1, NULL), (2, 2.5);' \
    'SELECT COUNT(*) AS reading_count FROM readings;' \
  | cargo run --locked --quiet -- --format csv
)
expected='reading_count
2'

if [ "$actual" != "$expected" ]; then
  printf 'unexpected CLI output:\n%s\n' "$actual" >&2
  exit 1
fi

printf '%s\n' "$actual"
