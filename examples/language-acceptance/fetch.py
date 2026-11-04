import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile


def git(root, *arguments):
    result = subprocess.run(
        ["git", "-c", "core.hooksPath=/dev/null", "-C", str(root), *arguments],
        env={**os.environ, "GIT_TERMINAL_PROMPT": "0"},
        check=True,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=120,
    )
    return result.stdout.strip()


def verify(root, repository):
    if git(root, "rev-parse", "HEAD") != repository["revision"]:
        raise ValueError(f"revision mismatch: {repository['id']}")
    for name, expected in repository["files"].items():
        path = root / name
        if path.is_symlink() or not path.resolve().is_relative_to(root.resolve()):
            raise ValueError(f"source escapes repository: {name}")
        if git(root, "hash-object", "--no-filters", name) != expected:
            raise ValueError(f"source blob mismatch: {repository['id']}/{name}")


def fetch(destination, repository):
    if not re.fullmatch(r"[a-zA-Z0-9_-]+", repository["id"]):
        raise ValueError("invalid repository id")
    if not re.fullmatch(r"https://github\.com/[a-zA-Z0-9_.-]+/[a-zA-Z0-9_.-]+", repository["repository"]):
        raise ValueError("expected a public GitHub repository URL")
    if not re.fullmatch(r"[0-9a-f]{40}", repository["revision"]):
        raise ValueError("expected an immutable commit id")
    target = destination / repository["id"]
    if target.exists():
        verify(target, repository)
        return f"Verified {repository['id']}"
    with tempfile.TemporaryDirectory(prefix="fetch-", dir=destination) as temporary:
        scratch = Path(temporary) / "repository"
        scratch.mkdir()
        git(scratch, "init", "--quiet", "--template=")
        git(scratch, "remote", "add", "origin", repository["repository"] + ".git")
        git(scratch, "fetch", "--quiet", "--depth=1", "origin", repository["revision"])
        git(scratch, "checkout", "--quiet", "--detach", "FETCH_HEAD")
        verify(scratch, repository)
        scratch.rename(target)
    return f"Fetched {repository['id']} at {repository['revision']}"


def main():
    parser = argparse.ArgumentParser(description="Fetch pinned acceptance sources without running repository scripts")
    parser.add_argument("--destination", required=True, type=Path)
    arguments = parser.parse_args()
    corpus = json.loads(Path(__file__).with_name("corpus.json").read_text())
    if corpus["schema_version"] != "0.1":
        raise ValueError("unsupported corpus schema")
    destination = arguments.destination.resolve()
    destination.mkdir(parents=True, exist_ok=True)
    with concurrent.futures.ThreadPoolExecutor(max_workers=3) as executor:
        futures = [executor.submit(fetch, destination, repository) for repository in corpus["repositories"]]
        failures = 0
        for repository, future in zip(corpus["repositories"], futures):
            try:
                print(future.result(), flush=True)
            except Exception as error:
                failures += 1
                print(f"Failed {repository['id']}: {error}", flush=True)
        if failures:
            raise SystemExit(1)


if __name__ == "__main__":
    main()
