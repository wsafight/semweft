#!/usr/bin/env python3
"""Build a native GPUI macOS preview and capture engine data."""
import argparse
import json
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[3]
BUILD = ROOT / 'target/desktop-preview'
APP = BUILD / 'Semweft Preview.app'
MANIFEST = ROOT / 'crates/semweft-desktop/Cargo.toml'


def build(workspace=None):
    if sys.platform != 'darwin':
        raise SystemExit('The GPUI preview currently supports macOS with Xcode Command Line Tools.')
    subprocess.run(['cargo', 'build', '--locked', '-p', 'semweft-cli', '--features', 'go-experimental,web-experimental'], cwd=ROOT, check=True)
    subprocess.run(['cargo', 'build', '--locked', '--manifest-path', str(MANIFEST)], cwd=ROOT, check=True)
    binary = ROOT / 'crates/semweft-desktop/target/debug/semweft-desktop'
    subprocess.run([str(binary), '--check-text'], cwd=ROOT, check=True)
    arguments = [sys.executable, str(ROOT / 'crates/semweft-desktop/scripts/export_preview.py'), '--output', str(BUILD / 'data.json')]
    if workspace:
        arguments += ['--workspace', str(workspace)]
    subprocess.run(arguments, cwd=ROOT, check=True)
    subprocess.run([str(binary), '--check-data', str(BUILD / 'data.json')], cwd=ROOT, check=True)
    resources = APP / 'Contents/Resources'
    executable = APP / 'Contents/MacOS/SemweftPreview'
    executable.parent.mkdir(parents=True, exist_ok=True)
    resources.mkdir(parents=True, exist_ok=True)
    # Remove resources left by the replaced WebKit preview.
    shutil.rmtree(resources / 'ui', ignore_errors=True)
    (resources / 'export_preview.py').unlink(missing_ok=True)
    # Replace the inode instead of overwriting a running Mach-O. macOS caches code
    # validation per vnode and can kill a newly launched, overwritten executable.
    with tempfile.TemporaryDirectory(prefix='.semweft-', dir=executable.parent) as staging:
        replacement = Path(staging) / executable.name
        shutil.copy2(binary, replacement)
        replacement.replace(executable)
    (resources / 'config.json').write_text(json.dumps({
        'repository': str(ROOT), 'workspace': str(workspace) if workspace else None,
        'python': sys.executable,
    }))
    with (APP / 'Contents/Info.plist').open('wb') as handle:
        plistlib.dump({
            'CFBundleExecutable': 'SemweftPreview', 'CFBundleIdentifier': 'local.semweft.preview',
            'CFBundleName': 'Semweft Preview', 'CFBundleDisplayName': 'Semweft Preview',
            'CFBundlePackageType': 'APPL', 'CFBundleShortVersionString': '0.1',
            'NSHighResolutionCapable': True, 'NSPrincipalClass': 'NSApplication',
            'LSMinimumSystemVersion': '13.0',
        }, handle)
    subprocess.run(['codesign', '--force', '--sign', '-', '--timestamp=none', str(APP)], check=True)
    subprocess.run(['codesign', '--verify', '--strict', str(APP)], check=True)
    print(APP)
    return APP


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--workspace', type=Path, help='Rust / Go / TS-JS project; defaults to an isolated example')
    args = parser.parse_args()
    build(args.workspace.resolve() if args.workspace else None)
