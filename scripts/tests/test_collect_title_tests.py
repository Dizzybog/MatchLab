"""Ensure diagnostic collection cannot manufacture unconditional truth labels."""
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest

MODULE = Path(__file__).resolve().parents[1] / 'diagnostics' / 'collect_title_tests.py'
SPEC = importlib.util.spec_from_file_location('collect_title_tests', MODULE)
collector = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = collector
SPEC.loader.exec_module(collector)


class SourceExpectationTests(unittest.TestCase):
    def read(self, source):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / 'fixture.rs'
            path.write_text(source)
            reader = collector.Reader(root)
            reader.file(path)
            return reader.payload()

    def test_literal_table_and_nested_field_labels_keep_source_values(self):
        result = self.read('''
            #[test] fn named_parts() {
                for (raw, episode) in [("Show - 12", 12), ("Another - 09", 9)] {
                    let parsed = parse_title(raw, false).unwrap();
                    let hint = &parsed["anime_episode_candidate"];
                    assert_eq!(hint["episode"], json!(episode));
                    assert_eq!(hint["verified"], false);
                    assert!(parsed.get("anime_title_verified").is_none());
                }
            }
        ''')
        self.assertEqual([row['title'] for row in result['cases']], ['Show - 12', 'Another - 09'])
        self.assertEqual(result['cases'][1]['expected'], [
            {'path': ['anime_episode_candidate', 'episode'], 'op': 'eq', 'value': 9},
            {'path': ['anime_episode_candidate', 'verified'], 'op': 'eq', 'value': False},
            {'path': ['anime_title_verified'], 'op': 'absent'},
        ])

    def test_unknown_conditional_branches_do_not_become_definite_labels(self):
        result = self.read('''
            #[test] fn ambiguous() {
                let parsed = parse_title("Show 27", false).unwrap();
                if let Some(hint) = parsed.get("anime_episode_candidate") {
                    assert_eq!(hint["episode"], 27);
                } else {
                    assert_eq!(parsed["episodes"], json!([27]));
                }
                match unknown {
                    _ => assert_eq!(parsed["episodes"], json!([99])),
                }
                assert!(parsed.get("anime_title_verified").is_none());
            }
        ''')
        self.assertEqual(result['cases'][0]['expected'], [
            {'path': ['anime_title_verified'], 'op': 'absent'},
        ])

    def test_unicode_dynamic_inputs_expand_and_unknown_format_stays_unknown(self):
        result = self.read(r'''
            #[test] fn whitespace() {
                for whitespace in ["\u{00a0}", "\u{000b}"] {
                    let raw = format!("Example{whitespace}Show");
                    let parsed = parse_title(&format!("{raw}.S01E02"), false).unwrap();
                    assert_eq!(parsed["title"], "Example Show");
                }
                let unsupported = parse_title(&format!("{unknown}.S01E02"), false).unwrap();
                assert_eq!(unsupported["title"], "Impossible label");
            }
        ''')
        self.assertEqual([row['title'] for row in result['cases']], [
            'Example\u00a0Show.S01E02', 'Example\u000bShow.S01E02',
        ])
        self.assertEqual(result['summary']['scored_assertions'], 2)

    def test_unsupported_expected_expression_is_not_serialized_as_truth(self):
        result = self.read('''
            #[test] fn unknown() {
                let parsed = parse_title("Show S01E02", false).unwrap();
                assert_eq!(parsed["episodes"], json!([unknown()]));
            }
        ''')
        self.assertEqual(result['cases'][0]['expected'], [])
        json.dumps(result)

    def test_release_generator_changes_fail_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'release_300.rs'
            path.write_text('fn variants() { changed_source(); }')
            with self.assertRaisesRegex(ValueError, 'changed'):
                collector.verify_variant_source(path)


class TraceObservationTests(unittest.TestCase):
    def row(self, title, observed, **overrides):
        return {'kind': 'parse_title', 'title': title, 'translate_languages': False,
                'thread': 'source_test', 'caller_file': 'fixture.rs', 'caller_line': 4,
                'parser_mode': 'live', 'revision': 0, 'observed': observed,
                'error': None, **overrides}

    def summarize(self, rows):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'trace.jsonl'
            path.write_text(''.join(json.dumps(row) + '\n' for row in rows))
            return collector.summarize_trace(path, {'Curated'})

    def test_replay_and_unstable_outputs_are_not_public_parity_baselines(self):
        data = self.summarize([
            self.row('Stable', {'title': 'Stable'}),
            self.row('Curated', {'title': 'Curated'}),
            self.row('Unstable', {'title': 'one'}),
            self.row('Unstable', {'title': 'two'}),
            self.row('Replay', {'title': 'learned'}, parser_mode='replay'),
            self.row('Translated', {'languages': ['English']}, translate_languages=True),
            self.row('', {'title': ''}),
        ])
        self.assertEqual([row['request']['title'] for row in data['title-parity-corpus.json']['cases']],
                         ['Curated', 'Stable'])
        self.assertEqual([row['request']['title'] for row in data['title-parity-new-only.json']['cases']],
                         ['Stable'])
        self.assertEqual(data['trace-summary.json']['unstable_public_baseline_inputs'], 1)
        self.assertIn('never independent', data['title-parity-corpus.json']['cases'][0]['expected_provenance'])


if __name__ == '__main__':
    unittest.main()
