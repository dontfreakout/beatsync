#!/usr/bin/env bash
# Fails when a Linux bundle needs a newer glibc than MAX_GLIBC.
#
# Usage: check-glibc.sh MAX_GLIBC BUNDLE...
#   MAX_GLIBC  oldest glibc the bundles must run on, for example 2.39
#   BUNDLE     an .AppImage, .deb or .rpm file, or a directory
#
# An AppImage carries the GTK and WebKit libraries of the build system but uses
# the glibc of the target system. The loader rejects any library that names a
# glibc symbol version the target glibc does not define. This script lists those
# names for every ELF file in the bundle.
set -euo pipefail

if [[ $# -lt 2 ]]; then
  echo "usage: $0 MAX_GLIBC BUNDLE..." >&2
  exit 2
fi
max=$1
shift

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# Prints the files of one bundle as a directory.
unpack() {
  local bundle dir
  bundle=$(realpath "$1") # absolute: the cases below change directory
  if [[ -d $bundle ]]; then
    echo "$bundle"
    return
  fi
  dir=$(mktemp -d "$work/unpack.XXXXXX")
  case $bundle in
    *.AppImage)
      # The AppImage runtime extracts into ./squashfs-root.
      (cd "$dir" && "$bundle" --appimage-extract >/dev/null)
      ;;
    *.deb) dpkg-deb -x "$bundle" "$dir" ;;
    *.rpm) (cd "$dir" && rpm2cpio "$bundle" | cpio -id --quiet) ;;
    *)
      echo "unknown bundle type: $bundle" >&2
      exit 2
      ;;
  esac
  echo "$dir"
}

# True when glibc MAX_GLIBC defines the symbol version $1.
allowed() {
  local name=$1
  case $name in
    GLIBC_2.*) [[ $(printf '%s\n%s\n' "${name#GLIBC_}" "$max" | sort -V | tail -n1) == "$max" ]] ;;
    # glibc 2.36 added this marker. Other GLIBC_ABI_* markers (GLIBC_ABI_GNU2_TLS)
    # are newer, or are backports that some distributions do not have.
    GLIBC_ABI_DT_RELR) [[ $(printf '%s\n%s\n' 2.36 "$max" | sort -V | tail -n1) == "$max" ]] ;;
    *) false ;;
  esac
}

failed=0
for bundle in "$@"; do
  dir=$(unpack "$bundle")
  highest=""
  while IFS= read -r -d '' file; do
    head -c 4 "$file" 2>/dev/null | grep -q $'^\x7fELF' || continue
    # Only the "Version needs" section names what the loader must find.
    needs=$(readelf -V -W "$file" 2>/dev/null |
      awk '/^Version needs section/ { on = 1; next }
           /^Version (definition|symbols) section/ { on = 0 }
           on && match($0, /Name: GLIBC_[A-Za-z0-9_.]+/) { print substr($0, RSTART + 6, RLENGTH - 6) }' |
      sort -u)
    bad=()
    for name in $needs; do
      if [[ $name == GLIBC_2.* ]]; then
        highest=$(printf '%s\n%s\n' "${highest:-2.0}" "${name#GLIBC_}" | sort -V | tail -n1)
      fi
      allowed "$name" || bad+=("$name")
    done
    if [[ ${#bad[@]} -gt 0 ]]; then
      echo "  ${file#"$dir"/}: ${bad[*]}"
      failed=1
    fi
  done < <(find "$dir" -type f -print0)
  echo "$(basename "$bundle"): needs glibc ${highest:-(none)}"
done

if [[ $failed -ne 0 ]]; then
  echo "FAIL: the files above need a glibc newer than $max" >&2
  exit 1
fi
echo "OK: all bundles run on glibc $max"
