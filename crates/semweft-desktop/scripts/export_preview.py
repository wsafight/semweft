#!/usr/bin/env python3
"""Export a fixed, read-only Semweft view for the desktop preview."""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[3]
if not (ROOT / 'Cargo.toml').exists():
    ROOT = Path(json.loads((Path(__file__).parent / 'config.json').read_text())['repository'])
BUILD = ROOT / 'target' / 'desktop-preview'

BASE = {
    'Cargo.toml': '[package]\nname = "checkout-service"\nversion = "0.1.0"\nedition = "2024"\n\n[workspace]\n',
    'src/lib.rs': 'pub mod inventory;\npub mod order;\npub mod cart;\npub mod checkout;\npub mod api;\npub mod pricing;\n',
    'src/inventory.rs': '''/// Inventory available for an order.
pub struct Stock {
    pub available: u32,
}

impl Stock {
    /// Reserve inventory for the requested quantity.
    pub fn reserve(&self, quantity: u32) -> u32 {
        quantity.min(self.available)
    }
}

/// Return the currently available inventory.
pub fn available(stock: &Stock) -> u32 {
    stock.available
}
''',
    'src/order.rs': '''use crate::inventory::Stock;

/// Create an order with reserved inventory.
pub fn create(stock: &Stock, quantity: u32) -> u32 {
    stock.reserve(quantity)
}

/// Retry an order after inventory becomes available.
pub fn retry(stock: &Stock, quantity: u32) -> u32 {
    create(stock, quantity)
}
''',
    'src/cart.rs': '''use crate::inventory::Stock;

/// Preview the number of items that can be purchased.
pub fn preview(stock: &Stock, quantity: u32) -> u32 {
    stock.reserve(quantity)
}
''',
    'src/checkout.rs': '''use crate::{inventory::Stock, order};

/// Submit the cart and create an order.
pub fn submit(stock: &Stock, quantity: u32) -> u32 {
    order::create(stock, quantity)
}

/// Finish a successful payment.
pub fn complete(stock: &Stock, quantity: u32) -> u32 {
    submit(stock, quantity)
}
''',
    'src/api.rs': '''use crate::{checkout, inventory::Stock};

/// Handle a checkout request.
pub fn checkout_handler(stock: &Stock, quantity: u32) -> u32 {
    checkout::submit(stock, quantity)
}

/// Handle a preview request.
pub fn preview_handler(stock: &Stock, quantity: u32) -> u32 {
    crate::cart::preview(stock, quantity)
}
''',
    'src/pricing.rs': '''/// Calculate the subtotal before discounts.
pub fn subtotal(unit_price: u32, quantity: u32) -> u32 {
    unit_price.saturating_mul(quantity)
}
''',
    'tests/inventory.rs': '''use checkout_service::inventory::Stock;

#[test]
fn reserves_available_inventory() {
    let stock = Stock { available: 8 };
    assert_eq!(stock.reserve(3), 3);
}
''',
}
HEAD = dict(BASE)
HEAD['src/inventory.rs'] = BASE['src/inventory.rs'].replace(
    '/// Reserve inventory for the requested quantity.',
    '/// Reserve the full quantity or return no reservation.').replace(
    '        quantity.min(self.available)',
    '        if quantity == 0 || quantity > self.available {\n            return 0;\n        }\n        quantity')
HEAD['src/order.rs'] = BASE['src/order.rs'].replace(
    '    stock.reserve(quantity)',
    '    let reserved = stock.reserve(quantity);\n    if reserved == 0 {\n        return 0;\n    }\n    reserved')


def git(workspace, *args):
    return subprocess.run(['git', '-c', 'core.fsmonitor=false', '-c', 'core.hooksPath=/dev/null', '-c', 'commit.gpgsign=false', *args],
        cwd=workspace, check=True, capture_output=True,
        env={**os.environ, 'GIT_AUTHOR_NAME': 'Semweft Preview', 'GIT_AUTHOR_EMAIL': 'preview@example.invalid',
            'GIT_COMMITTER_NAME': 'Semweft Preview', 'GIT_COMMITTER_EMAIL': 'preview@example.invalid'}).stdout.decode('utf-8')


def demo():
    workspace = BUILD / 'example-workspace'
    workspace.mkdir(parents=True, exist_ok=True)
    if not (workspace / '.git').exists():
        for name, content in BASE.items():
            path = workspace / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content)
        git(workspace, 'init', '-q')
        git(workspace, 'add', '.')
        git(workspace, 'commit', '-qm', 'Initial checkout flow')
    for name, content in HEAD.items():
        (workspace / name).write_text(content)
    return workspace


def export(workspace, destination, example=False):
    binary = Path(os.environ.get('SEMWEFT_BIN', str(ROOT / 'target/debug/semweft')))
    env = {**os.environ, 'SEMWEFT_CACHE_DIR': str(BUILD / 'cache'), 'SEMWEFT_CONFIG_DIR': str(BUILD / 'config')}

    def query(*args):
        completed = subprocess.run([str(binary), '--workspace', str(workspace), '--format', 'json',
                                    '--timeout-ms', '60000', *args], capture_output=True, text=True, env=env, timeout=65)
        try:
            result = json.loads(completed.stdout)
        except json.JSONDecodeError:
            raise RuntimeError(completed.stderr or completed.stdout or 'Semweft did not return JSON')
        if completed.returncode or 'error' in result:
            raise RuntimeError(result.get('error', result))
        return result

    def paged(*args, fields):
        result = query(*args)
        page = result
        seen = set()
        while cursor := page.get('output', {}).get('continuation', {}).get('next_cursor'):
            if cursor in seen:
                raise RuntimeError('Repeated continuation cursor')
            seen.add(cursor)
            page = query('--cursor', cursor, *args)
            if page.get('snapshot_id') != result.get('snapshot_id') or page.get('epochs') != result.get('epochs'):
                raise RuntimeError('Snapshot changed between pages')
            for field in fields:
                old, new = result, page
                keys = field.split('.')
                for key in keys[:-1]:
                    old, new = old.setdefault(key, {}), new.get(key, {})
                old.setdefault(keys[-1], []).extend(new.get(keys[-1], []))
            result['output'] = page['output']
        result['captured_pages'] = len(seen) + 1
        return result

    query('index', '--syntax-only')
    symbols = paged('search', '::', '--limit', '1000', fields=['symbols'])
    snapshot = symbols['snapshot_id']
    epoch = str(symbols['epochs']['semantic'])
    pinned = ('--snapshot-id', snapshot, '--semantic-epoch', epoch)
    structure = paged('map', '--depth', '10', '--max-symbols-per-module', '0', '--limit', '1000', *pinned, fields=['modules', 'dependencies', 'entries'])
    module_map = paged('map', '--depth', '10', '--dependencies', 'module', '--max-symbols-per-module', '0', '--limit', '1000', *pinned, fields=['modules', 'dependencies', 'entries'])
    status = query('status', '--snapshot-id', snapshot)
    impact_error = None
    try:
        impact = paged('impact', '--diff', 'WORKTREE', '--depth', '5', '--max-nodes', '5000', '--limit', '1000', fields=['relations', 'relation_changes.added', 'relation_changes.removed'])
    except RuntimeError as error:
        impact_error = str(error)
        available_symbols = symbols.get('symbols', [])
        impact = paged('impact', available_symbols[0]['symbol_id'], '--depth', '5', '--max-nodes', '5000', '--limit', '1000', *pinned, fields=['relations']) if available_symbols else symbols
    sources = {}
    base_sources = {}
    try:
        repository_prefix = git(workspace, 'rev-parse', '--show-prefix').rstrip('\n')
    except subprocess.CalledProcessError:
        repository_prefix = ''
    paths = {module['path'] for module in structure.get('modules', [])}
    paths.update(relation['location']['file'] for relation in impact.get('relations', []))
    for change in impact.get('changes', []):
        for side in ('base_symbol', 'head_symbol'):
            if change.get(side):
                paths.add(change[side]['location']['file'])
    paths.update(symbol['location']['file'] for symbol in symbols.get('symbols', []))
    paths.update(structure.get('omitted_modules', []))
    for path in sorted(paths):
        full = (workspace / path).resolve()
        if not full.is_relative_to(workspace):
            continue
        if full.is_file():
            sources[path] = full.read_bytes().decode('utf-8')
        try:
            base_sources[path] = git(workspace, 'show', f'HEAD:{repository_prefix}{path}')
        except subprocess.CalledProcessError:
            pass  # No captured Base source; never substitute Head text.
    after = query('search', '::', '--limit', '1')
    head_id = impact.get('snapshots', {}).get('head', {}).get('snapshot_id', snapshot)
    if after['snapshot_id'] != snapshot or head_id != snapshot:
        raise RuntimeError('Workspace inputs changed while capturing the preview. Refresh to capture a new fixed view.')
    # All captured symbol spans refer to this exported input.
    base_id = impact.get('snapshots', {}).get('base', {}).get('snapshot_id', '')
    binding = {'head': snapshot, 'head_semantic_epoch': epoch, 'base': base_id,
               'snapshots': impact.get('snapshots')}
    key = hashlib.sha256(json.dumps(binding, sort_keys=True).encode()).hexdigest()
    churn = {}
    git_error = None
    try:
        history = subprocess.run(['git', '-c', 'core.fsmonitor=false', 'log', '-n', '200', '--since=90 days ago', '--format=', '--numstat', '--relative', '-z', '--no-renames', '--', '.'], cwd=workspace, capture_output=True, check=True, timeout=20).stdout.decode('utf-8')
        for record in history.split('\0'):
            fields = record.lstrip('\n').split('\t', 2)
            if len(fields) != 3 or fields[2] not in sources:
                continue
            added, deleted, path = fields
            if not added.isdigit() or not deleted.isdigit():
                continue
            item = churn.setdefault(path, {'added': 0, 'deleted': 0, 'touches': 0})
            item['added'] += int(added)
            item['deleted'] += int(deleted)
            item['touches'] += 1
    except (subprocess.SubprocessError, UnicodeDecodeError) as error:
        git_error = str(error)
    try:
        branch = git(workspace, 'branch', '--show-current').strip() or None
    except subprocess.CalledProcessError:
        branch = None
    payload = {
        'name': 'checkout-service' if example else workspace.name,
        'example': example, 'workspace': str(workspace),
        'generated_at': datetime.datetime.now().astimezone().isoformat(timespec='seconds'),
        'review_key': key, 'symbols': symbols, 'map': structure, 'module_map': module_map, 'status': status,
        'churn': churn, 'git_error': git_error, 'churn_scope': '最近 90 天、最多 200 条提交；按当前捕获路径统计，未追踪重命名',
        'impact': impact, 'impact_error': impact_error, 'sources': sources, 'base_sources': base_sources,
        'branch': branch,
    }
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = destination.with_suffix(destination.suffix + '.tmp')
    temporary.write_text(json.dumps(payload, ensure_ascii=False, indent=2) + '\n')
    temporary.replace(destination)
    print(json.dumps({'workspace': str(workspace), 'symbols': len(symbols.get('symbols', [])),
                      'relations': len(impact.get('relations', [])), 'output': str(destination)}, ensure_ascii=False))


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--workspace', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    export(args.workspace.resolve() if args.workspace else demo(), args.output.resolve(), example=args.workspace is None)
