#!/usr/bin/env python3
"""Install a verified local macOS release, retaining one version for rollback."""
import argparse
import hashlib
from pathlib import Path, PurePosixPath
import plistlib
import stat
import subprocess
import sys
import tempfile
import zipfile


def verify(application, team_id, allow_ad_hoc):
    subprocess.run(['codesign', '--verify', '--deep', '--strict', str(application)], check=True)
    result = subprocess.run(['codesign', '--display', '--verbose=4', str(application)], check=True, capture_output=True, text=True)
    team = next((line.partition('=')[2] for line in result.stderr.splitlines() if line.startswith('TeamIdentifier=')), None)
    if team_id and team != team_id:
        raise ValueError('The application does not match the expected signing team.')
    if not team_id and not allow_ad_hoc:
        raise ValueError('Set --team-id to a trusted Developer ID team; local preview testing requires --allow-ad-hoc.')
    if team not in (None, 'not set') and not allow_ad_hoc:
        subprocess.run(['spctl', '--assess', '--type', 'execute', str(application)], check=True)
    with (application / 'Contents/Info.plist').open('rb') as handle:
        metadata = plistlib.load(handle)
    if metadata.get('CFBundleIdentifier') != 'local.semweft.preview':
        raise ValueError('This is not a Semweft desktop application.')
    return metadata


def version(metadata):
    return tuple(int(part) for part in metadata['CFBundleShortVersionString'].split('.')), int(metadata.get('CFBundleVersion', '0'))


def activate(candidate, destination, backup, staging):
    rollback = candidate == backup
    displaced = staging / 'previous.app' if rollback else backup
    older = staging / 'older.app'
    moved_older = False
    moved_destination = False
    activated = False
    try:
        if not rollback and backup.exists() and destination.exists():
            backup.rename(older)
            moved_older = True
        if destination.exists():
            destination.rename(displaced)
            moved_destination = True
        candidate.rename(destination)
        activated = True
        if rollback and moved_destination:
            displaced.rename(backup)
            moved_destination = False
    except BaseException:
        if activated:
            destination.rename(candidate)
        if moved_destination:
            displaced.rename(destination)
        if moved_older:
            older.rename(backup)
        raise


def install(archive, checksum, destination, team_id=None, allow_ad_hoc=False, rollback=False):
    if sys.platform != 'darwin':
        raise ValueError('This installer requires macOS.')
    destination = destination.expanduser().absolute()
    if destination.suffix != '.app' or destination.is_symlink():
        raise ValueError('Destination must be a real .app path.')
    destination.parent.mkdir(parents=True, exist_ok=True)
    backup = destination.with_name(destination.stem + '.previous.app')
    if backup.is_symlink():
        raise ValueError('Backup must not be a symbolic link.')
    lock_path = destination.with_suffix('.install.lock')
    import fcntl
    with lock_path.open('a') as lock, tempfile.TemporaryDirectory(prefix='.semweft-install-', dir=destination.parent) as directory:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        staging = Path(directory)
        if rollback:
            verify(backup, team_id, allow_ad_hoc)
            activate(backup, destination, backup, staging)
        else:
            if archive is None or checksum is None or len(checksum) != 64:
                raise ValueError('Install requires --archive and its trusted --sha256 digest.')
            digest = hashlib.sha256()
            frozen = staging / 'release.zip'
            with archive.open('rb') as source, frozen.open('wb') as target:
                for chunk in iter(lambda: source.read(1024 * 1024), b''):
                    digest.update(chunk)
                    target.write(chunk)
            if digest.hexdigest() != checksum.lower():
                raise ValueError('Archive checksum mismatch.')
            with zipfile.ZipFile(frozen) as package:
                entries = package.infolist()
                if len(entries) > 50000 or sum(entry.file_size for entry in entries) > 2 * 1024 * 1024 * 1024:
                    raise ValueError('Archive exceeds installation limits.')
                for entry in entries:
                    path = PurePosixPath(entry.filename)
                    if path.is_absolute() or '..' in path.parts or stat.S_ISLNK(entry.external_attr >> 16):
                        raise ValueError('Archive contains an unsafe path or symbolic link.')
            unpacked = staging / 'unpacked'
            unpacked.mkdir()
            subprocess.run(['ditto', '-x', '-k', str(frozen), str(unpacked)], check=True)
            applications = list(unpacked.glob('*.app'))
            if len(applications) != 1:
                raise ValueError('Archive must contain exactly one application.')
            candidate = applications[0]
            incoming = verify(candidate, team_id, allow_ad_hoc)
            if destination.exists():
                existing = verify(destination, team_id, allow_ad_hoc)
                if version(incoming) <= version(existing):
                    raise ValueError('Upgrade must have a newer version/build; use --rollback for the previous version.')
            activate(candidate, destination, backup, staging)
        print(destination)
        if backup.exists():
            print('Rollback available:', backup)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--archive', type=Path)
    parser.add_argument('--sha256')
    parser.add_argument('--destination', type=Path, default=Path('/Applications/Semweft Preview.app'))
    parser.add_argument('--team-id')
    parser.add_argument('--allow-ad-hoc', action='store_true', help='Explicitly allow local preview builds for development testing')
    parser.add_argument('--rollback', action='store_true')
    options = parser.parse_args()
    try:
        install(options.archive, options.sha256, options.destination, options.team_id, options.allow_ad_hoc, options.rollback)
    except (OSError, ValueError, subprocess.CalledProcessError, zipfile.BadZipFile) as error:
        parser.exit(1, str(error) + '\n')
