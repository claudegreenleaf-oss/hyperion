#!/usr/bin/env bash
# Cross-build Hyperion for macOS arm64 on a Linux host (the a6000-box).
# Chromium supports Mac targets on Linux hosts (clang-mac-runtime-library, hermetic Xcode
# dir); the macOS SDK comes from a Mac's Command Line Tools, see stage "sdk".
# Usage: cross_build_linux.sh <stage>...   stages: fetch toolchain patch sdk gen build
set -euxo pipefail

_root_dir="$(dirname "$(dirname "$(readlink -f "$0")")")"
mkdir -p "$_root_dir/.shim"
ln -sf "$(command -v readlink)" "$_root_dir/.shim/greadlink"
export PATH="$_root_dir/.shim:$PATH"
source "$_root_dir/env.sh"
source "$_root_dir/devutils/build_tools.sh"
source "$_root_dir/devutils/shared.sh"
_gn_path="$_src_dir/buildtools/linux64/gn"

for stage in "$@"; do
  case "$stage" in
    fetch)
      rm -rf "$_src_dir"
      mkdir -p "$_download_cache" "$_root_dir/build"
      "$_root_dir/retrieve_and_unpack_resource.sh" -d -g arm64
      # clang/update.py reads target_os from the .gclient beside src to add the Mac runtime.
      echo "target_os = ['mac']" > "$_root_dir/build/.gclient"
      python3 "$_main_repo/utils/prune_binaries.py" "$_src_dir" "$_main_repo/pruning.list"
      ;;
    toolchain)
      cd "$_src_dir"
      tools/rust/update_rust.py
      for pkg in clang objdump clang-tidy libclang; do
        tools/clang/scripts/update.py --package $pkg
      done
      third_party/node/update_node_binaries
      # The lite tarball leaves the Linux clang-format GCS object out; crubit codegen needs it.
      obj=$(python3 -c 'import re; d = open("DEPS").read(); i = d.index("src/buildtools/linux64-format"); print(re.search(r"object_name.: .([0-9a-f]+)", d[i:]).group(1))')
      curl -sfL -o buildtools/linux64-format/clang-format \
        "https://storage.googleapis.com/chromium-clang-format/$obj"
      chmod +x buildtools/linux64-format/clang-format
      # Linux-host tools DEPS gates on host_os == "linux" (gperf) or a mac checkout (mig,
      # prebuilt ANGLE Metal shaders); the lite tarball has none of them.
      export CIPD_CACHE_DIR="$_download_cache/cipd"
      while read -r dir pkg ver; do
        echo "$pkg $ver" | third_party/depot_tools/cipd ensure -root "$dir" -ensure-file -
      done <<'PKGS'
third_party/gperf/cipd infra/3pp/tools/gperf/${platform} version:3@3.2
third_party/mig/bin chromium/third_party/mig/${platform} 4wxov_ILjFdgBumBqgUgOgIcr4kcMh7i4b4oJi_cLjcC
ui/gl/resources/angle-metal chromium/gpu/angle-metal-shader-libraries S0FPOVKrgaiqyuR20SSHiPorLgYez29bfwEdKBobUMMC
PKGS
      # update_pgo_profiles.py needs vpython3/gsutil; the bucket is public over HTTPS.
      profile=$(cat chrome/build/mac-arm.pgo.txt)
      mkdir -p chrome/build/pgo_profiles
      curl -sfL -o "chrome/build/pgo_profiles/$profile" \
        "https://storage.googleapis.com/chromium-optimization-profiles/pgo_profiles/$profile"
      ;;
    patch)
      python3 "$_main_repo/utils/patches.py" apply "$_src_dir" "$_main_repo/patches" "$_root_dir/patches"
      python3 "$_main_repo/utils/domain_substitution.py" apply -r "$_main_repo/domain_regex.list" -f "$_main_repo/domain_substitution.list" "$_src_dir"
      python3 "$_main_repo/utils/name_substitution.py" --sub -t "$_src_dir"
      python3 "$_main_repo/utils/i18n_apply.py" -t "$_src_dir"
      python3 "$_main_repo/utils/helium_version.py" \
        --tree "$_main_repo" --platform-tree "$_root_dir" --chromium-tree "$_src_dir"
      helium_resources
      ;;
    sdk)
      # Expects $_root_dir/build/MacOSX.sdk.tar (the SDK named in mac_sdk_official_version)
      # and $_root_dir/build/xcode-version.plist, copied from a Mac.
      dev="$_src_dir/build/mac_files/xcode_binaries/Contents/Developer"
      sdks="$dev/Platforms/MacOSX.platform/Developer/SDKs"
      mkdir -p "$sdks" "$dev/Toolchains/XcodeDefault.xctoolchain/usr/bin"
      tar -xf "$_root_dir/build/MacOSX.sdk.tar" -C "$sdks"
      cp "$_root_dir/build/xcode-version.plist" "$_src_dir/build/mac_files/xcode_binaries/Contents/version.plist"
      ;;
    gen)
      write_gn_args arm64 release true
      printf 'target_os = "mac"\nuse_system_xcode = false\n# Helium hardcodes /usr/bin/dsymutil; dSYMs are not needed here.\nenable_dsyms = false\n' >> "$_out_dir/args.gn"
      sed -i '/cc_wrapper/d' "$_out_dir/args.gn"
      cd "$_src_dir"
      ___helium_install_cipd_deps
      ___helium_configure_siso
      "$_gn_path" gen out/Default --fail-on-unused-args
      ;;
    build)
      cd "$_src_dir"
      SISO_PATH="$_siso_path" python3 "$_depot_tools_dir/autoninja.py" -C "$_out_dir" \
        -j "${JOBS:-120}" -k 0 chrome
      ;;
    *) echo "unknown stage $stage" >&2; exit 2 ;;
  esac
done
