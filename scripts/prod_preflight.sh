#!/usr/bin/env bash
set -euo pipefail

ENV_FILE="${1:-.env}"

fail() {
  echo "ERROR: $*"
  exit 1
}

check_cmd() {
  local cmd="$1"
  command -v "${cmd}" >/dev/null 2>&1 || fail "Missing required command: ${cmd}"
}

require_non_empty_var() {
  local name="$1"
  local line
  line="$(grep -E "^${name}=" "${ENV_FILE}" | tail -n 1 || true)"
  [[ -n "${line}" ]] || fail "Missing variable in ${ENV_FILE}: ${name}"
  local value="${line#*=}"
  [[ -n "${value}" ]] || fail "Empty variable in ${ENV_FILE}: ${name}"
}

echo "Running production preflight checks..."

[[ -f "${ENV_FILE}" ]] || fail "Env file not found: ${ENV_FILE}"

check_cmd rustc
check_cmd cargo
check_cmd cmake
check_cmd nasm

require_non_empty_var POLYMARKET_PRIVATE_KEY

unsafe_flag="$(grep -E '^MERGE_ALLOW_UNSAFE_ENDPOINTS=' "${ENV_FILE}" | tail -n 1 | cut -d'=' -f2- || true)"
if [[ "${unsafe_flag,,}" == "1" || "${unsafe_flag,,}" == "true" || "${unsafe_flag,,}" == "yes" ]]; then
  fail "MERGE_ALLOW_UNSAFE_ENDPOINTS is enabled in ${ENV_FILE}; disable it for production"
fi

relayer_url="$(grep -E '^RELAYER_URL=' "${ENV_FILE}" | tail -n 1 | cut -d'=' -f2- || true)"
if [[ -n "${relayer_url}" && ! "${relayer_url}" =~ ^https:// ]]; then
  fail "RELAYER_URL must use https://"
fi

echo "Preflight checks passed."
