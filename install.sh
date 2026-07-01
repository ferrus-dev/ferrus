#!/bin/sh
set -eu

REPO="${FERRUS_INSTALL_REPO:-RomanEmreis/ferrus}"
VERSION="${FERRUS_INSTALL_VERSION:-latest}"
TARGET=""
ARCHIVE=""
CHECKSUM_FILE=""
RELEASE_URL=""
CHECKSUM_URL=""
INSTALL_DIR=""
TMP_DIR=""
HAS_CHECKSUM=0

need_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "error: required command not found: $1" >&2
        exit 1
    fi
}

detect_platform() {
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Linux)
            case "$arch" in
                x86_64|amd64)
                    TARGET="x86_64-unknown-linux-gnu"
                    ;;
                aarch64|arm64)
                    TARGET="aarch64-unknown-linux-gnu"
                    ;;
                *)
                    echo "error: unsupported Linux architecture: ${arch}" >&2
                    echo "supported Linux targets: x86_64, aarch64" >&2
                    exit 1
                    ;;
            esac
            ;;
        Darwin)
            case "$arch" in
                x86_64|amd64)
                    TARGET="x86_64-apple-darwin"
                    ;;
                aarch64|arm64)
                    TARGET="aarch64-apple-darwin"
                    ;;
                *)
                    echo "error: unsupported macOS architecture: ${arch}" >&2
                    echo "supported macOS targets: x86_64, arm64" >&2
                    exit 1
                    ;;
            esac
            ;;
        *)
            echo "error: unsupported platform: ${os}" >&2
            echo "supported platforms: Linux, macOS" >&2
            exit 1
            ;;
    esac

    ARCHIVE="ferrus-${TARGET}.tar.gz"
    CHECKSUM_FILE="${ARCHIVE}.sha256"
}

resolve_url() {
    case "$VERSION" in
        latest)
            RELEASE_URL="https://github.com/${REPO}/releases/latest/download/${ARCHIVE}"
            CHECKSUM_URL="https://github.com/${REPO}/releases/latest/download/${CHECKSUM_FILE}"
            ;;
        *)
            RELEASE_URL="https://github.com/${REPO}/releases/download/${VERSION}/${ARCHIVE}"
            CHECKSUM_URL="https://github.com/${REPO}/releases/download/${VERSION}/${CHECKSUM_FILE}"
            ;;
    esac
}

pick_install_dir() {
    if [ -n "${FERRUS_INSTALL_DIR:-}" ]; then
        INSTALL_DIR="$FERRUS_INSTALL_DIR"
        return
    fi

    if [ -n "${XDG_BIN_HOME:-}" ]; then
        INSTALL_DIR="$XDG_BIN_HOME"
        return
    fi

    if [ -z "${HOME:-}" ]; then
        echo "error: HOME is not set" >&2
        exit 1
    fi

    INSTALL_DIR="${HOME}/.local/bin"
}

download_file() {
    url="$1"
    output="$2"
    label="$3"
    asset_name="$4"
    allow_missing="${5:-false}"

    if command -v curl >/dev/null 2>&1; then
        http_code="$(curl -sSL -o "$output" -w '%{http_code}' "$url")" || {
            echo "error: failed to download ${label} from ${url}" >&2
            exit 1
        }

        if [ "$http_code" = "200" ]; then
            return
        fi

        if [ "$http_code" = "404" ] && [ "$allow_missing" = "true" ]; then
            return 2
        fi

        if [ "$http_code" = "404" ]; then
            echo "error: ${label} was not found: ${url}" >&2
            echo "hint: the requested release may not include asset ${asset_name} for version ${VERSION}" >&2
        else
            echo "error: failed to download ${label} from ${url} (HTTP ${http_code})" >&2
        fi
        exit 1
    fi

    if command -v wget >/dev/null 2>&1; then
        if wget -qO "$output" "$url"; then
            return
        fi

        http_code="$(wget -S --spider "$url" 2>&1 | awk '/^  HTTP\// { code=$2 } END { print code }')"
        if [ "$http_code" = "404" ] && [ "$allow_missing" = "true" ]; then
            return 2
        fi

        if [ "$http_code" = "404" ]; then
            echo "error: ${label} was not found: ${url}" >&2
            echo "hint: the requested release may not include asset ${asset_name} for version ${VERSION}" >&2
        elif [ -n "$http_code" ]; then
            echo "error: failed to download ${label} from ${url} (HTTP ${http_code})" >&2
        else
            echo "error: failed to download ${label} from ${url}" >&2
        fi
        exit 1
    fi

    echo "error: either curl or wget is required" >&2
    exit 1
}

download_archive() {
    download_file "$RELEASE_URL" "$TMP_DIR/$ARCHIVE" "release archive" "$ARCHIVE"
}

download_checksum() {
    if download_file "$CHECKSUM_URL" "$TMP_DIR/$CHECKSUM_FILE" "checksum file" "$CHECKSUM_FILE" "true"; then
        HAS_CHECKSUM=1
        return
    fi

    if [ "$VERSION" = "latest" ]; then
        echo "error: checksum file was not found: ${CHECKSUM_URL}" >&2
        echo "hint: the latest release is expected to publish asset ${CHECKSUM_FILE}" >&2
        exit 1
    fi

    HAS_CHECKSUM=0
    echo "warning: checksum file ${CHECKSUM_FILE} is not available for ${VERSION}; proceeding without checksum verification for this pinned install" >&2
}

compute_sha256() {
    file="$1"

    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$file" | awk '{print $1}'
        return
    fi

    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$file" | awk '{print $1}'
        return
    fi

    echo "error: sha256sum or shasum is required for checksum verification" >&2
    exit 1
}

verify_checksum() {
    if [ "$HAS_CHECKSUM" -ne 1 ]; then
        return
    fi

    checksum="$(awk '{print $1}' "$TMP_DIR/$CHECKSUM_FILE")"
    if [ -z "$checksum" ]; then
        echo "error: checksum file is empty or malformed: $CHECKSUM_FILE" >&2
        exit 1
    fi

    actual_checksum="$(compute_sha256 "$TMP_DIR/$ARCHIVE")"
    if [ "$checksum" != "$actual_checksum" ]; then
        echo "error: checksum verification failed for $ARCHIVE" >&2
        echo "expected: $checksum" >&2
        echo "actual:   $actual_checksum" >&2
        exit 1
    fi
}

verify_archive_layout() {
    expected_entry="ferrus-${TARGET}/ferrus"
    if ! tar -tzf "$TMP_DIR/$ARCHIVE" | grep -Fx "$expected_entry" >/dev/null 2>&1; then
        echo "error: archive does not contain expected binary entry: $expected_entry" >&2
        exit 1
    fi
}

warn_existing_install() {
    if [ -e "$INSTALL_DIR/ferrus" ]; then
        echo "warning: overwriting existing installation at $INSTALL_DIR/ferrus" >&2
    fi
}

install_binary() {
    mkdir -p "$INSTALL_DIR"
    if [ -n "${HOME:-}" ]; then
        mkdir -p "${HOME}/.ferrus/projects"
    fi
    tar -xzf "$TMP_DIR/$ARCHIVE" -C "$TMP_DIR"

    BIN_PATH="$TMP_DIR/ferrus-${TARGET}/ferrus"

    if [ ! -f "$BIN_PATH" ]; then
        echo "error: ferrus binary not found in archive" >&2
        exit 1
    fi

    warn_existing_install
    install "$BIN_PATH" "$INSTALL_DIR/ferrus"
}

print_success() {
    installed_version="$("$INSTALL_DIR/ferrus" --version 2>/dev/null || true)"

    echo "installed ferrus to ${INSTALL_DIR}/ferrus"
    if [ -n "$installed_version" ]; then
        echo "version: ${installed_version}"
    else
        echo "warning: failed to determine installed ferrus version" >&2
    fi

    case ":$PATH:" in
        *:"$INSTALL_DIR":*)
            ;;
        *)
            echo "warning: ${INSTALL_DIR} is not on PATH" >&2
            echo "add this to your shell profile:" >&2
            echo "  export PATH=\"${INSTALL_DIR}:\$PATH\"" >&2
            ;;
    esac
}

main() {
    need_cmd uname
    need_cmd tar
    need_cmd mktemp
    need_cmd install

    detect_platform
    resolve_url
    pick_install_dir

    echo "installing ferrus (${VERSION}) for ${TARGET}"
    echo "download: ${RELEASE_URL}"

    TMP_DIR="$(mktemp -d)"
    trap 'rm -rf "$TMP_DIR"' EXIT INT TERM

    download_archive
    download_checksum
    verify_checksum
    verify_archive_layout
    install_binary
    print_success
}

main "$@"
