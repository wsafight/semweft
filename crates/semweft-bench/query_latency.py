#!/usr/bin/env python3
"""Measure cold indexing and warm CLI query latency in an isolated syntax-only index."""
import argparse
import datetime
import json
import math
import os
from pathlib import Path
import platform
import statistics
import subprocess
import sys
import time


def measure(binary, workspace, symbol, output, samples=100, commands=('search', 'refs', 'calls', 'impact'), pinned=False):
    if samples < 5:
        raise ValueError('At least five observations are required; use 100 or more for percentile measurement.')
    workspace = workspace.resolve(strict=True)
    binary = binary.resolve(strict=True)
    output = output.absolute()
    output.parent.mkdir(parents=True, exist_ok=True)
    runtime = output.with_suffix('.runtime')
    runtime.mkdir(mode=0o700, exist_ok=False)
    config = runtime / 'benchmark.toml'
    config.write_text('schema_version="0.1"\n[workspace]\nroot=' + json.dumps(str(workspace)) + '\n[index]\ndirectory=' + json.dumps(str(runtime / 'index')) + '\n')
    environment = {**os.environ, 'SEMWEFT_CACHE_DIR': str(runtime / 'cache'), 'SEMWEFT_CONFIG_DIR': str(runtime / 'config'), 'SEMWEFT_QUERY_TIMING': '1'}
    base = [str(binary), '--workspace', str(workspace), '--config', str(config), '--format', 'json', '--timeout-ms', '60000']

    def run(arguments, allow_error=False):
        started = time.perf_counter()
        process = subprocess.run(base + arguments, env=environment, capture_output=True, timeout=180)
        elapsed = (time.perf_counter() - started) * 1000
        try:
            value = json.loads(process.stdout)
        except ValueError as error:
            raise RuntimeError(process.stderr.decode(errors='replace')) from error
        if not allow_error and (process.returncode != 0 or 'error' in value):
            raise RuntimeError(json.dumps(value, ensure_ascii=False))
        return elapsed, value, len(process.stdout)

    result = {
        'schema_version': '0.1', 'measured_at': datetime.datetime.now(datetime.timezone.utc).isoformat(),
        'workspace': str(workspace), 'binary': str(binary), 'platform': platform.platform(), 'machine': platform.machine(),
        'mode': 'syntax_only', 'binding': 'fixed_snapshot' if pinned else 'latest_verified_inputs',
        'samples_per_query': samples, 'percentile_method': 'nearest_rank' if samples >= 100 else 'insufficient_samples_for_acceptance',
        'measurement': 'CLI wall time including process startup, IPC and stdout transfer; first page; warm filesystem cache after indexing',
        'budgets': {'query_cache_bytes': 67108864, 'cursor_cache_bytes': 67108864}, 'requested_symbol': symbol,
        'runtime': str(runtime), 'queries': {},
    }
    def save():
        temporary = output.with_suffix('.next.json')
        temporary.write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
        temporary.replace(output)

    try:
        elapsed, indexed, _ = run(['index', '--syntax-only'])
        result['cold_index_ms'] = elapsed
        print(f'{workspace.name}: cold index {elapsed:.1f}ms', flush=True)
        _, resolved, _ = run(['symbol', symbol])
        target = resolved['symbol']
        result['target'] = {key: target.get(key) for key in ('symbol_id', 'qualified_name', 'kind', 'location')}
        binding = ['--snapshot-id', indexed['snapshot_id'], '--semantic-epoch', str(indexed['epochs']['semantic'])] if pinned else []
        for command in commands:
            query_target = target['qualified_name'].rsplit('::', 1)[-1] if command == 'search' else target['symbol_id']
            arguments = binding + [command, query_target]
            run(arguments)
            values = []
            observed = []
            for index in range(samples):
                elapsed, value, size = run(arguments)
                if value.get('snapshot_id') != indexed['snapshot_id'] or value.get('epochs', {}).get('semantic') != indexed['epochs']['semantic']:
                    raise RuntimeError('Analysis inputs changed during measurement; retain this failed run and start a new run.')
                values.append(elapsed)
                observed.append({'snapshot_id': value.get('snapshot_id'), 'semantic_epoch': value.get('epochs', {}).get('semantic'),
                    'relations': len(value.get('relations', [])), 'nodes': len(value.get('nodes', [])),
                    'symbols': len(value.get('symbols', [])), 'output_bytes': size, 'truncated': value.get('output', {}).get('truncated')})
                if (index + 1) % 10 == 0:
                    print(f'{workspace.name}: {command} {index + 1}/{samples}', flush=True)
            ordered = sorted(values)
            result['queries'][command] = {'samples_ms': values, 'min_ms': min(values), 'median_ms': statistics.median(values),
                'max_ms': max(values), 'p95_ms': ordered[math.ceil(0.95 * samples) - 1] if samples >= 100 else None,
                'observations': observed, 'arguments': arguments}
            print(f'{workspace.name}: {command} median={statistics.median(values):.1f}ms p95={result["queries"][command]["p95_ms"]}', flush=True)
            save()
        _, status, _ = run(['status'])
        result['storage'] = status.get('storage')
        pid = status.get('clients', {}).get('daemon_pid')
        if pid and sys.platform != 'win32':
            process = subprocess.run(['ps', '-o', 'rss=', '-p', str(pid)], capture_output=True, text=True, check=True)
            result['daemon_rss_bytes'] = int(process.stdout.strip()) * 1024 if process.stdout.strip() else None
        result['daemon_pid'] = pid
        save()
    finally:
        try:
            run(['__stop'], allow_error=True)
        except (OSError, RuntimeError, subprocess.TimeoutExpired):
            pass
    return result


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--workspace', type=Path, required=True)
    parser.add_argument('--symbol', required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--samples', type=int, default=100)
    parser.add_argument('--commands', nargs='+', choices=['search', 'refs', 'calls', 'impact'], default=['search', 'refs', 'calls', 'impact'])
    parser.add_argument('--pinned', action='store_true')
    arguments = parser.parse_args()
    try:
        measure(arguments.binary, arguments.workspace, arguments.symbol, arguments.output, arguments.samples, arguments.commands, arguments.pinned)
    except (OSError, ValueError, RuntimeError, subprocess.TimeoutExpired) as error:
        parser.exit(1, str(error) + '\n')
