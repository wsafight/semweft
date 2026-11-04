import argparse
import concurrent.futures
import json
import os
import pathlib
import platform
import signal
import subprocess
import tempfile
import time


def observe_startup(mode, deadline_seconds):
    with tempfile.TemporaryDirectory(prefix="semweft-shell-start-") as directory:
        root = pathlib.Path(directory)
        script = root / "producer.sh"
        marker = root / "entered"
        script.write_text("#!/bin/sh\nprintf entered > entered\nsleep 30\n")
        script.chmod(0o700)
        command = [str(script)] if mode == "direct_script" else ["/bin/sh", str(script)]
        with tempfile.TemporaryFile() as errors:
            started = time.monotonic()
            child = subprocess.Popen(
                command,
                cwd=root,
                process_group=0,
                stdout=subprocess.DEVNULL,
                stderr=errors,
            )
            spawned = time.monotonic()
            try:
                deadline = spawned + deadline_seconds
                while (
                    not marker.exists()
                    and time.monotonic() < deadline
                    and child.poll() is None
                ):
                    time.sleep(0.01)
                ready = marker.exists()
                elapsed = time.monotonic() - spawned
                exited = child.poll()
            finally:
                try:
                    os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                child.kill()
                child.wait(timeout=10)
            errors.seek(0)
            return {
                "mode": mode,
                "ready": ready,
                "exit_before_cleanup": exited,
                "spawn_ms": round((spawned - started) * 1000, 3),
                "entry_wait_ms": round(elapsed * 1000, 3),
                "stderr": errors.read().decode(errors="replace"),
            }


def positive_integer(value):
    parsed = int(value)
    if parsed < 1:
        raise argparse.ArgumentTypeError("must be positive")
    return parsed


def main():
    parser = argparse.ArgumentParser(description="Compare direct script and interpreter startup.")
    parser.add_argument("--jobs", type=positive_integer, default=32)
    parser.add_argument("--rounds", type=positive_integer, default=2)
    parser.add_argument("--deadline-seconds", type=positive_integer, default=3)
    parser.add_argument("--output", type=pathlib.Path)
    args = parser.parse_args()
    if os.name != "posix":
        parser.error("requires a POSIX host and /bin/sh; Python 3.11 or newer")
    results = []
    for iteration in range(args.rounds):
        modes = ["direct_script", "explicit_interpreter"]
        if iteration % 2:
            modes.reverse()
        for mode in modes:
            with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as workers:
                observations = list(
                    workers.map(
                        lambda _: observe_startup(mode, args.deadline_seconds),
                        range(args.jobs),
                    )
                )
            for observation in observations:
                observation["round"] = iteration + 1
            results.extend(observations)
    summary = {}
    for mode in ["direct_script", "explicit_interpreter"]:
        observations = [result for result in results if result["mode"] == mode]
        summary[mode] = {
            "runs": len(observations),
            "ready": sum(result["ready"] for result in observations),
            "not_ready": sum(not result["ready"] for result in observations),
            "max_spawn_ms": max(result["spawn_ms"] for result in observations),
            "max_entry_wait_ms": max(result["entry_wait_ms"] for result in observations),
        }
    report = {
        "schema_version": "0.1",
        "platform": platform.platform(),
        "python": platform.python_version(),
        "cpu_count": os.cpu_count(),
        "jobs": args.jobs,
        "rounds": args.rounds,
        "deadline_seconds": args.deadline_seconds,
        "summary": summary,
        "observations": results,
    }
    rendered = json.dumps(report, indent=2) + "\n"
    if args.output:
        args.output.write_text(rendered)
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
