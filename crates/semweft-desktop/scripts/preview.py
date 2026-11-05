#!/usr/bin/env python3
"""Build a self-contained native GPUI macOS app."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import platform
import re
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[3]
BUILD = ROOT / 'target/desktop-preview'
APP = BUILD / 'Semweft Preview.app'
MANIFEST = ROOT / 'crates/semweft-desktop/Cargo.toml'


def build(workspace=None, release=False, sign_identity=None, notary_profile=None, app_version='0.1.0', build_number='1'):
    if sys.platform != 'darwin':
        raise SystemExit('The GPUI preview currently supports macOS with Xcode Command Line Tools.')
    if not re.fullmatch(r'\d+\.\d+\.\d+', app_version) or not re.fullmatch(r'[1-9]\d*', build_number):
        raise SystemExit('Use a numeric major.minor.patch app version and a positive build number.')
    if notary_profile and (not release or not sign_identity or sign_identity == '-'):
        raise SystemExit('Notarization requires --release and a Developer ID --sign-identity.')
    profile = ['--release'] if release else []
    subprocess.run(['cargo', 'build', '--locked', *profile, '-p', 'semweft-cli', '--features', 'go-experimental,web-experimental'], cwd=ROOT, check=True)
    subprocess.run(['cargo', 'build', '--locked', *profile, '--manifest-path', str(MANIFEST)], cwd=ROOT, check=True)
    target = 'release' if release else 'debug'
    binary = ROOT / f'crates/semweft-desktop/target/{target}/semweft-desktop'
    subprocess.run([str(binary), '--check-text'], cwd=ROOT, check=True)
    from export_preview import demo
    example = demo()
    engine = ROOT / f'target/{target}/semweft'
    subprocess.run([str(binary), '--capture', str(workspace or example), str(BUILD / 'data.json')], cwd=ROOT, check=True,
                   env={**os.environ, 'SEMWEFT_ENGINE': str(engine)})
    subprocess.run([str(binary), '--check-data', str(BUILD / 'data.json')], cwd=ROOT, check=True)
    resources = APP / 'Contents/Resources'
    executable = APP / 'Contents/MacOS/SemweftPreview'
    executable.parent.mkdir(parents=True, exist_ok=True)
    resources.mkdir(parents=True, exist_ok=True)
    shutil.rmtree(resources / 'ui', ignore_errors=True)
    (resources / 'export_preview.py').unlink(missing_ok=True)
    with tempfile.TemporaryDirectory(prefix='.semweft-', dir=executable.parent) as staging:
        replacement = Path(staging) / executable.name
        shutil.copy2(binary, replacement)
        replacement.replace(executable)
        replacement = Path(staging) / 'semweft'
        shutil.copy2(engine, replacement)
        replacement.replace(executable.parent / 'semweft')
    shutil.rmtree(resources / 'example-workspace', ignore_errors=True)
    shutil.copytree(example, resources / 'example-workspace', ignore=shutil.ignore_patterns('target'))
    shutil.copytree(ROOT / 'crates/semweft-desktop/licenses', resources / 'licenses', dirs_exist_ok=True)
    (resources / 'config.json').write_text(json.dumps({
        'workspace': str(workspace) if workspace else None,
    }))
    with (APP / 'Contents/Info.plist').open('wb') as handle:
        plistlib.dump({
            'CFBundleExecutable': 'SemweftPreview', 'CFBundleIdentifier': 'local.semweft.preview',
            'CFBundleName': 'Semweft Preview', 'CFBundleDisplayName': 'Semweft Preview',
            'CFBundlePackageType': 'APPL', 'CFBundleShortVersionString': app_version,
            'CFBundleVersion': build_number,
            'NSHighResolutionCapable': True, 'NSPrincipalClass': 'NSApplication',
            'LSMinimumSystemVersion': '13.0',
        }, handle)
    signature = ['--options', 'runtime', '--timestamp'] if sign_identity and sign_identity != '-' else ['--timestamp=none']
    for target_path in [executable.parent / 'semweft', executable, APP]:
        subprocess.run(['codesign', '--force', '--sign', sign_identity or '-', *signature, str(target_path)], check=True)
    subprocess.run(['codesign', '--verify', '--deep', '--strict', str(APP)], check=True)
    print(APP)
    archive = BUILD / 'Semweft-macOS.zip'
    subprocess.run(['ditto', '-c', '-k', '--sequesterRsrc', '--keepParent', str(APP), str(archive)], check=True)
    if notary_profile:
        subprocess.run(['xcrun', 'notarytool', 'submit', str(archive), '--keychain-profile', notary_profile, '--wait'], check=True)
        subprocess.run(['xcrun', 'stapler', 'staple', str(APP)], check=True)
        subprocess.run(['xcrun', 'stapler', 'validate', str(APP)], check=True)
        subprocess.run(['spctl', '--assess', '--type', 'execute', str(APP)], check=True)
        archive.unlink()
        subprocess.run(['ditto', '-c', '-k', '--sequesterRsrc', '--keepParent', str(APP), str(archive)], check=True)
    checksum = hashlib.sha256()
    with archive.open('rb') as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b''):
            checksum.update(chunk)
    (BUILD / 'release.json').write_text(json.dumps({
        'schema_version': '0.1', 'app_version': app_version, 'build_number': build_number,
        'platform': 'macos', 'architecture': platform.machine(), 'minimum_macos': '13.0',
        'archive': archive.name, 'sha256': checksum.hexdigest(), 'size_bytes': archive.stat().st_size,
        'signature': 'developer_id' if sign_identity and sign_identity != '-' else 'ad_hoc',
        'notarized': bool(notary_profile),
    }, indent=2) + '\n')
    print(archive)
    return APP


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--workspace', type=Path, help='Rust / Go / TS-JS project; defaults to an isolated example')
    parser.add_argument('--release', action='store_true', help='Build optimized desktop and bundled engine')
    parser.add_argument('--sign-identity', help='Developer ID identity available in the local keychain')
    parser.add_argument('--notary-profile', help='Explicitly submit to Apple using this notarytool keychain profile')
    parser.add_argument('--app-version', default='0.1.0')
    parser.add_argument('--build-number', default='1')
    args = parser.parse_args()
    build(args.workspace.resolve() if args.workspace else None, args.release, args.sign_identity, args.notary_profile, args.app_version, args.build_number)
