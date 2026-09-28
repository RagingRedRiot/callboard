"""Exercise the exact policy embedded in the base-only workflow, without GitHub."""
import copy
from pathlib import Path
import textwrap
import unittest

workflow = Path(__file__).parents[2] / '.github/workflows/controls.yml'
source = workflow.read_text().split("          python3 - <<'PY'\n", 1)[1].rsplit('          PY', 1)[0]
policy = {'__name__': 'policy_under_test'}
exec(compile(textwrap.dedent(source), str(workflow), 'exec'), policy)


class HostedControls(unittest.TestCase):
    def setUp(self):
        self.pr = {'head': {'sha': 'head-a'}, 'base': {'sha': 'base-a'}, 'changed_files': 1,
                   'labels': [{'name': 'reviewed-controls'}]}
        self.event = {'action': 'labeled', 'label': {'name': 'reviewed-controls'},
                      'pull_request': copy.deepcopy(self.pr)}
        self.files = [{'filename': 'scripts/security/users.sh'}]

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
        for file in ({'filename': 'unprotected.rs', 'previous_filename': 'crates/example/tests/auth.rs'},
                     {'filename': '.github/workflows/replace.yml'}):
            self.files = [file]
            with self.assertRaises(RuntimeError):
                self.evaluate()

    def test_incomplete_file_list_fails_closed(self):
        self.pr['changed_files'] = 2
        with self.assertRaises(RuntimeError):
            self.evaluate()

    def test_plain_code_or_docs_change_can_pass_without_review_label(self):
        self.files = [{'filename': 'README.md'}]
        self.event['action'] = 'synchronize'
        self.assertEqual(self.evaluate(permission=None), [])

    def test_no_checkout_and_only_read_permissions_in_control_workflow(self):
        text = workflow.read_text()
        self.assertNotIn('uses:', text)
        self.assertNotIn('write\n', text)
        self.assertIn('pull_request_target:', text)
        self.assertNotIn('git checkout', source)
