#!/usr/bin/env bash

set -euo pipefail

source "./scripts/tests/lib.sh"

client_curl_resource

docker compose restart api # Restart portal

sleep 5 # Wait for client to reconnect

client_curl_resource