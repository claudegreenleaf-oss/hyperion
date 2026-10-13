#!/usr/bin/env python3
"""Stage individual Chromium files, patched exactly as the build patches them.

Writing a Hyperion patch needs the file as it looks after every Helium patch,
but a full Chromium checkout is ~100 GB. This fetches single files from
gitiles at the pinned Chromium tag and replays the patch series onto them.

  hyperion_stage.py stage <path>...   fetch + patch into .stage/{base,work}
  hyperion_stage.py diff  <path>...   unified diff work vs base (a patch)
  hyperion_stage.py check             dry-run every hyperion patch on staged bases

Patches from patches/hyperion/ are applied to `base` only up to (not
including) the patch being written, so pass --upto NAME when revising one.
"""

import argparse
import base64
import difflib
import re
import shutil
import subprocess
import sys
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MAIN = ROOT / 'helium-chromium'
STAGE = ROOT / '.stage'
TAG = (MAIN / 'chromium_version.txt').read_text().strip()


def series():
    """Yield every patch path in apply order (main series, then platform)."""
    for tree in (MAIN / 'patches', ROOT / 'patches'):
        for line in (tree / 'series').read_text().splitlines():
            line = line.split('#')[0].strip()
            if line:
                yield tree / line


def sections(patch_text):
    """Split a multi-file patch into {path: section_text}."""
    out = {}
    for part in re.split(r'(?m)^(?=--- )', patch_text):
        m = re.search(r'(?m)^\+\+\+ b/(\S+)', part)
        if m:
            out.setdefault(m.group(1), []).append(part)
    return {k: ''.join(v) for k, v in out.items()}


def fetch_dep(path):
    """Files under a deps.ini output_path come from that dep's archive."""
    import configparser, io, zipfile
    cfg = configparser.ConfigParser()
    cfg.read(MAIN / 'deps.ini')
    for name in cfg.sections():
        sec = cfg[name]
        prefix = sec['output_path'] + '/'
        if path.startswith(prefix):
            cache = STAGE / 'deps' / sec['download_filename']
            if not cache.exists():
                cache.parent.mkdir(parents=True, exist_ok=True)
                urllib.request.urlretrieve(sec['url'], cache)
            inner = sec.get('strip_leading_dirs', '') + '/' + path[len(prefix):]
            with zipfile.ZipFile(cache) as z:
                return z.read(inner.lstrip('/'))
    return False


def fetch(path):
    dep = fetch_dep(path)
    if dep is not False:
        return dep
    url = (f'https://chromium.googlesource.com/chromium/src/+/refs/tags/'
           f'{TAG}/{path}?format=TEXT')
    try:
        with urllib.request.urlopen(url) as r:
            return base64.b64decode(r.read())
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return None  # file is created by a patch
        raise


def apply(tree, section, name):
    proc = subprocess.run(
        ['patch', '-p1', '-s', '--ignore-whitespace', '--no-backup-if-mismatch',
         '-d', str(tree)], input=section.encode(), capture_output=True)
    if proc.returncode:
        sys.exit(f'{name} failed on staged file:\n{proc.stdout.decode()}'
                 f'{proc.stderr.decode()}')


def stage(paths, upto):
    base = STAGE / 'base'
    for path in paths:
        dst = base / path
        dst.parent.mkdir(parents=True, exist_ok=True)
        data = fetch(path)
        if data is None:
            dst.unlink(missing_ok=True)
        else:
            dst.write_bytes(data)
        applied = 0
        for patch in series():
            if upto and patch.name == upto:
                break
            sec = sections(patch.read_text(errors='replace')).get(path)
            if sec:
                apply(base, sec, patch.relative_to(ROOT))
                applied += 1
        work = STAGE / 'work' / path
        work.parent.mkdir(parents=True, exist_ok=True)
        if dst.exists():
            shutil.copyfile(dst, work)
        else:
            work.unlink(missing_ok=True)  # created by the patch being written
        print(f'{path}: {applied} patches applied -> .stage/work/{path}')


def diff(paths):
    for path in paths:
        base, work = STAGE / 'base' / path, STAGE / 'work' / path
        a = base.read_text().splitlines(keepends=True) if base.exists() else []
        b = work.read_text().splitlines(keepends=True) if work.exists() else []
        sys.stdout.writelines(
            difflib.unified_diff(a, b, f'a/{path}', f'b/{path}', n=3))


def check():
    """Restage each file a hyperion patch touches, then dry-run that patch."""
    ok = True
    for patch in series():
        if patch.parent.name != 'hyperion':
            continue
        for path, sec in sections(patch.read_text()).items():
            stage([path], patch.name)
            proc = subprocess.run(
                ['patch', '-p1', '--dry-run', '--fuzz=0', '-d',
                 str(STAGE / 'base')], input=sec.encode(), capture_output=True)
            status = 'ok' if proc.returncode == 0 else 'FAIL'
            ok &= proc.returncode == 0
            print(f'{status} {patch.name} {path}')
    sys.exit(0 if ok else 1)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('cmd', choices=('stage', 'diff', 'check'))
    ap.add_argument('paths', nargs='*')
    ap.add_argument('--upto', help='stop before this patch file name')
    args = ap.parse_args()
    if args.cmd == 'stage':
        stage(args.paths, args.upto)
    elif args.cmd == 'diff':
        diff(args.paths)
    else:
        check()


if __name__ == '__main__':
    main()
