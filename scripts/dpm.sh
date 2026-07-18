#!/usr/bin/env bash
# Reviewable declarative migrations for the dedicated `athleto` database.
# The schema authority lives in k8s-cluster's pg-defs; this service never
# performs DDL at process startup.
set -euo pipefail

cmd="${1:-diff}"
[ "$#" -gt 0 ] && shift

service_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cluster_root="${ATHLETO_K8S_CLUSTER:-$HOME/codes/ores/k8s-cluster}"
vendored_schema="$service_dir/../../libs/pg-defs/schema/databases/athleto/schema.sql"
standalone_schema="$cluster_root/remote/libs/pg-defs/schema/databases/athleto/schema.sql"
schema_sql="${ATHLETO_SCHEMA_SQL:-$vendored_schema}"
if [ ! -f "$schema_sql" ] && [ -z "${ATHLETO_SCHEMA_SQL:-}" ]; then
  schema_sql="$standalone_schema"
fi

if [ ! -f "$schema_sql" ]; then
  echo "error: Athlet-O schema contract not found." >&2
  echo "checked: $vendored_schema" >&2
  echo "checked: $standalone_schema" >&2
  echo "initialize k8s-cluster submodules or set ATHLETO_SCHEMA_SQL." >&2
  exit 1
fi

if ! command -v dpm >/dev/null 2>&1; then
  echo "error: dpm not found on PATH." >&2
  echo "install: brew install declarative-migrations/tap/dpm" >&2
  exit 1
fi

if [ -z "${SHADOW_DATABASE_URL:-}" ]; then
  echo "error: SHADOW_DATABASE_URL is required and must be safe for throwaway databases." >&2
  exit 1
fi

target="${TARGET_DATABASE_URL:-${DATABASE_URL:-}}"
case "$cmd" in
  bootstrap)
    exec dpm bootstrap --source "$schema_sql" "$@"
    ;;
  diff | verify | review | apply)
    if [ -z "$target" ]; then
      echo "error: set TARGET_DATABASE_URL or DATABASE_URL." >&2
      exit 1
    fi
    # Keep database credentials out of argv/process listings.
    export TARGET_DATABASE_URL="$target"
    exec dpm "$cmd" --source "$schema_sql" "$@"
    ;;
  *)
    exec dpm "$cmd" "$@"
    ;;
esac
