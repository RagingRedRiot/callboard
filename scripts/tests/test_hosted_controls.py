"""Exercise the exact policy embedded in the base-only workflow, without GitHub."""
import copy
import importlib.util
from pathlib import Path
import textwrap
import unittest

workflow = Path(__file__).parents[2] / '.github/workflows/controls.yml'
source = workflow.read_text().split("          python3 - <<'PY'\n", 1)[1].rsplit('          PY', 1)[0]
policy = {'__name__': 'policy_under_test'}
exec(compile(textwrap.dedent(source), str(workflow), 'exec'), policy)


spec = importlib.util.spec_from_file_location('checks', Path(__file__).parents[1] / 'checks.py')
checks = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checks)

GUARDED = [
    '.github/workflows/ci.yml', '.cargo/config.toml', 'scripts/security/run.sh', '.gitignore',
    'Cargo.toml', 'Cargo.lock', 'crates/callboard-gui/Cargo.toml', 'crates/callboard/build.rs',
    'rust-toolchain.toml', 'crates/callboard/src/lifecycle.rs', 'crates/callboard/src/server.rs',
    'crates/callboard/src/upgrade.rs', 'crates/callboard/src/new_module.rs',
    'crates/callboard-core/src/feed.rs', 'crates/callboard-core/tests/feed.rs',
    'crates/callboard-core/tests/audit.rs', 'crates/callboard-core/migrations/0010_new.sql',
]
UNGUARDED = [
    'README.md', 'DESIGN.md', 'docs/merge-checks.md', 'crates/callboard-gui/src/app.rs',
    'crates/callboard-gui/src/feed.rs', 'crates/callboard-gui/tests/autostart.rs',
    'crates/callboard/src/main.rs', 'crates/callboard/src/board_cli.rs',
    'crates/callboard/src/view_cli.rs', 'crates/callboard-core/src/store.rs',
    'crates/callboard-core/src/layout.rs', 'crates/callboard-core/tests/colors.rs',
    'crates/callboard-core/tests/store.rs', 'crates/other/tests/auth.rs',
]


APPEND_ONLY = ['crates/callboard/src/routes/feeds.rs', 'crates/callboard/src/routes/new_feature.rs',
               'crates/callboard/tests/routes.rs', 'crates/callboard/tests/service.rs',
               'crates/callboard/tests/new.rs']
CHANGES = ('modified', 'removed', 'renamed', 'copied', 'changed')


class Scope(unittest.TestCase):
    def both(self, path, status):
        hosted, local = policy['protected'](path, status), checks.protected(path, status)
        self.assertEqual(hosted, local, (path, status))
        return hosted

    def test_security_and_stability_paths_are_guarded_in_both_copies(self):
        for path in GUARDED:
            for status in ('added', *CHANGES):
                self.assertTrue(self.both(path, status), (path, status))

    def test_gui_cli_store_and_docs_pass_in_both_copies(self):
        for path in UNGUARDED:
            for status in ('added', *CHANGES):
                self.assertFalse(self.both(path, status), (path, status))

    def test_routes_and_service_tests_are_append_only(self):
        for path in APPEND_ONLY:
            self.assertFalse(self.both(path, 'added'), path)
            for status in CHANGES:
                self.assertTrue(self.both(path, status), (path, status))

    def test_renaming_an_existing_route_away_is_guarded(self):
        self.pr['changed_files'] = 1
        event = {'action': 'synchronize', 'pull_request': copy.deepcopy(self.pr)}
        files = [{'filename': 'crates/callboard/src/routes/elsewhere.rs', 'status': 'renamed',
                  'previous_filename': 'crates/callboard/src/routes/feeds.rs'}]
        with self.assertRaises(RuntimeError):
            policy['evaluate'](event, self.pr, files, None, '1')
        files = [{'filename': 'crates/callboard/src/routes/new_feature.rs', 'status': 'added'}]
        self.assertEqual(policy['evaluate'](event, self.pr, files, None, '1'), [])

    def setUp(self):
        self.pr = {'head': {'sha': 'h'}, 'base': {'sha': 'b'}, 'changed_files': 1, 'labels': []}

    def test_both_copies_define_the_same_scope(self):
        for name in ('CONTROL_PREFIXES', 'APPEND_ONLY_PREFIXES', 'GUARDED_PREFIXES', 'CLI_FRONT_END',
                     'BUILD_NAMES', 'GUARDED_FILES'):
            self.assertEqual(policy[name], getattr(checks, name), name)


class HostedControls(unittest.TestCase):
    def setUp(self):
        self.pr = {'head': {'sha': 'head-a'}, 'base': {'sha': 'base-a'}, 'changed_files': 1,
                   'labels': [{'name': 'reviewed-controls'}]}
        self.event = {'action': 'labeled', 'label': {'name': 'reviewed-controls'},
                      'pull_request': copy.deepcopy(self.pr)}
        self.files = [{'filename': 'scripts/security/users.sh', 'status': 'modified'}]

    def evaluate(self, permission='write', attempt='1'):
        return policy['evaluate'](self.event, self.pr, self.files, permission, attempt)

    def test_write_authorized_label_event_can_approve_current_controls(self):
        self.assertEqual(self.evaluate(), ['scripts/security/users.sh'])

    def test_untrusted_actor_cannot_approve(self):
        for permission in (None, 'read', 'triage'):
            with self.assertRaises(RuntimeError):
                self.evaluate(permission=permission)

    def test_sticky_label_and_rerun_do_not_approve(self):
        for action in ('synchronize', 'opened', 'reopened', 'unlabeled', 'edited'):
            self.event['action'] = action
            with self.assertRaises(RuntimeError):
                self.evaluate()
        self.event['action'] = 'labeled'
        with self.assertRaises(RuntimeError):
            self.evaluate(attempt='2')

    def test_old_event_cannot_approve_new_head_or_base(self):
        for side in ('head', 'base'):
            self.pr[side]['sha'] += '-new'
            with self.assertRaises(RuntimeError):
                self.evaluate()
            self.pr[side]['sha'] = self.event['pull_request'][side]['sha']

    def test_removed_label_cannot_approve(self):
        self.pr['labels'] = []
        with self.assertRaises(RuntimeError):
            self.evaluate()

    def test_rename_checks_old_path_and_workflow_additions_are_protected(self):
        self.event['action'] = 'synchronize'
        for file in ({'filename': 'unprotected.rs', 'previous_filename': 'crates/callboard/tests/auth.rs', 'status': 'renamed'},
                     {'filename': '.github/workflows/replace.yml', 'status': 'added'}):
            self.files = [file]
            with self.assertRaises(RuntimeError):
                self.evaluate()

    def test_incomplete_file_list_fails_closed(self):
        self.pr['changed_files'] = 2
        with self.assertRaises(RuntimeError):
            self.evaluate()

    def test_plain_code_or_docs_change_can_pass_without_review_label(self):
        self.files = [{'filename': 'README.md', 'status': 'modified'}]
        self.event['action'] = 'synchronize'
        self.assertEqual(self.evaluate(permission=None), [])

    def test_no_checkout_and_only_read_permissions_in_control_workflow(self):
        text = workflow.read_text()
        self.assertNotIn('uses:', text)
        self.assertNotIn('write\n', text)
        self.assertIn('pull_request_target:', text)
        self.assertNotIn('git checkout', source)
