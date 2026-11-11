#!/usr/bin/env python3
"""Resolve validated community suggestions into bounded native Rust replay plans.

A rule never supplies executable code or chooses a conversion/handler option.
Every planned addition copies an exact handler in the immutable catalogue.
Ambiguous mappings remain review-only, never guessed.
"""
import argparse
import json
import re
from pathlib import Path
from community_rule_intake import (
    TV_NATIVE_PATTERNS, canonical, technical_pattern, valid_extended_rule,
    valid_rule,
)

SUPPORTED = {
    "learned_field_regex", "learned_tv_regex", "learned_source_regex_patch",
    "learned_corrective_regex", "learned_general_regex_review",
    "learned_field_regex_review", "historical_regex_review",
    "verified_regex_proposal",
}
TV_FIELDS = ("seasons", "episodes")
STATIC = re.compile(r"(?:value|uniq_concat\(value)\('([^']{1,48})'\)\)?\Z")
GENERAL_PREFIX = r"(?i)(?:^|[ ._-])("
GENERAL_SUFFIX = r")(?:$|[ ._-])"


def native_entry(handlers, index, field=None):
    if type(index) is not int or not 0 <= index < len(handlers):
        return None
    row = handlers[index]
    if row.get("kind") != "regex" or not isinstance(row.get("pattern"), str):
        return None
    if field is not None and row.get("name") != field:
        return None
    return row


def operation(handlers, index, new_pattern, mode):
    row = native_entry(handlers, index)
    if row is None or not isinstance(new_pattern, str) or not 4 <= len(new_pattern) <= 512:
        raise ValueError("Invalid native regex source or replacement")
    if mode not in ("add", "patch", "correct"):
        raise ValueError("Invalid native regex operation")
    return {
        "operation": mode, "handler_index": index,
        "field": row["name"], "original_pattern": row["pattern"],
        "pattern": new_pattern, "transform": row.get("transform"),
    }


def anchored_general(value):
    if not isinstance(value, str) or not (
        value.startswith(GENERAL_PREFIX) and value.endswith(GENERAL_SUFFIX)
        and 20 <= len(value) <= 256
    ):
        return False
    parts = value[len(GENERAL_PREFIX):-len(GENERAL_SUFFIX)].split("[ ._-]+")
    return 2 <= len(parts) <= 6 and all(
        re.fullmatch(r"[A-Za-z0-9]{1,24}", token) for token in parts)


def field_template(handlers, field, canonical_value):
    if isinstance(canonical_value, str):
        accepted = {"value('" + canonical_value + "')",
                    "uniq_concat(value('" + canonical_value + "'))"}
    elif (isinstance(canonical_value, list) and len(canonical_value) == 1
          and isinstance(canonical_value[0], str)):
        accepted = {"uniq_concat(value('" + canonical_value[0] + "'))"}
    elif canonical_value is True:
        accepted = {"boolean"}
    else:
        return None
    matches = [
        i for i, row in enumerate(handlers)
        if native_entry(handlers, i, field) and row.get("transform") in accepted
    ]
    if field == "extension" and isinstance(canonical_value, str):
        matches = [
            i for i, row in enumerate(handlers)
            if native_entry(handlers, i, field) and row.get("transform") == "lowercase"
        ]
    # Deliberately select the first trusted handler with the EXACT desired
    # output transform. Different options are still replayed and checked.
    return matches[0] if matches else None


def tv_template(handlers, field, is_range=False):
    transform = "range_func" if is_range else "array(integer)"
    for index, row in enumerate(handlers):
        opts = row.get("options") or {}
        if (native_entry(handlers, index, field) and row.get("transform") == transform
                and opts.get("remove") is False
                and opts.get("skipIfAlreadyFound") is True):
            return index
    return None


def resolve(rule, handlers):
    """Return (native_operations, reason); empty operations never stage a PR."""
    if not isinstance(rule, dict) or not isinstance(rule.get("kind"), str):
        return [], "not_a_valid_proposal"
    kind = rule["kind"]
    if kind not in SUPPORTED:
        return [], "unknown_proposal_family"
    if kind == "historical_regex_review":
        return [], "historical_source_requires_current_grammar_and_new_proof"
    if kind == "learned_source_regex_patch":
        field = rule.get("field")
        if field not in {"codec", "resolution", "bit_depth"}:
            return [], "unsafe_source_patch_field"
        index = rule.get("handler_index")
        row = native_entry(handlers, index, field)
        extra = technical_pattern(field, rule.get("signal"))
        if (row is None or extra is None or
            row.get("pattern") != rule.get("original_regex") or
            row.get("transform") != rule.get("transform")):
            return [], "stale_or_invalid_source_handler"
        proposed = "(?:" + row["pattern"] + ")|(?:" + extra + ")"
        if rule.get("proposed_regex") != proposed or len(proposed) > 512:
            return [], "invalid_native_source_replacement"
        return [operation(handlers, index, proposed, "patch")], "replay_ready"

    if kind == "learned_field_regex":
        field, signal = rule.get("field"), rule.get("signal")
        pattern = technical_pattern(field, signal)
        if pattern is None or rule.get("regex") != pattern:
            return [], "unsupported_field_regex_grammar"
        idx = field_template(handlers, field, rule.get("canonical"))
        if idx is None:
            return [], "no_unambiguous_matching_native_transform"
        # Extension regexes must operate on actual filename suffixes, not
        # on arbitrary title words that happen to equal 'mp4' or 'mkv'.
        if field == "extension":
            if not isinstance(signal, str) or not re.fullmatch(r"[a-z0-9]{2,8}", signal):
                return [], "invalid_filename_extension_marker"
            pattern = r"(?i)\." + re.escape(signal) + r"$"
        return [operation(handlers, idx, pattern, "add")], "replay_ready"

    if kind == "learned_tv_regex":
        family = rule.get("family")
        patterns = TV_NATIVE_PATTERNS.get(family)
        if patterns is None or rule.get("native_patterns") != patterns:
            return [], "invalid_tv_family"
        changes = []
        for field in TV_FIELDS:
            idx = tv_template(handlers, field)
            if idx is None:
                return [], "missing_safe_native_tv_handler"
            changes.append(operation(handlers, idx, patterns[field], "add"))
        return changes, "replay_ready"

    if kind == "learned_corrective_regex":
        index, field = rule.get("handler_index"), rule.get("field")
        source = native_entry(handlers, index, field)
        family = rule.get("family")
        if source is None or field not in TV_FIELDS:
            return [], "stale_corrective_handler_index"
        options = source.get("options") or {}
        if options.get("remove") is not False or options.get("skipIfAlreadyFound") is not True:
            return [], "unsafe_corrective_handler_options"
        if family == "sxe_range":
            pattern = (r"(?i)(?:^|[._ -])s[0-9]{1,3}[._ -]*e([0-9]{1,4}[._ -]*-[._ -]*(?:e(?:p)?)?[._ -]*[0-9]{1,4})(?:$|[._ -])")
            valid = field == "episodes" and source.get("transform") == "range_func"
        else:
            pattern = TV_NATIVE_PATTERNS.get(family, {}).get(field)
            valid = source.get("transform") == "array(integer)"
        if not valid or not isinstance(pattern, str) or rule.get("regex") != pattern:
            return [], "invalid_corrective_coordinate_grammar"
        return [operation(handlers, index, pattern, "correct")], "replay_ready"

    if kind == "learned_general_regex_review":
        index, field = rule.get("handler_index"), rule.get("field")
        row = native_entry(handlers, index, field)
        pattern = rule.get("regex")
        if row is None:
            return [], "stale_general_handler_index"
        if not anchored_general(pattern):
            return [], "invalid_separator_grammar"
        if row.get("pattern") == pattern:
            return [], "already_supported"
        if rule.get("mode") == "add":
            return [operation(handlers, index, pattern, "add")], "replay_ready"
        if rule.get("mode") == "correct":
            transform = row.get("transform", "")
            if not (transform == "boolean" or
                    (transform.startswith("value('") and "$1" not in transform) or
                    transform.startswith("uniq_concat(value('")):
                return [], "corrective_rule_has_dynamic_transform"
            merged = "(?:" + row["pattern"] + ")|(?:" + pattern + ")"
            if len(merged) > 512:
                return [], "replacement_exceeds_safe_limit"
            return [operation(handlers, index, merged, "patch")], "replay_ready"
        return [], "unknown_general_mode"

    if kind == "learned_field_regex_review":
        field, signal = rule.get("field"), rule.get("signal")
        if not isinstance(signal, str) or not re.fullmatch("[a-z0-9]{3,16}", signal):
            return [], "invalid_field_discovery_marker"
        indices = [i for i, row in enumerate(handlers)
                   if native_entry(handlers, i, field)
                   and row.get("transform") not in ("none", "")]
        transformations = {handlers[i]["transform"] for i in indices}
        if len(transformations) != 1:
            return [], "ambiguous_native_transform_requires_explicit_source_identity"
        if rule.get("regex") != r"(?i)(?:^|[ ._-])" + signal + r"(?:$|[ ._-])":
            return [], "invalid_field_discovery_grammar"
        return [operation(handlers, indices[0], rule["regex"], "add")], "replay_ready"

    if kind == "verified_regex_proposal":
        marker = rule.get("marker")
        pattern = canonical(marker)
        if pattern is None or rule.get("regex") != pattern:
            return [], "invalid_verified_episode_marker"
        idx = tv_template(handlers, "episodes")
        if idx is None:
            return [], "missing_native_episode_handler"
        return [operation(handlers, idx, pattern, "add")], "replay_ready"
    return [], "not_replayable"


def candidate_valid(rule):
    if not isinstance(rule, dict):
        return False
    source = rule.get("source_build")
    if not isinstance(source, str) or ":" not in source:
        return False
    sha = source.split(":", 1)[0]
    # The intake independently verifies ancestry against current main.
    # Here revalidate syntax, schema, and fingerprint against source build.
    if rule.get("kind") == "verified_regex_proposal":
        return valid_rule(rule, sha) is not None
    return valid_extended_rule(rule, sha, ancestor=lambda *_: True) is not None


def build(candidates, handlers):
    if not isinstance(candidates, list) or len(candidates) > 24:
        raise ValueError("Invalid review candidate batch")
    plans = {}
    for rule in candidates:
        fp = rule.get("fingerprint") if isinstance(rule,dict) else None
        if not isinstance(fp,str) or re.fullmatch("[0-9a-f]{64}",fp) is None:
            raise ValueError("Invalid review candidate fingerprint")
        if fp in plans:
            raise ValueError("Duplicate rule fingerprint in review batch")
        if not candidate_valid(rule):
            # Old queued proposals can legitimately become stale as main
            # changes. One invalid item must not abort review of valid rules.
            # Keep its fingerprint/decision visible; never execute its regex.
            plans[fp] = {"source_changes": [],
                         "reason": "stale_or_invalid_queued_proposal"}
            continue
        changes, reason = resolve(rule, handlers)
        plans[fp] = {"source_changes": changes, "reason": reason}
    return plans


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--candidates", required=True, type=Path)
    p.add_argument("--handlers", required=True, type=Path)
    p.add_argument("--out", required=True, type=Path)
    args = p.parse_args()
    rules = json.loads(args.candidates.read_text(encoding="utf-8"))
    handlers = json.loads(args.handlers.read_text(encoding="utf-8"))["handlers"]
    plans = build(rules, handlers)
    args.out.write_text(json.dumps(plans, ensure_ascii=False, indent=2) + "\n",
                        encoding="utf-8")
    print("community_replay_planned=" + str(sum(
        bool(plan["source_changes"]) for plan in plans.values()
    )) + " total=" + str(len(plans)))


if __name__ == "__main__":
    main()
