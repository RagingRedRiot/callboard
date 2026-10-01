#!/usr/bin/env python3
"""Local merge checks. For merge review, execute this file from a trusted base."""
import argparse
import json
from pathlib import Path, PurePosixPath
import subprocess
import sys


def git(repo, *args):
    return subprocess.check_output(['git', '-C', str(repo), *args], stderr=subprocess.PIPE)


# The gate guards security and core stability, not ordinary feature work.
# Service runtime is protected by default (new modules included) except the
# CLI front end, which only builds requests. GUI code, store queries, layouts,
# and docs pass without review. Keep in sync with .github/workflows/controls.yml;
# scripts/tests/test_hosted_controls.py checks that the two agree.
CONTROL_PREFIXES = ('.github/', '.cargo/', 'scripts/')
GUARDED_PREFIXES = ('crates/callboard/src/', 'crates/callboard/tests/', 'crates/callboard-core/migrations/')
CLI_FRONT_END = {'crates/callboard/src/main.rs', 'crates/callboard/src/board_cli.rs', 'crates/callboard/src/view_cli.rs'}
BUILD_NAMES = {'Cargo.toml', 'Cargo.lock', 'build.rs', 'rust-toolchain', 'rust-toolchain.toml'}
GUARDED_FILES = {
    '.gitignore',
    'crates/callboard-core/src/feed.rs',
    'crates/callboard-core/tests/feed.rs',
    'crates/callboard-core/tests/audit.rs',
}


def protected(path):
    return (
        path.startswith(CONTROL_PREFIXES)
        or (path.startswith(GUARDED_PREFIXES) and path not in CLI_FRONT_END)
        or PurePosixPath(path).name in BUILD_NAMES
        or path in GUARDED_FILES
    )


def review(repo, base):
    # No external diff drivers, textconv, rename heuristics, or ignored errors.
    base = git(repo, 'rev-parse', '--verify', base + '^{commit}').decode().strip()
    head = git(repo, 'rev-parse', '--verify', 'HEAD^{commit}').decode().strip()
    subprocess.run(['git', '-C', str(repo), 'merge-base', '--is-ancestor', base, head], check=True)
    if git(repo, 'status', '--porcelain=v1', '--untracked-files=all'):
        raise RuntimeError('Merge checks require a clean committed tree, including untracked files.')
    names = git(repo, 'diff', '--no-ext-diff', '--no-textconv', '--no-renames', '--name-only', '-z', base, head)
    touched = [p for p in names.decode().split('\0') if p and protected(p)]
    if touched:
        print('Control changes (additions, deletions, and both sides of renames count):', flush=True)
        for name in touched:
            print('  ' + json.dumps(name), flush=True)
        raise RuntimeError('Protected checks or controls differ from the trusted base. Local merge mode has no approval override; use the maintainer-gated hosted control workflow for intentional updates.')
    return base, head


def run(repo, command):
    print('+ ' + ' '.join(map(str, command)), flush=True)
    subprocess.run(command, cwd=repo, check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo', type=Path, default=Path.cwd())
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument('--local', action='store_true', help='development/initial bootstrap, not a merge approval')
    mode.add_argument('--base', help='trusted, reviewed base commit; use its copy of this runner')
    args = parser.parse_args()
    repo = args.repo.resolve()
    baseline = review(repo, args.base) if args.base else None
    if args.local:
        print('DEVELOPMENT CHECKS: this does not approve a merge.', flush=True)
    run(repo, [sys.executable, '-m', 'unittest', 'discover', '-s', 'scripts/tests', '-v'])
    run(repo, ['bash', '-n', 'scripts/security/run.sh'])
    run(repo, ['bash', '-n', 'scripts/security/users.sh'])
    run(repo, ['cargo', 'fmt', '--all', '--', '--check'])
    run(repo, ['cargo', 'clippy', '--workspace', '--all-targets', '--locked', '--', '-D', 'warnings'])
    run(repo, ['cargo', 'test', '--workspace', '--locked'])
    run(repo, ['cargo', 'build', '-p', 'callboard', '--locked'])
    metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--locked', '--no-deps', '--format-version=1'], cwd=repo))
    run(repo, ['bash', 'scripts/security/run.sh', str(Path(metadata['target_directory']) / 'debug/callboard')])
    if baseline and review(repo, args.base) != baseline:
        raise RuntimeError('Source changed during checks; rerun against the reviewed commit.')
    print('PASS: merge checks' if baseline else 'PASS: development checks (merge review still required)', flush=True)


if __name__ == '__main__':
    try:
        main()
    except (RuntimeError, subprocess.CalledProcessError, OSError) as exc:
        print(f'FAIL: {exc}', file=sys.stderr)
        sys.exit(1)
