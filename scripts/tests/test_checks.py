import contextlib
import importlib.util
import io
from pathlib import Path
import subprocess
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('checks', Path(__file__).parents[1] / 'checks.py')
checks = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checks)


class ReviewGate(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        self.git('init', '-q')
        self.git('config', 'user.name', 'Fixture')
        self.git('config', 'user.email', 'fixture@example.invalid')
        self.write('scripts/checks.py', 'original control')
        self.write('crates/callboard/tests/auth.rs', 'original test')
        self.write('README.md', 'original docs')
        self.commit()
        self.base = self.git('rev-parse', 'HEAD').strip()

    def git(self, *args):
        return subprocess.check_output(['git', '-C', str(self.repo), *args], stderr=subprocess.PIPE).decode()

    def write(self, name, text):
        path = self.repo / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def commit(self):
        self.git('add', '.')
        self.git('commit', '-qm', 'fixture')

    def review(self):
        with contextlib.redirect_stdout(io.StringIO()):
            return checks.review(self.repo, self.base)

    def assert_blocked(self):
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(RuntimeError):
            checks.review(self.repo, self.base)

    def test_docs_only_needs_no_control_approval(self):
        self.write('README.md', 'new docs')
        self.commit()
        self.review()

    def test_gui_and_cli_changes_need_no_control_approval(self):
        self.write('crates/callboard-gui/src/app.rs', 'new gui')
        self.write('crates/callboard/src/board_cli.rs', 'new cli')
        self.write('crates/callboard-core/src/store.rs', 'new query')
        self.commit()
        self.review()

    def test_new_service_module_is_protected(self):
        self.write('crates/callboard/src/new_module.rs', 'new runtime code')
        self.commit()
        self.assert_blocked()

    def test_control_edits_cannot_be_approved_by_a_digest(self):
        self.write('scripts/checks.py', 'changed control')
        self.commit()
        self.assert_blocked()
        with self.assertRaises(TypeError):
            checks.review(self.repo, self.base, 'caller-supplied-approval')
        with self.assertRaises(RuntimeError):
            self.review()

    def test_deleted_and_renamed_tests_require_review(self):
        self.git('mv', 'crates/callboard/tests/auth.rs', 'renamed.rs')
        self.commit()
        self.assert_blocked()

    def test_new_workflow_and_test_are_protected(self):
        self.write('.github/workflows/new.yml', 'new workflow')
        self.write('crates/callboard/tests/new.rs', 'new test')
        self.commit()
        self.assert_blocked()

    def test_dirty_and_untracked_files_fail_even_with_prior_approval(self):
        self.write('scripts/checks.py', 'changed control')
        self.commit()
        self.assert_blocked()
        self.write('untracked.txt', 'not reviewed')
        with self.assertRaises(RuntimeError):
            self.review()
        self.git('add', '.')
        with self.assertRaises(RuntimeError):
            self.review()

    def test_invalid_base_does_not_pass(self):
        self.base = 'missing-ref'
        with self.assertRaises(subprocess.CalledProcessError):
            self.review()


if __name__ == '__main__':
    unittest.main()
