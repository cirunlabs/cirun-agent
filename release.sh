#!/bin/bash
set -e

# Build and run to get the version
echo "Building and retrieving version..."
VERSION=$(cargo run -- --version | grep -o "cirun-agent [0-9]\+\.[0-9]\+\.[0-9]\+" | cut -d ' ' -f2)

if [ -z "$VERSION" ]; then
  echo "Error: Could not extract version number. Exiting."
  exit 1
fi

echo "=========================="
echo "Detected version: $VERSION"
echo "=========================="
echo "Changes to be committed:"
git status --short

echo ""
echo "Proceed with release v$VERSION? (y/n)"
read -r CONFIRM

if [ "$CONFIRM" = "y" ]; then
  echo "Creating release commit..."
  git commit -am "release: $VERSION"

  echo "Creating tag v$VERSION..."
  git tag "v$VERSION"

  echo "Pushing to remote..."
  git push

  echo "Pushing tags to remote..."
  git push --tags

  echo "✅ Release v$VERSION completed successfully!"
else
  echo "⚠️ Release canceled"
fi
