# Local merge checks

Run the whole development workflow from the repository root:

```sh
python3 scripts/checks.py --local
```

Requires Linux, Python 3, Bash, a Rust toolchain with rustfmt/Clippy, and a working
Docker engine. It runs review-gate self-tests, shell syntax checks, formatting,
Clippy with warnings denied, all Rust tests, a fresh locked build, and the
container UID tests. Every step must pass; missing Docker, timeouts, build
failures, skipped prerequisites, and test failures are failures. Rust operations
use Cargo.lock. No systemd service is installed and no host users are created.

The Docker fixture uses a digest-pinned Ubuntu 24.04 base and installs Python
while building its image. Its runtime is offline, read-only except for tmpfs,
resource-limited, and not privileged. Only the freshly built executable is
mounted from the host, read-only; no checkout, credentials, or Docker socket is
mounted. Root in the container arranges fixture permissions and changes UIDs.
Candidate services and probes run with real/effective UIDs 10001 and 10002,
no capabilities, and no-new-privileges. The host executable must be compatible
with the container's architecture and glibc (Ubuntu 24.04/glibc 2.39); an ABI
mismatch fails rather than skipping tests. Use an Ubuntu 24.04 development/CI
environment when a different host cannot build a compatible executable.

Each UID takes a turn owning the service. Tests assert:

1. The owner can submit and read a feed through the production service.
2. The other UID cannot connect through the normal 0700 directory/0600 socket.
3. After the fixture deliberately relaxes only socket access permissions, the
   other UID **can connect** but receives no HTTP response for either a read or
   a write, even with forged identity headers. This exercises the server's
   kernel peer-UID check independently of filesystem protection and client code.
4. The owner can still read the unchanged feed afterward. A crashed service,
   timeout, or failed connection cannot masquerade as successful UID rejection.
5. Permissions are restored and normal owner CLI access and shutdown work.

To run just that test while developing, first build the current binary:

```sh
cargo build --locked -p callboard
bash scripts/security/run.sh "$PWD/target/debug/callboard"
```

## Trust the base, not the proposed checks

This follows the control-review work in stethoscope-mcp (decision 46) and relay
(commit `73d0727`): a candidate must not be able to redefine what passing means,
and an old approval must not cover a later revision. Relay's hardened workflow
was found in its Git history; the current relay main checkout does not contain it.

For a merge, use a clean, committed candidate tree and extract the runner from a
**trusted, already reviewed base commit**. Do not use the candidate's runner as
the authority for approving its own control changes:

```sh
# Set this explicitly to the full SHA of the trusted merge base.
base=REVIEWED_BASE_COMMIT_SHA
runner=$(mktemp)
git show "$base:scripts/checks.py" > "$runner"
python3 "$runner" --repo "$PWD" --base "$base"
```

If controls changed, the runner lists them and fails before executing candidate
checks. There is **no local acknowledgement or digest override**. Unchanged
protected files are identical to the trusted base, and Git cleanliness and the
base/head revisions are checked again after running the suite. Changes to the
control definitions need the hosted maintainer-review path described below.
The `--local` command is for development and bootstrap, never merge approval.

Protected paths include `.github/`, `.cargo/`, `scripts/`, every `tests/`
directory, runtime code under `crates/callboard/src/`, feed validation, Cargo
manifests/lockfiles, build scripts, toolchain files, and `.gitignore`. Additions,
removals, and both sides of renames are covered. Git errors fail closed.

## Base-branch GitHub gate

`.github/workflows/controls.yml` implements the stethoscope/relay mechanism:

- Runs on `pull_request_target`, using the **base branch's definition**. It does
  not check out the PR or execute any candidate script, test, or build step.
- Reads changed-file metadata using a read-only token, checking old and new
  names. API failures and incomplete/capped file lists fail the job.
- Changes to controls fail unless the triggering event applies
  `reviewed-controls` and the applying actor has write/maintain/admin permission.
- The event's base and head must match the current PR. Reruns, stale labels,
  withdrawn labels, and newer revisions cannot reuse the approval. The PR is
  queried again before passing to detect pushes during evaluation.

This means contributors can **propose** changed tests but cannot approve their
use. A trusted maintainer can deliberately approve an exact revision after
reading it, matching the other projects. This is not a claim that every Rust
test is always copied from the base: ordinary CI runs the candidate suite, while
the separate base-owned gate prevents untrusted changes to its definitions
from being accepted for merge.

`.github/workflows/ci.yml` runs the local suite with a read-only token and
checkout credentials disabled, followed by `merge checks complete`. The latter
fails for failed, cancelled, skipped, or missing jobs. No path filters suppress
checks.

Policy tests exercise the exact Python policy embedded in the control workflow,
including unauthorized approvals, stale revisions, reruns, removed labels,
renames, and missing files. The UID/container suite remains mandatory.
