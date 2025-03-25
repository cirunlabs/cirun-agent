#!/usr/bin/env bash

set -euo pipefail

REPO="cirunlabs/cirun-agent"
ASSET_NAME="cirun-agent-installer.sh"

echo "Fetching latest release from $REPO..."

DOWNLOAD_URL=$(curl -s "https://api.github.com/repos/$REPO/releases/latest" \
  | grep "browser_download_url" \
  | grep "$ASSET_NAME" \
  | cut -d '"' -f 4)

if [ -z "$DOWNLOAD_URL" ]; then
  echo "Error: Could not find $ASSET_NAME in the latest release assets."
  exit 1
fi

echo "Downloading $ASSET_NAME from $DOWNLOAD_URL..."
curl --proto '=https' --tlsv1.2 -LsSf "$DOWNLOAD_URL" | sh
