#!/usr/bin/env bash
set -euo pipefail

SERVICE_NAME="poly_5min_bot"
REPO_DIR="${1:-/opt/poly_5min_bot}"
SERVICE_SRC="${REPO_DIR}/deploy/systemd/${SERVICE_NAME}.service"
SERVICE_DST="/etc/systemd/system/${SERVICE_NAME}.service"
ENV_DIR="/etc/poly_5min_bot"
ENV_FILE="${ENV_DIR}/${SERVICE_NAME}.env"

if [[ "${EUID}" -ne 0 ]]; then
  echo "This script must be run as root."
  exit 1
fi

if [[ ! -f "${SERVICE_SRC}" ]]; then
  echo "Service file not found: ${SERVICE_SRC}"
  echo "Pass repo path explicitly, for example:"
  echo "  sudo ./scripts/install_systemd_service.sh /opt/poly_5min_bot"
  exit 1
fi

if ! id -u polybot >/dev/null 2>&1; then
  useradd --system --create-home --shell /usr/sbin/nologin polybot
  echo "Created system user: polybot"
fi

install -m 0644 "${SERVICE_SRC}" "${SERVICE_DST}"
install -d -m 0750 "${ENV_DIR}"

if [[ ! -f "${ENV_FILE}" ]]; then
  cp "${REPO_DIR}/.env.example" "${ENV_FILE}"
  chmod 0600 "${ENV_FILE}"
  echo "Created ${ENV_FILE} from .env.example"
  echo "Edit it before starting the service."
fi

systemctl daemon-reload
systemctl enable "${SERVICE_NAME}"

echo "Installed ${SERVICE_NAME}.service"
echo "Next steps:"
echo "  1) Edit ${ENV_FILE}"
echo "  2) Build binary at ${REPO_DIR}/target/release/poly_5min_bot"
echo "  3) Start:   sudo systemctl start ${SERVICE_NAME}"
echo "  4) Status:  sudo systemctl status ${SERVICE_NAME}"
echo "  5) Logs:    sudo journalctl -u ${SERVICE_NAME} -f"
