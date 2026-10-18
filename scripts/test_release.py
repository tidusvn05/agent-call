"""Release safety tests. All registry writes and Cargo publication are mocked."""
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import tomllib
import unittest
from unittest.mock import patch
import urllib.error

spec = importlib.util.spec_from_file_location('release', Path(__file__).with_name('release.py'))
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


def package(name, dependencies=(), version='1.2.3', publish=None):
    return dict(name=name, id=name, version=version, publish=publish,
                dependencies=[dict(name=n, kind=kind) for n, kind in dependencies])


def meta(*packages):
    return dict(packages=list(packages), workspace_members=[p['id'] for p in packages])


class OrderTests(unittest.TestCase):
    def test_dev_build_and_normal_dependencies_and_nonpublishable(self):
        data = meta(package('app', [('core', None), ('fixtures', 'dev'), ('build', 'build')]),
                    package('core'), package('fixtures', [('core', None)]), package('build'),
                    package('private', publish=[]), package('other-registry', publish=['private']))
        with patch.object(release, 'metadata', return_value=data):
            order, version = release.publish_order()
        self.assertEqual(order, ['build', 'core', 'fixtures', 'app'])
        self.assertEqual(version, '1.2.3')

    def test_dev_dependency_cycle_fails(self):
        with patch.object(release, 'metadata', return_value=meta(
                package('a', [('b', 'dev')]), package('b', [('a', None)]))):
            with self.assertRaisesRegex(SystemExit, 'dependency cycle'):
                release.publish_order()

    def test_version_mismatch_and_empty_workspace_fail(self):
        for data, error in [(meta(package('a'), package('b', version='1.2.4')), 'different versions'),
                            (meta(package('a', publish=[])), 'no crates.io')]:
            with patch.object(release, 'metadata', return_value=data):
                with self.assertRaisesRegex(SystemExit, error):
                    release.publish_order()


class VersionTests(unittest.TestCase):
    def test_versions_aliases_inheritance_targets_and_bin_tables(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            a = root / 'a'; a.mkdir()
            b = root / 'b'; b.mkdir()
            (root / 'Cargo.toml').write_text('''[workspace]
members = ["a", "b"]
[workspace.package]
version = "1.2.3"
[workspace.dependencies]
renamed = { version = "=1.2.3", package = "a", path = "a" }
''')
            (a / 'Cargo.toml').write_text('''[package]
name = "a"
version.workspace = true
[[bin]]
name = "tool"
path = "main.rs"
''')
            (b / 'Cargo.toml').write_text('''[package]
name = "b"
version = "1.2.3"
[dependencies]
renamed = { workspace = true }
a = { path = "../a" }
external = "9"
[dev-dependencies.a]
path = "../a"
[target.'cfg(unix)'.build-dependencies]
alias = { package = "a", version = "1.2.3", path = "../a" }
''')
            data = meta(package('a'), package('b'))
            for p in data['packages']:
                p['manifest_path'] = str(root / p['name'] / 'Cargo.toml')
            with patch.object(release, 'ROOT', root), patch.object(release, 'metadata', return_value=data), \
                    patch.object(release.subprocess, 'run') as run:
                release.set_version('2.0.0-rc.1')
            manifest = tomllib.loads((b / 'Cargo.toml').read_text())
            self.assertEqual(manifest['package']['version'], '2.0.0-rc.1')
            self.assertEqual(manifest['dependencies']['a']['version'], '2.0.0-rc.1')
            self.assertEqual(manifest['dev-dependencies']['a']['version'], '2.0.0-rc.1')
            self.assertEqual(manifest['target']['cfg(unix)']['build-dependencies']['alias']['version'], '2.0.0-rc.1')
            self.assertEqual(manifest['dependencies']['renamed'], {'workspace': True})
            self.assertEqual(manifest['dependencies']['external'], '9')
            self.assertEqual(tomllib.loads((a / 'Cargo.toml').read_text())['package']['version'], {'workspace': True})
            shared = tomllib.loads((root / 'Cargo.toml').read_text())['workspace']
            self.assertEqual(shared['package']['version'], '2.0.0-rc.1')
            self.assertEqual(shared['dependencies']['renamed']['version'], '2.0.0-rc.1')
            run.assert_called_once_with(['cargo', 'update', '--workspace', '--quiet'], cwd=root, check=True)

    def test_invalid_versions_do_not_mutate(self):
        for version in ['01.2.3', '1.2', 'v1.2.3', '1.2.3; echo nope', '1.2.3-rc.01']:
            with self.assertRaises(SystemExit), patch.object(release, 'metadata') as metadata:
                release.set_version(version)
            metadata.assert_not_called()


class PublishingTests(unittest.TestCase):
    def setUp(self):
        self.order = patch.object(release, 'publish_order', return_value=(['a', 'b'], '1.2.3'))
        self.order.start()
        self.addCleanup(self.order.stop)

    def test_dry_run_uses_workspace_and_never_touches_registry(self):
        with patch.object(release.subprocess, 'run') as run, patch.object(release, 'index_versions') as index:
            release.publish(True)
        run.assert_called_once_with(['cargo', 'publish', '--workspace', '--dry-run', '--locked', '--allow-dirty'],
                                    cwd=release.ROOT, check=True)
        index.assert_not_called()

    def test_new_crate_stops_before_any_publication(self):
        with patch.object(release, 'index_versions', side_effect=[{'1.0.0'}, None]), \
                patch.object(release.subprocess, 'run') as run:
            with self.assertRaisesRegex(SystemExit, 'New crates: b.*cannot create'):
                release.publish(False)
        run.assert_not_called()

    def test_skip_published_and_retry_rate_limited_publication(self):
        limited = subprocess.CompletedProcess([], 1, '', 'HTTP 429 Too Many Requests')
        success = subprocess.CompletedProcess([], 0, '', '')
        with patch.object(release, 'index_versions', side_effect=[{'1.2.3'}, {'1.0.0'}]), \
                patch.object(release.subprocess, 'run', side_effect=[limited, success]) as run, \
                patch.object(release.time, 'sleep') as sleep:
            release.publish(False)
        self.assertEqual(run.call_count, 2)
        self.assertEqual(run.call_args.args[0][-1], 'b')
        sleep.assert_called_once_with(600)

    def test_prerelease_and_publish_failure_are_errors(self):
        with patch.object(release, 'publish_order', return_value=(['a'], '1.2.3-rc.1')):
            with self.assertRaisesRegex(SystemExit, 'pre-release'):
                release.publish(False)
        with patch.object(release, 'index_versions', return_value={'1.0.0'}), \
                patch.object(release.subprocess, 'run', return_value=subprocess.CompletedProcess([], 1, '', 'denied')):
            with self.assertRaisesRegex(SystemExit, 'publishing a failed'):
                release.publish(False)

    def test_pending_outputs_missing_and_new_crates(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / 'output'
            with patch.dict(os.environ, GITHUB_OUTPUT=str(output)), \
                    patch.object(release, 'index_versions', side_effect=[{'1.2.3'}, None]):
                release.pending()
            self.assertEqual(output.read_text(), 'count=1\nnew=b\n')


class RegistryTests(unittest.TestCase):
    def test_index_404_and_yanked_versions(self):
        error = urllib.error.HTTPError('url', 404, 'missing', {}, io.BytesIO())
        with patch.object(release, 'open_retry', side_effect=error):
            self.assertIsNone(release.index_versions('a'))
        with patch.object(release, 'open_retry') as request:
            request.return_value.__enter__.return_value.read.return_value = b'{"vers":"1.0.0","yanked":true}\n'
            self.assertEqual(release.index_versions('abc'), {'1.0.0'})
            self.assertTrue(request.call_args.args[0].full_url.endswith('/3/a/abc'))

    def test_api_retry_after_and_fatal_errors(self):
        error = urllib.error.HTTPError('url', 429, 'limited', {'Retry-After': '5'}, io.BytesIO())
        with patch.object(release.urllib.request, 'urlopen', side_effect=[error, 'response']), \
                patch.object(release.time, 'sleep') as sleep:
            self.assertEqual(release.open_retry('url'), 'response')
        sleep.assert_called_once_with(5)
        error = urllib.error.HTTPError('url', 403, 'denied', {}, io.BytesIO())
        with patch.object(release.urllib.request, 'urlopen', side_effect=error):
            with self.assertRaises(urllib.error.HTTPError):
                release.open_retry('url')

    def test_cargo_login_credentials_and_idempotent_trustpub_only(self):
        with tempfile.TemporaryDirectory() as tmp:
            Path(tmp, 'credentials.toml').write_text('[registry]\ntoken = "test-only-placeholder"\n')
            with patch.dict(os.environ, CARGO_HOME=tmp):
                self.assertEqual(release.cargo_token(), 'test-only-placeholder')
        existing = {'github_configs': [dict(repository_owner='Owner', repository_name='Repo',
                                            workflow_filename='release.yml', environment='crates-io')]}
        with patch.object(release, 'cargo_token', return_value='placeholder'), \
                patch.object(release, 'crates_io', side_effect=[existing, {}]) as api:
            release.trustpub(['owner/repo', '--crates', 'a', '--only'])
        self.assertEqual([c.args[0] for c in api.call_args_list], ['GET', 'PATCH'])
        self.assertEqual(api.call_args.args[3], {'crate': {'trustpub_only': True}})

    def test_trustpub_creates_matching_repo_workflow_and_environment(self):
        with patch.object(release, 'cargo_token', return_value='placeholder'), \
                patch.object(release, 'crates_io', side_effect=[{}, {}, {}]) as api:
            release.trustpub(['owner/repo', '--crates', 'a', '--only'])
        self.assertEqual(api.call_args_list[1].args[3]['github_config'], dict(
            crate='a', repository_owner='owner', repository_name='repo',
            workflow_filename='release.yml', environment='crates-io'))
        self.assertEqual([c.args[0] for c in api.call_args_list], ['GET', 'POST', 'PATCH'])


if __name__ == '__main__':
    unittest.main()
