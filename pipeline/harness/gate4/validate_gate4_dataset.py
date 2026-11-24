#!/usr/bin/env python3
"""Validate Gate 4 SLM command code spec, GBNF grammar, and 120-utterance dataset (v1.2).

Uses only the Python standard library and parses schema/slm/override.gbnf directly
(no hardcoded regex duplicate) to verify grammar soundness, character-level parity
with docs/specs/03-routing-and-two-layer-control.md §4.1, expected_codes.json
alignment, v1.2 safety invariants, and JSONL/TSV parity.
"""

from __future__ import annotations

import argparse
import csv
import itertools
import json
import pathlib
import re
import sys
from dataclasses import dataclass
from typing import Any


REPO_ROOT = pathlib.Path(__file__).resolve().parents[3]
SCHEMA_PATH = REPO_ROOT / "schema" / "slm" / "expected_codes.json"
GBNF_PATH = REPO_ROOT / "schema" / "slm" / "override.gbnf"
SPEC_03_PATH = REPO_ROOT / "docs" / "specs" / "03-routing-and-two-layer-control.md"
LEGACY_SCHEMA_PATH = REPO_ROOT / "schema" / "slm-command-code.v1.json"
LEGACY_GBNF_PATH = (
    REPO_ROOT / "android" / "native" / "inference" / "grammars" / "driving_command_v1.gbnf"
)
JSONL_PATH = REPO_ROOT / "pipeline" / "harness" / "gate4" / "utterances_ja_120.jsonl"
TSV_PATH = REPO_ROOT / "pipeline" / "harness" / "gate4" / "utterances_ja_120.tsv"

EXPECTED_CATEGORY_COUNTS = {
    "clear_allowed": 40,
    "ambiguous_colloquial": 30,
    "negation_cancel": 20,
    "out_of_contract": 20,
    "in_cabin_noise": 10,
}

ALLOWED_NOISE_PROFILES = {
    "quiet_cabin",
    "highway_road_noise",
    "rain_wipers",
    "ac_fan_high",
    "passenger_chatter",
    "tunnel_echo",
    "radio_bleed",
}

ALLOWED_EXPECTED_EFFECTS = {
    "override_preview",
    "s12_review_proposal",
    "confirm_offer",
    "reconfirm_offer",
    "reject_offer",
    "cancel_offer",
    "undo_recent_change",
    "speak_reason_template",
    "bookmark_history",
    "noop",
}

TSV_COLUMNS = [
    "id",
    "category",
    "dispatch_path",
    "expected_code",
    "acceptable_codes",
    "expected_effect",
    "pending_offer",
    "last_junction",
    "next_junction",
    "trip_mode",
    "noise_profile",
    "safety_critical",
    "prohibited_reason",
    "utterance_ja",
    "notes",
]


# ---------------------------------------------------------------------------
# Minimal, faithful GBNF parser and full-string recognizer
# Supports: comments (#), rules (name ::= expr), alternation (|),
# concatenation, grouping ((...)), literals ("..." with escapes),
# rule references, and postfix quantifiers (?, *, +).
# ---------------------------------------------------------------------------

@dataclass(frozen=True)
class LiteralNode:
    value: str


@dataclass(frozen=True)
class RefNode:
    name: str


@dataclass(frozen=True)
class SeqNode:
    items: tuple[Any, ...]


@dataclass(frozen=True)
class AltNode:
    branches: tuple[Any, ...]


@dataclass(frozen=True)
class RepeatNode:
    item: Any
    min_count: int
    max_count: int | None


class GbnfParser:
    def __init__(self, text: str) -> None:
        self.rules: dict[str, Any] = {}
        self._parse_file(text)

    def _parse_file(self, text: str) -> None:
        for line_no, raw_line in enumerate(text.splitlines(), start=1):
            line = self._strip_comment(raw_line).strip()
            if not line:
                continue
            if "::=" not in line:
                raise ValueError(f"GBNF line {line_no}: expected '::=' in {raw_line!r}")
            lhs, rhs = line.split("::=", 1)
            rule_name = lhs.strip()
            if not rule_name:
                raise ValueError(f"GBNF line {line_no}: empty rule name")
            if rule_name in self.rules:
                raise ValueError(f"GBNF line {line_no}: duplicate rule {rule_name!r}")
            self._buf = rhs
            self._pos = 0
            expr = self._parse_alt()
            self._skip_ws()
            if self._pos != len(self._buf):
                raise ValueError(
                    f"GBNF line {line_no}: unexpected trailing token at {self._buf[self._pos:]!r}"
                )
            self.rules[rule_name] = expr

        if "root" not in self.rules:
            raise ValueError("GBNF grammar missing 'root' rule")
        self._check_refs()

    @staticmethod
    def _strip_comment(line: str) -> str:
        in_str = False
        escaped = False
        for i, ch in enumerate(line):
            if escaped:
                escaped = False
                continue
            if ch == "\\" and in_str:
                escaped = True
                continue
            if ch == '"':
                in_str = not in_str
                continue
            if ch == "#" and not in_str:
                return line[:i]
        return line

    def _skip_ws(self) -> None:
        while self._pos < len(self._buf) and self._buf[self._pos] in " \t":
            self._pos += 1

    def _parse_alt(self) -> Any:
        branches = [self._parse_seq()]
        while True:
            self._skip_ws()
            if self._pos < len(self._buf) and self._buf[self._pos] == "|":
                self._pos += 1
                branches.append(self._parse_seq())
            else:
                break
        return branches[0] if len(branches) == 1 else AltNode(tuple(branches))

    def _parse_seq(self) -> Any:
        items = []
        while True:
            self._skip_ws()
            if self._pos >= len(self._buf) or self._buf[self._pos] in "|)":
                break
            items.append(self._parse_postfix())
        if not items:
            raise ValueError(f"GBNF empty sequence at position {self._pos} in {self._buf!r}")
        return items[0] if len(items) == 1 else SeqNode(tuple(items))

    def _parse_postfix(self) -> Any:
        node = self._parse_atom()
        self._skip_ws()
        if self._pos < len(self._buf) and self._buf[self._pos] in "?*+":
            op = self._buf[self._pos]
            self._pos += 1
            if op == "?":
                return RepeatNode(node, 0, 1)
            if op == "*":
                return RepeatNode(node, 0, None)
            if op == "+":
                return RepeatNode(node, 1, None)
        return node

    def _parse_atom(self) -> Any:
        self._skip_ws()
        if self._pos >= len(self._buf):
            raise ValueError("GBNF unexpected end of rule")
        ch = self._buf[self._pos]
        if ch == "(":
            self._pos += 1
            node = self._parse_alt()
            self._skip_ws()
            if self._pos >= len(self._buf) or self._buf[self._pos] != ")":
                raise ValueError(f"GBNF missing closing ')' in {self._buf!r}")
            self._pos += 1
            return node
        if ch == '"':
            return self._parse_literal()
        return self._parse_ident()

    def _parse_literal(self) -> LiteralNode:
        assert self._buf[self._pos] == '"'
        self._pos += 1
        chars: list[str] = []
        while self._pos < len(self._buf):
            ch = self._buf[self._pos]
            self._pos += 1
            if ch == '"':
                return LiteralNode("".join(chars))
            if ch == "\\":
                if self._pos >= len(self._buf):
                    raise ValueError("GBNF unterminated escape")
                esc = self._buf[self._pos]
                self._pos += 1
                mapping = {"n": "\n", "r": "\r", "t": "\t", '"': '"', "\\": "\\"}
                if esc not in mapping:
                    raise ValueError(f"GBNF unsupported escape \\{esc}")
                chars.append(mapping[esc])
            else:
                chars.append(ch)
        raise ValueError("GBNF unterminated string literal")

    def _parse_ident(self) -> RefNode:
        start = self._pos
        while self._pos < len(self._buf) and (
            self._buf[self._pos].isalnum() or self._buf[self._pos] in "-_"
        ):
            self._pos += 1
        if self._pos == start:
            raise ValueError(f"GBNF unexpected char {self._buf[self._pos:]!r}")
        return RefNode(self._buf[start : self._pos])

    def _check_refs(self) -> None:
        def walk(node: Any) -> None:
            if isinstance(node, RefNode):
                if node.name not in self.rules:
                    raise ValueError(f"GBNF undefined rule reference: {node.name!r}")
            elif isinstance(node, SeqNode):
                for item in node.items:
                    walk(item)
            elif isinstance(node, AltNode):
                for branch in node.branches:
                    walk(branch)
            elif isinstance(node, RepeatNode):
                walk(node.item)

        for rule in self.rules.values():
            walk(rule)

    def accepts(self, text: str) -> bool:
        end_positions = self._match(self.rules["root"], text, 0)
        return len(text) in end_positions

    def _match(self, node: Any, text: str, pos: int) -> set[int]:
        if isinstance(node, LiteralNode):
            if text.startswith(node.value, pos):
                return {pos + len(node.value)}
            return set()
        if isinstance(node, RefNode):
            return self._match(self.rules[node.name], text, pos)
        if isinstance(node, SeqNode):
            positions = {pos}
            for item in node.items:
                next_positions: set[int] = set()
                for p in positions:
                    next_positions.update(self._match(item, text, p))
                positions = next_positions
                if not positions:
                    break
            return positions
        if isinstance(node, AltNode):
            results: set[int] = set()
            for branch in node.branches:
                results.update(self._match(branch, text, pos))
            return results
        if isinstance(node, RepeatNode):
            results: set[int] = set()
            current = {pos}
            count = 0
            while current:
                if count >= node.min_count:
                    results.update(current)
                if node.max_count is not None and count >= node.max_count:
                    break
                nxt: set[int] = set()
                for p in current:
                    for q in self._match(node.item, text, p):
                        if q != p:
                            nxt.add(q)
                current = nxt
                count += 1
            return results
        raise TypeError(f"Unknown AST node: {node!r}")


def normalize_japanese_utterance(text: str) -> str:
    cleaned = text.strip().replace("\n", "").replace("\r", "").replace("\t", "")
    trailing_punct = "。、！？!? "
    while cleaned and cleaned[-1] in trailing_punct:
        cleaned = cleaned[:-1]
    return cleaned


def estimate_bpe_tokens(command_with_newline: str) -> int:
    """Conservative upper-bound BPE token estimate for ASCII fixed command lines."""
    text = command_with_newline
    tokens = 0
    if text.endswith("\n"):
        tokens += 1
        text = text[:-1]
    parts = text.split(" ")
    for idx, part in enumerate(parts):
        if idx == 0:
            tokens += 2 if len(part) >= 3 else 1
        elif "=" in part:
            tokens += 3
        else:
            tokens += 2
    return tokens


def verify_single_source_of_truth(gbnf_text: str, schema: dict[str, Any]) -> None:
    if LEGACY_SCHEMA_PATH.exists():
        raise ValueError(
            f"Duplicate legacy schema still exists: {LEGACY_SCHEMA_PATH.relative_to(REPO_ROOT)}"
        )
    if LEGACY_GBNF_PATH.exists():
        legacy_text = LEGACY_GBNF_PATH.read_text(encoding="utf-8")
        if legacy_text != gbnf_text:
            raise ValueError(
                f"Legacy grammar {LEGACY_GBNF_PATH.relative_to(REPO_ROOT)} differs from "
                f"canonical {GBNF_PATH.relative_to(REPO_ROOT)}"
            )
    if schema.get("grammar_path") != "schema/slm/override.gbnf":
        raise ValueError(f"expected_codes.json grammar_path mismatch: {schema.get('grammar_path')!r}")

    # Verify exact character-for-character parity with docs/specs/03 §4.1 when docs exist
    if SPEC_03_PATH.exists():
        spec_text = SPEC_03_PATH.read_text(encoding="utf-8")
        match = re.search(r"```gbnf\n(.*?)```", spec_text, flags=re.DOTALL)
        if not match:
            raise ValueError(f"Could not locate ```gbnf block in {SPEC_03_PATH}")
        spec_gbnf = match.group(1)
        if gbnf_text != spec_gbnf:
            raise ValueError(
                "schema/slm/override.gbnf does not match docs/specs/03 §4.1 character-for-character"
            )


def verify_grammar(grammar: GbnfParser, schema: dict[str, Any]) -> int:
    # 1. All simple commands accepted with trailing '\n', rejected without '\n'
    simple_cmds = [
        cmd for cmd, meta in schema["commands"].items() if not meta["has_parameters"]
    ]
    for cmd in simple_cmds:
        assert grammar.accepts(f"{cmd}\n"), f"Grammar failed to accept simple cmd: {cmd!r}"
        assert not grammar.accepts(cmd), f"Grammar accepted cmd without newline: {cmd!r}"
        assert not grammar.accepts(f"{cmd} rc=H\n"), f"Grammar accepted params on {cmd!r}"

    # 2. Verify dt values strictly T|S (R removed in v1.2)
    ovr_meta = schema["commands"]["OVR"]
    key_order = ovr_meta["canonical_key_order"]
    assert key_order == ["rc", "nr", "mg", "dt", "jp"], f"Unexpected key order: {key_order!r}"
    dt_vals = list(ovr_meta["parameters"]["dt"]["values"].keys())
    assert dt_vals == ["T", "S"], f"dt must be ['T', 'S'] only, got {dt_vals!r}"

    # 3. Exhaustive check: all 31 canonical subsets and all 581 valid sentences MUST be accepted
    accepted_subsets = 0
    total_valid_sentences = len(simple_cmds)
    for r in range(1, len(key_order) + 1):
        for combo in itertools.combinations(key_order, r):
            accepted_subsets += 1
            value_lists = [list(ovr_meta["parameters"][k]["values"].keys()) for k in combo]
            for vals in itertools.product(*value_lists):
                kv_str = " ".join(f"{k}={v}" for k, v in zip(combo, vals))
                line = f"OVR {kv_str}\n"
                assert grammar.accepts(line), f"Grammar rejected valid OVR sentence: {line!r}"
                total_valid_sentences += 1
    assert accepted_subsets == 31, f"Expected 31 subsets, checked {accepted_subsets}"
    assert total_valid_sentences == 581, f"Expected 581 valid sentences, got {total_valid_sentences}"

    # 4. Invalid / adversarial strings MUST all be rejected
    invalid_samples = [
        "",
        "\n",
        "OVR\n",
        "OVR \n",
        "OVR rc=H",  # missing newline
        "OVR rc=H \n",  # trailing space
        "OVR  rc=H\n",  # double space after OVR
        " OVR rc=H\n",  # leading space
        "OVR dt=R\n",  # R removed in v1.2
        "OVR rc=H dt=R\n",  # R removed in v1.2
        "OVR rc=H nr=H mg=H dt=R jp=last\n",  # R removed in v1.2
        "OVR rc=H rc=L\n",  # duplicate key
        "OVR nr=H rc=H\n",  # out of canonical order
        "OVR dt=S mg=H\n",  # out of canonical order
        "OVR jp=next dt=T\n",  # out of canonical order
        "OVR jp=last jp=next\n",  # duplicate jp
        "OVR rc=X\n",  # invalid level
        "OVR rc=h\n",  # lowercase level
        "OVR dt=H\n",  # invalid detour
        "OVR jp=j1\n",  # raw ref ID forbidden
        "OVR jp=prev\n",  # invalid jp value
        "OVR through_ref=a1\n",  # parked-only field forbidden
        "OVR foo=bar\n",  # unknown key
        "OVR rc=H  nr=H\n",  # double space
        "OVR rc=H\nYES\n",  # multi-line
        "NOOP\n\n",  # extra newline
        "OVR rc=H\r\n",  # CRLF
        "CXL WHY\n",  # multiple commands
        "POI conv\n",  # Phase 2 command not in v1
        '{"action":"suggest_route_preference"}\n',  # raw JSON forbidden
        "はい\n",  # natural text forbidden
    ]
    for bad in invalid_samples:
        assert not grammar.accepts(bad), f"Grammar erroneously accepted invalid input: {bad!r}"

    return total_valid_sentences


def build_prefilter_map(schema: dict[str, Any]) -> dict[str, str]:
    mapping: dict[str, str] = {}
    exact_matches = schema["deterministic_prefilter"]["exact_matches"]
    for cmd, phrases in exact_matches.items():
        for phrase in phrases:
            norm = normalize_japanese_utterance(phrase)
            if norm in mapping:
                raise ValueError(f"Duplicate deterministic prefilter phrase: {phrase!r}")
            mapping[norm] = cmd
    return mapping


def load_jsonl(path: pathlib.Path) -> list[dict[str, Any]]:
    records: list[dict[str, Any]] = []
    for line_no, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        if not raw.strip():
            raise ValueError(f"{path.name} line {line_no}: blank line not allowed")
        try:
            records.append(json.loads(raw))
        except json.JSONDecodeError as exc:
            raise ValueError(f"{path.name} line {line_no}: invalid JSON: {exc}") from exc
    return records


def record_to_tsv_row(rec: dict[str, Any]) -> list[str]:
    ctx = rec["context"]
    return [
        rec["id"],
        rec["category"],
        rec["dispatch_path"],
        rec["expected_code"],
        "|".join(rec["acceptable_codes"]),
        rec["expected_effect"],
        ctx["pending_offer"],
        ctx["last_junction"],
        ctx["next_junction"],
        ctx["trip_mode"],
        rec["noise_profile"],
        "true" if rec["safety_critical"] else "false",
        "" if rec["prohibited_reason"] is None else rec["prohibited_reason"],
        rec["utterance_ja"],
        rec["notes"],
    ]


def export_tsv(records: list[dict[str, Any]], path: pathlib.Path) -> None:
    with path.open("w", encoding="utf-8", newline="") as fh:
        writer = csv.writer(fh, delimiter="\t", lineterminator="\n")
        writer.writerow(TSV_COLUMNS)
        for rec in records:
            writer.writerow(record_to_tsv_row(rec))


def verify_tsv_parity(records: list[dict[str, Any]], path: pathlib.Path) -> None:
    if not path.exists():
        raise FileNotFoundError(f"TSV file missing: {path}")
    with path.open("r", encoding="utf-8", newline="") as fh:
        rows = list(csv.reader(fh, delimiter="\t"))
    if not rows or rows[0] != TSV_COLUMNS:
        raise ValueError(f"TSV header mismatch in {path.name}")
    data_rows = rows[1:]
    if len(data_rows) != len(records):
        raise ValueError(f"TSV row count {len(data_rows)} != JSONL count {len(records)}")
    for idx, (rec, tsv_row) in enumerate(zip(records, data_rows), start=1):
        expected_row = record_to_tsv_row(rec)
        if tsv_row != expected_row:
            raise ValueError(
                f"TSV parity mismatch at row {idx} ({rec['id']}): "
                f"got {tsv_row!r}, expected {expected_row!r}"
            )


def _check_expected_effect(
    rec_id: str,
    exp_code: str,
    dispatch: str,
    ctx: dict[str, str],
    effect: str,
) -> None:
    if effect not in ALLOWED_EXPECTED_EFFECTS:
        raise ValueError(f"{rec_id}: invalid expected_effect {effect!r}")

    if exp_code == "OVR jp=last":
        # v1.2 03 §4.1: standalone jp=last never changes current route; creates S12 proposal only
        if effect != "s12_review_proposal":
            raise ValueError(
                f"{rec_id}: standalone 'OVR jp=last' must have expected_effect='s12_review_proposal', "
                f"got {effect!r}"
            )
    elif exp_code.startswith("OVR "):
        if effect != "override_preview":
            raise ValueError(
                f"{rec_id}: OVR with route parameters must have expected_effect='override_preview', "
                f"got {effect!r}"
            )
    elif exp_code == "YES":
        # ADR-0007: only Rust dict_norm confirms route; SLM YES only prompts re-confirmation
        expected = "confirm_offer" if dispatch == "deterministic_rule" else "reconfirm_offer"
        if effect != expected:
            raise ValueError(
                f"{rec_id}: YES with dispatch_path={dispatch!r} must have "
                f"expected_effect={expected!r}, got {effect!r}"
            )
    elif exp_code == "NO":
        if effect != "reject_offer":
            raise ValueError(f"{rec_id}: NO must have expected_effect='reject_offer', got {effect!r}")
    elif exp_code == "CXL":
        expected = "undo_recent_change" if ctx["pending_offer"] == "undo_window" else "cancel_offer"
        if effect != expected:
            raise ValueError(
                f"{rec_id}: CXL with pending_offer={ctx['pending_offer']!r} must have "
                f"expected_effect={expected!r}, got {effect!r}"
            )
    elif exp_code == "WHY":
        if effect != "speak_reason_template":
            raise ValueError(
                f"{rec_id}: WHY must have expected_effect='speak_reason_template', got {effect!r}"
            )
    elif exp_code == "BMK":
        if effect != "bookmark_history":
            raise ValueError(
                f"{rec_id}: BMK must have expected_effect='bookmark_history', got {effect!r}"
            )
    elif exp_code == "NOOP":
        if effect != "noop":
            raise ValueError(f"{rec_id}: NOOP must have expected_effect='noop', got {effect!r}")


def verify_dataset(
    records: list[dict[str, Any]],
    grammar: GbnfParser,
    schema: dict[str, Any],
) -> dict[str, Any]:
    prefilter_map = build_prefilter_map(schema)
    prohibited_reasons = set(schema["prohibited_reasons"])
    max_chars = schema["runtime_budget"]["max_utterance_chars"]
    max_tokens_target = schema["runtime_budget"]["max_grammar_tokens_target"]
    ctx_props = schema["input_context_schema"]["properties"]
    scoring_policy = schema["gate4_scoring_policy"]["categories"]

    if len(records) != 120:
        raise ValueError(f"Expected 120 records, found {len(records)}")

    seen_ids: set[str] = set()
    seen_utterances: set[str] = set()
    category_counts: dict[str, int] = {k: 0 for k in EXPECTED_CATEGORY_COUNTS}
    dispatch_counts: dict[str, int] = {"deterministic_rule": 0, "slm": 0}
    command_counts: dict[str, int] = {}
    effect_counts: dict[str, int] = {}
    max_estimated_tokens = 0
    standalone_jp_last_count = 0
    no_pending_target_noop_count = 0

    for idx, rec in enumerate(records, start=1):
        expected_id = f"G4-{idx:03d}"
        rec_id = rec.get("id")
        if rec_id != expected_id:
            raise ValueError(f"Row {idx}: expected id {expected_id!r}, got {rec_id!r}")
        if rec_id in seen_ids:
            raise ValueError(f"Duplicate id: {rec_id}")
        seen_ids.add(rec_id)

        cat = rec.get("category")
        if cat not in EXPECTED_CATEGORY_COUNTS:
            raise ValueError(f"{rec_id}: invalid category {cat!r}")
        category_counts[cat] += 1

        utt = rec.get("utterance_ja", "")
        if not isinstance(utt, str) or not utt.strip():
            raise ValueError(f"{rec_id}: empty utterance_ja")
        if len(utt) > max_chars:
            raise ValueError(f"{rec_id}: utterance exceeds {max_chars} chars ({len(utt)})")
        norm_utt = normalize_japanese_utterance(utt)
        if norm_utt in seen_utterances:
            raise ValueError(f"{rec_id}: duplicate utterance {utt!r}")
        seen_utterances.add(norm_utt)

        dispatch = rec.get("dispatch_path")
        if dispatch not in dispatch_counts:
            raise ValueError(f"{rec_id}: invalid dispatch_path {dispatch!r}")
        dispatch_counts[dispatch] += 1

        exp_code = rec.get("expected_code")
        acc_codes = rec.get("acceptable_codes")
        if not isinstance(exp_code, str) or not isinstance(acc_codes, list) or not acc_codes:
            raise ValueError(f"{rec_id}: invalid expected_code / acceptable_codes")
        if exp_code not in acc_codes:
            raise ValueError(f"{rec_id}: expected_code {exp_code!r} not in acceptable_codes")
        if len(set(acc_codes)) != len(acc_codes):
            raise ValueError(f"{rec_id}: duplicate entries in acceptable_codes")

        # Validate every code against the ACTUAL parsed GBNF file
        for code in acc_codes:
            line = f"{code}\n"
            if not grammar.accepts(line):
                raise ValueError(f"{rec_id}: code {code!r} rejected by GBNF grammar")
            est_tok = estimate_bpe_tokens(line)
            max_estimated_tokens = max(max_estimated_tokens, est_tok)
            if est_tok > max_tokens_target:
                raise ValueError(
                    f"{rec_id}: code {code!r} estimated {est_tok} tokens > target {max_tokens_target}"
                )

        cmd_head = exp_code.split(" ", 1)[0]
        command_counts[cmd_head] = command_counts.get(cmd_head, 0) + 1

        # Cross-check deterministic prefilter alignment
        prefilter_hit = prefilter_map.get(norm_utt)
        if dispatch == "deterministic_rule":
            if prefilter_hit != exp_code:
                raise ValueError(
                    f"{rec_id}: dispatch_path is deterministic_rule but prefilter returned "
                    f"{prefilter_hit!r} instead of {exp_code!r}"
                )
        else:
            if prefilter_hit is not None:
                raise ValueError(
                    f"{rec_id}: dispatch_path is slm but utterance {utt!r} matches "
                    f"deterministic_prefilter ({prefilter_hit!r})"
                )

        # Validate context flags against schema
        ctx = rec.get("context")
        if not isinstance(ctx, dict) or set(ctx.keys()) != set(ctx_props.keys()):
            raise ValueError(f"{rec_id}: invalid context keys {ctx!r}")
        for k, spec in ctx_props.items():
            if ctx[k] not in spec["enum"]:
                raise ValueError(f"{rec_id}: invalid context[{k!r}] = {ctx[k]!r}")

        # v1.2 pending_offer invariants (pending_offer=none -> YES/NO/CXL must be NOOP)
        pending = ctx["pending_offer"]
        if pending == "none":
            for code in acc_codes:
                if code in {"YES", "NO", "CXL"}:
                    raise ValueError(
                        f"{rec_id}: pending_offer=='none' forbids {code!r} in acceptable_codes"
                    )
            if exp_code == "NOOP" and "NO_PENDING_TARGET" in rec.get("notes", ""):
                no_pending_target_noop_count += 1
        if exp_code in {"YES", "NO"} and pending not in {"avoid", "override_preview"}:
            raise ValueError(
                f"{rec_id}: expects {exp_code!r} but context.pending_offer={pending!r}"
            )
        if exp_code == "CXL" and pending not in {"avoid", "override_preview", "undo_window"}:
            raise ValueError(
                f"{rec_id}: expects 'CXL' but context.pending_offer={pending!r}"
            )

        # Semantic consistency checks on jp=last / jp=next
        if "jp=last" in exp_code and ctx["last_junction"] != "present":
            raise ValueError(f"{rec_id}: expects jp=last but context.last_junction != 'present'")
        if "jp=next" in exp_code and ctx["next_junction"] != "present":
            raise ValueError(f"{rec_id}: expects jp=next but context.next_junction != 'present'")
        if exp_code == "OVR jp=last":
            standalone_jp_last_count += 1

        # Verify expected_effect matches v1.2 semantics
        effect = rec.get("expected_effect")
        if not isinstance(effect, str):
            raise ValueError(f"{rec_id}: missing expected_effect")
        _check_expected_effect(rec_id, exp_code, dispatch, ctx, effect)
        effect_counts[effect] = effect_counts.get(effect, 0) + 1

        # Noise profile check
        noise = rec.get("noise_profile")
        if noise not in ALLOWED_NOISE_PROFILES:
            raise ValueError(f"{rec_id}: invalid noise_profile {noise!r}")
        if cat == "in_cabin_noise" and noise == "quiet_cabin":
            raise ValueError(f"{rec_id}: in_cabin_noise case cannot use quiet_cabin")

        # v1.2 Gate 4 scoring policy conformance (07 §2.3)
        cat_policy = scoring_policy[cat]
        if cat_policy["noop_acceptable"]:
            if "NOOP" not in acc_codes:
                raise ValueError(
                    f"{rec_id}: category {cat!r} requires safety fallback 'NOOP' in acceptable_codes"
                )
        else:
            if "NOOP" in acc_codes:
                raise ValueError(
                    f"{rec_id}: category {cat!r} must not allow 'NOOP' in acceptable_codes"
                )

        # Strict out_of_contract invariants (Gate 4: 0 out-of-contract executions)
        if cat == "out_of_contract":
            if exp_code != "NOOP" or acc_codes != ["NOOP"]:
                raise ValueError(f"{rec_id}: out_of_contract must strictly expect ['NOOP']")
            if rec.get("safety_critical") is not True:
                raise ValueError(f"{rec_id}: out_of_contract must set safety_critical=true")
            if rec.get("prohibited_reason") not in prohibited_reasons:
                raise ValueError(
                    f"{rec_id}: invalid prohibited_reason {rec.get('prohibited_reason')!r}"
                )
        else:
            if rec.get("prohibited_reason") is not None:
                raise ValueError(f"{rec_id}: non-out_of_contract must have prohibited_reason=null")

    if category_counts != EXPECTED_CATEGORY_COUNTS:
        raise ValueError(
            f"Category counts mismatch: got {category_counts}, expected {EXPECTED_CATEGORY_COUNTS}"
        )
    if standalone_jp_last_count < 1:
        raise ValueError("Dataset must include at least one standalone 'OVR jp=last' S12 proposal case")
    if no_pending_target_noop_count < 2:
        raise ValueError(
            "Dataset must include pending_offer='none' NOOP cases covering YES-like and CXL-like utterances"
        )

    return {
        "total": len(records),
        "category_counts": category_counts,
        "dispatch_counts": dispatch_counts,
        "command_counts": dict(sorted(command_counts.items())),
        "effect_counts": dict(sorted(effect_counts.items())),
        "standalone_jp_last_s12_cases": standalone_jp_last_count,
        "no_pending_target_noop_cases": no_pending_target_noop_count,
        "max_estimated_tokens": max_estimated_tokens,
    }


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Validate Gate 4 SLM grammar (v1.2) and 120-utterance dataset."
    )
    parser.add_argument(
        "--export-tsv",
        action="store_true",
        help="Regenerate utterances_ja_120.tsv from utterances_ja_120.jsonl before validating.",
    )
    args = parser.parse_args()

    schema = json.loads(SCHEMA_PATH.read_text(encoding="utf-8"))
    gbnf_text = GBNF_PATH.read_text(encoding="utf-8")
    verify_single_source_of_truth(gbnf_text, schema)

    grammar = GbnfParser(gbnf_text)
    valid_sentences = verify_grammar(grammar, schema)

    records = load_jsonl(JSONL_PATH)
    summary = verify_dataset(records, grammar, schema)
    summary["gbnf_valid_sentences"] = valid_sentences

    if args.export_tsv:
        export_tsv(records, TSV_PATH)

    verify_tsv_parity(records, TSV_PATH)

    print("PASS: Gate 4 v1.2 expected_codes.json, override.gbnf (03 §4.1 parity), JSONL (120), and TSV parity verified.")
    print(json.dumps(summary, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
