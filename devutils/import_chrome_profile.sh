#!/bin/bash
# Make Hyperion a full copy of a Chrome user data dir: every profile, extension, cookie, saved
# password, open tab and setting. Chrome and Hyperion use the same v10 Safe Storage scheme, so
# copying Chrome's keychain secret into Hyperion's item keeps all encrypted data readable.
# Usage: import_chrome_profile.sh [chrome-user-data-dir]   (Hyperion must not be running)
set -euo pipefail

SRC=${1:-"$HOME/Library/Application Support/Google/Chrome"}
DST="$HOME/Library/Application Support/Hyperion"
APP=/Applications/Hyperion.app
BUNDLE_ID=com.thomasgreenleaf.hyperion

if pgrep -f "$APP/Contents/MacOS/" >/dev/null; then
  echo "quit Hyperion first" >&2; exit 1
fi
[ -f "$SRC/Local State" ] || { echo "no Chrome user data at $SRC" >&2; exit 1; }

# 1. Keychain: Hyperion's Safe Storage item gets Chrome's secret (macOS asks to allow this once).
secret=$(security find-generic-password -w -s "Chrome Safe Storage" -a "Chrome")
security delete-generic-password -s "Hyperion Safe Storage" -a "Hyperion" >/dev/null 2>&1 || true
security add-generic-password -s "Hyperion Safe Storage" -a "Hyperion" -T "$APP" -w "$secret"
unset secret

# 2. Profile data. Caches are rebuilt on demand; Singleton* are Chrome's live process locks.
if [ -d "$DST" ]; then
  mv "$DST" "$DST.before-import-$(date +%Y%m%d-%H%M%S)"
fi
mkdir -p "$DST"
rsync -a \
  --exclude 'Singleton*' --exclude 'RunningChromeVersion' --exclude 'Last Version' \
  --exclude 'Cache/' --exclude 'Code Cache/' --exclude 'GPUCache/' --exclude 'DawnGraphiteCache/' \
  --exclude 'DawnWebGPUCache/' --exclude 'GrShaderCache/' --exclude 'ShaderCache/' \
  --exclude 'GraphiteDawnCache/' --exclude 'GPUPersistentCache/' --exclude 'Crashpad/' \
  --exclude 'component_crx_cache/' --exclude 'optimization_guide_model_store/' \
  "$SRC/" "$DST/"

# 3. Hyperion cannot sign in to a Google account, and Chromium drops account-scoped data when no
#    account is signed in. Fold account passwords and bookmarks into each profile's local store.
python3 - "$DST" <<'EOF'
import json, os, sqlite3, sys

dst = sys.argv[1]
profiles = json.load(open(os.path.join(dst, "Local State")))["profile"]["info_cache"]
for name in profiles:
    path = os.path.join(dst, name)
    acct = os.path.join(path, "Login Data For Account")
    local = os.path.join(path, "Login Data")
    if os.path.exists(acct) and os.path.exists(local):
        db = sqlite3.connect(local)
        db.execute("ATTACH ? AS acct", (acct,))
        cols = [r[1] for r in db.execute("PRAGMA main.table_info(logins)") if r[1] != "id"]
        acct_cols = {r[1] for r in db.execute("PRAGMA acct.table_info(logins)")}
        cols = [c for c in cols if c in acct_cols]
        col_list = ", ".join(cols)
        before = db.total_changes
        db.execute(f"INSERT OR IGNORE INTO main.logins ({col_list}) SELECT {col_list} FROM acct.logins")
        db.commit()
        print(f"{name}: merged {db.total_changes - before} account passwords")
        db.close()
    acct_bm = os.path.join(path, "AccountBookmarks")
    local_bm = os.path.join(path, "Bookmarks")
    if os.path.exists(acct_bm):
        a = json.load(open(acct_bm))
        if os.path.exists(local_bm):
            b = json.load(open(local_bm))
            for root in ("bookmark_bar", "other", "synced"):
                if root in a["roots"] and root in b["roots"]:
                    b["roots"][root].setdefault("children", []).extend(a["roots"][root].get("children", []))
        else:
            b = a
        b.pop("checksum", None)
        json.dump(b, open(local_bm, "w"), indent=3)
        print(f"{name}: merged account bookmarks")
EOF

# 4. Chrome's own policies (DoH, memory saver, extension blocklist) under Hyperion's bundle id.
if defaults read com.google.Chrome >/dev/null 2>&1; then
  python3 - "$BUNDLE_ID" <<'EOF'
import plistlib, subprocess, sys, tempfile
data = plistlib.loads(subprocess.run(["defaults", "export", "com.google.Chrome", "-"],
                                     capture_output=True, check=True).stdout)
keep = {k: v for k, v in data.items() if k[:1].isupper() and not k.startswith(("NS", "LastRun"))}
with tempfile.NamedTemporaryFile(suffix=".plist") as f:
    plistlib.dump(keep, f)
    f.flush()
    subprocess.run(["defaults", "import", sys.argv[1], f.name], check=True)
print("policies:", ", ".join(sorted(keep)))
EOF
fi

echo "imported $SRC into $DST"
