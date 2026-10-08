#!/usr/bin/env bash
# Collect executables, their recursive runtime dependencies and notices.
# Run on the target OS/architecture, never copy binaries across architectures.
set -euo pipefail
export PATH="/usr/bin:/bin:$PATH"
repo_root=$(cd "$(dirname "$0")/.." && pwd)
output="$repo_root/src-tauri/resources/sync-tools"
rm -rf "${output:?}/bin" "${output:?}/lib" "${output:?}/licenses"
mkdir -p "$output/bin" "$output/lib" "$output/licenses"

case "$(uname -s)" in
  Linux) platform=linux ;;
  MSYS*|MINGW*) platform=windows ;;
  Darwin) exit 0 ;;
  *) echo 'Unsupported sync-tools build host' >&2; exit 1 ;;
esac

declare -a queue=() packages=()
declare -A seen=() seen_packages=()
for name in rsync ssh ssh-keygen; do
  suffix=''
  if [[ $platform == windows ]]; then suffix='.exe'; fi
  source="/usr/bin/$name$suffix"
  if [[ ! -f $source ]]; then
    echo "Missing build dependency: $source (install rsync and openssh on the build host)" >&2
    exit 1
  fi
  cp "$source" "$output/bin/$name$suffix"
  queue+=("$source")
done

record_package() {
  local path=$1 package
  if [[ $platform == windows ]]; then
    package=$(pacman -Qoq "$path")
  else
    package=$(dpkg-query -S "$path" 2>/dev/null | head -1 | sed 's/: \/.*//') || true
    if [[ -z $package ]]; then
      package=$(dpkg-query -S "*/$(basename "$path")" | head -1 | sed 's/: \/.*//')
    fi
  fi
  if [[ -z $package ]]; then echo "Cannot identify package for $path" >&2; exit 1; fi
  if [[ -z ${seen_packages[$package]+x} ]]; then
    seen_packages[$package]=1
    packages+=("$package")
  fi
}

for ((i=0; i<${#queue[@]}; i++)); do
  source=${queue[$i]}
  if [[ -n ${seen[$source]+x} ]]; then continue; fi
  seen[$source]=1
  record_package "$source"
  dependencies=$(ldd "$source")
  if [[ $dependencies == *'not found'* ]]; then
    echo "$dependencies" >&2; exit 1
  fi
  while IFS= read -r library; do
    [[ -n $library ]] || continue
    library=$(readlink -f "$library")
    filename=$(basename "$library")
    if [[ $platform == windows ]]; then
      # Windows system DLLs stay on the OS. Only the MSYS2 DLL closure ships.
      [[ $library == /usr/bin/* ]] || continue
      cp "$library" "$output/bin/$filename"
    else
      # Use the host glibc/loader; copying them breaks portability and NSS.
      case "$filename" in
        ld-linux*|libc.so*|libm.so*|libpthread.so*|libdl.so*|librt.so*|libresolv.so*|libutil.so*) continue ;;
      esac
      # Keep the SONAME, not the versioned file's basename (e.g. libacl.so.1).
      soname=$(patchelf --print-soname "$library")
      cp "$library" "$output/lib/${soname:-$filename}"
    fi
    queue+=("$library")
  done < <(printf '%s\n' "$dependencies" | sed -nE 's/.*=> (\/[^ ]+).*/\1/p; s/^[[:space:]]*(\/[^ ]+).*/\1/p')
done

: > "$output/PACKAGES.txt"
sources="$repo_root/src-tauri/target/sync-tool-sources"
if [[ ${SYNC_TOOLS_COLLECT_SOURCES:-0} == 1 ]]; then mkdir -p "$sources"; fi
for package in "${packages[@]}"; do
  safe_package=${package//:/_}
  mkdir -p "$output/licenses/$safe_package"
  if [[ $platform == windows ]]; then
    pacman -Qi "$package" >> "$output/PACKAGES.txt"
    # Use # as the address delimiter: | also occurs in the ERE alternation.
    # Capture first so set -e/pipefail catches failures in notice collection.
    license_files=$(pacman -Qlq "$package" | sed -nE '\#/share/licenses/#p; \#/share/doc/.*/(COPYING[^/]*|LICENSE[^/]*|CYGWIN_LICENSE)$#p')
    while IFS= read -r file; do
      if [[ -f $file ]]; then cp "$file" "$output/licenses/$safe_package/"; fi
    done <<< "$license_files"
    if [[ ${SYNC_TOOLS_COLLECT_SOURCES:-0} == 1 ]]; then
      version=$(pacman -Q "$package" | cut -d' ' -f2)
      desc="/var/lib/pacman/local/$package-$version/desc"
      base=$(sed -n '/^%BASE%$/{n;p;}' "$desc")
      base=${base:-$package}
      url="https://repo.msys2.org/msys/sources/$base-$version.src.tar.zst"
      curl --fail --location --retry 3 "$url" -o "$sources/$base-$version.src.tar.zst"
    fi
  else
    dpkg-query -W -f='${Package} ${Version} ${source:Package} ${source:Version}\n' "$package" >> "$output/PACKAGES.txt"
    doc="/usr/share/doc/${package%%:*}/copyright"
    [[ -f $doc ]] || { echo "Missing copyright for $package" >&2; exit 1; }
    cp -L "$doc" "$output/licenses/$safe_package/copyright"
    if [[ ${SYNC_TOOLS_COLLECT_SOURCES:-0} == 1 ]]; then
      source_package=$(dpkg-query -W -f='${source:Package}=${source:Version}' "$package")
      (cd "$sources" && apt-get source --download-only "$source_package")
    fi
  fi
done
if [[ $platform == linux ]]; then
  cp /usr/share/common-licenses/GPL-3 "$output/licenses/GPL-3"
fi

# Test the collected binaries with system rsync/SSH removed from PATH.
# LD_LIBRARY_PATH matches the app's child-only environment on Linux.
suffix=''
if [[ $platform == windows ]]; then suffix='.exe'; fi
env PATH=/nonexistent LD_LIBRARY_PATH="$output/lib" "$output/bin/rsync$suffix" --version
env PATH=/nonexistent LD_LIBRARY_PATH="$output/lib" "$output/bin/ssh$suffix" -V
fixture=$(mktemp -d)
trap 'rm -rf "$fixture"' EXIT
env PATH=/nonexistent LD_LIBRARY_PATH="$output/lib" "$output/bin/ssh-keygen$suffix" -t ed25519 -N '' -f "$fixture/key"
printf 'bundle-signature-test' | env PATH=/nonexistent LD_LIBRARY_PATH="$output/lib" "$output/bin/ssh-keygen$suffix" -Y sign -n sync-bundle-test -f "$fixture/key" > "$fixture/signature"
printf 'fixture %s\n' "$(cat "$fixture/key.pub")" > "$fixture/allowed_signers"
printf 'bundle-signature-test' | env PATH=/nonexistent LD_LIBRARY_PATH="$output/lib" "$output/bin/ssh-keygen$suffix" -Y verify -n sync-bundle-test -I fixture -f "$fixture/allowed_signers" -s "$fixture/signature"
(cd "$output" && find bin lib -type f -print0 | sort -z | xargs -0 sha256sum > SHA256SUMS)
if [[ ${SYNC_TOOLS_COLLECT_SOURCES:-0} == 1 ]]; then
  tar -czf "$repo_root/src-tauri/target/sync-tools-sources-$platform-$(uname -m).tar.gz" -C "$sources" .
fi
