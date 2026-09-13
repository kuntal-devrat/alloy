#!/usr/bin/env sh
# Alloy Installer for Linux and macOS
# Usage: curl -fsSL https://raw.githubusercontent.com/alloy-runtime/alloy/main/install.sh | sh

set -e

REPO="alloy-runtime/alloy"
INSTALL_DIR="${ALLOY_INSTALL_DIR:-$HOME/.alloy/bin}"

# ANSI Colors
BOLD="\033[1m"
GREEN="\033[32m"
CYAN="\033[36m"
RED="\033[31m"
RESET="\033[0m"

printf "${CYAN}${BOLD}"
cat << 'EOF'
     ___       __   __                 
    /   |     / /  / /____  __  __     
   / /| |    / /  / // __ \/ / / /     
  / ___ |   / /__/ // /_/ / /_/ /      
 /_/  |_|  /_____/____/\____/\__, /       
                            /____/         
EOF
printf "${RESET}\n"
printf "${BOLD}Installing Alloy Systems Runtime...${RESET}\n\n"

# 1. Detect OS
OS="$(uname -s)"
case "$OS" in
    Linux*)     PLATFORM="unknown-linux-gnu" ;;
    Darwin*)    PLATFORM="apple-darwin" ;;
    *)
        printf "${RED}Error: Unsupported operating system: $OS${RESET}\n"
        exit 1
        ;;
esac

# 2. Detect Architecture
ARCH="$(uname -m)"
case "$ARCH" in
    x86_64|amd64) ARCH_NAME="x86_64" ;;
    arm64|aarch64) ARCH_NAME="aarch64" ;;
    *)
        printf "${RED}Error: Unsupported architecture: $ARCH${RESET}\n"
        exit 1
        ;;
esac

TARGET="${ARCH_NAME}-${PLATFORM}"
ARCHIVE_NAME="alloy-${TARGET}.tar.gz"
DOWNLOAD_URL="https://github.com/${REPO}/releases/latest/download/${ARCHIVE_NAME}"

printf "  Target:       ${BOLD}${TARGET}${RESET}\n"
printf "  Destination:  ${BOLD}${INSTALL_DIR}/alloy${RESET}\n\n"

# 3. Download Archive
TMP_DIR="$(mktemp -d 2>/dev/null || mktemp -d -t 'alloy-install')"
trap 'rm -rf "$TMP_DIR"' EXIT INT TERM

printf "Downloading ${CYAN}${ARCHIVE_NAME}${RESET}...\n"
if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$DOWNLOAD_URL" -o "$TMP_DIR/$ARCHIVE_NAME" || {
        printf "${RED}Failed to download binary from $DOWNLOAD_URL${RESET}\n"
        exit 1
    }
elif command -v wget >/dev/null 2>&1; then
    wget -qO "$TMP_DIR/$ARCHIVE_NAME" "$DOWNLOAD_URL" || {
        printf "${RED}Failed to download binary from $DOWNLOAD_URL${RESET}\n"
        exit 1
    }
else
    printf "${RED}Error: curl or wget is required to install Alloy.${RESET}\n"
    exit 1
fi

# 4. Extract and Install
mkdir -p "$INSTALL_DIR"
tar -xzf "$TMP_DIR/$ARCHIVE_NAME" -C "$TMP_DIR"

if [ -f "$TMP_DIR/alloy" ]; then
    cp "$TMP_DIR/alloy" "$INSTALL_DIR/alloy"
elif [ -f "$TMP_DIR/target/${TARGET}/release/alloy" ]; then
    cp "$TMP_DIR/target/${TARGET}/release/alloy" "$INSTALL_DIR/alloy"
else
    # Fallback search for alloy binary in extracted files
    FOUND_BIN="$(find "$TMP_DIR" -type f -name alloy -perm -111 | head -n 1)"
    if [ -n "$FOUND_BIN" ]; then
        cp "$FOUND_BIN" "$INSTALL_DIR/alloy"
    else
        printf "${RED}Error: alloy binary not found inside archive.${RESET}\n"
        exit 1
    fi
fi

chmod +x "$INSTALL_DIR/alloy"

printf "\n${GREEN}${BOLD}Alloy was installed successfully!${RESET}\n\n"

# 5. Check PATH and advise configuration
case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        printf "To add Alloy to your current shell PATH, run:\n\n"
        printf "  ${BOLD}export PATH=\"\$HOME/.alloy/bin:\$PATH\"${RESET}\n\n"
        printf "To persist it, append it to your shell configuration file:\n"
        if [ -n "$ZSH_VERSION" ] || [ -f "$HOME/.zshrc" ]; then
            printf "  ${CYAN}echo 'export PATH=\"\$HOME/.alloy/bin:\$PATH\"' >> ~/.zshrc${RESET}\n"
        else
            printf "  ${CYAN}echo 'export PATH=\"\$HOME/.alloy/bin:\$PATH\"' >> ~/.bashrc${RESET}\n"
        fi
        printf "\n"
        ;;
esac

printf "Verify installation with:\n"
printf "  ${BOLD}alloy --version${RESET}\n\n"
