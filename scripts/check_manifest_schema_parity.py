#!/usr/bin/env python3
"""Manifest schema parity checker for task-fabric-svn.

The task manifest schema is declared independently in several places that
must move together:

  containment  task_containment.py :: load_config
               (the validator the worker runs at launch; the supervisor
               and worker hold no key set of their own -- the worker
               delegates to this function)
  proxy        task_inference_proxy.py :: load_proxy_config
               (the proxy's own strict copy; the spend-gate boundary)
  producer     task_proxy_manifest.py :: build_manifest
               (the document the supervisor generates per task)

Nothing asserted those declarations agree. Adding the optional
``codemode_executable`` key to generated manifests wedged all task
dispatch in Oct 2026 because the proxy's copy of the allowed set did not
know the key ("proxy refusing to start: containment_config_invalid"; the
first real task became the proxy copy's integration test). The fix
(c1c08ef) kept the copies in sync by a comment. This checker turns that
comment into an assertion: it extracts each site's declared key sets from
source (AST, literal sets only) and fails -- naming the divergent key and
the sites that disagree -- when they drift.

Fail-closed throughout: a site whose declaration cannot be extracted
(missing file, missing function, non-literal set expression) is
UNRESOLVED and the run fails, never passes. The checker also scans the
other top-level scripts/*.py modules for a strict manifest key-set
declaration (a ``set(contract)`` comparison, or an assigned literal set
naming the manifest keys) outside the registered sites: a new validator
copy must register here and be in parity, not slip in unnoticed.

Exit codes: 0 parity verified / 1 DIVERGE / 2 UNRESOLVED (NOT verified).
Run on any commit that touches the manifest schema.
"""

from __future__ import annotations

import argparse
import ast
import sys
from dataclasses import dataclass, field
from pathlib import Path

# Registered declaration sites: (site name, file, function).
VALIDATOR_SITES = (
    ("containment", "task_containment.py", "load_config"),
    ("proxy", "task_inference_proxy.py", "load_proxy_config"),
)
PRODUCER_SITE = ("producer", "task_proxy_manifest.py", "build_manifest")

# The manifest top-level keys. A literal set naming at least three of
# these, anywhere else in scripts/*.py, smells like an unregistered
# validator copy.
SIGNATURE_KEYS = {"schema", "harness_executable", "proxy_socket", "contract"}


class Unresolved(Exception):
    """A site's key-set declaration could not be extracted."""


@dataclass
class ValidatorSets:
    site: str
    required: frozenset
    allowed: frozenset
    contract: frozenset


@dataclass
class ProducerSets:
    site: str
    always: frozenset       # top-level keys emitted unconditionally
    conditional: frozenset  # top-level keys emitted only when set
    contract: frozenset     # emitted contract keys


@dataclass
class Divergence:
    scope: str  # "top-level" or "contract"
    key: str
    detail: str


@dataclass
class Report:
    validators: list = field(default_factory=list)
    producer: ProducerSets | None = None
    divergences: list = field(default_factory=list)
    unresolved: list = field(default_factory=list)


# ---------------------------------------------------------------------------
# AST extraction helpers
# ---------------------------------------------------------------------------

def _literal_str_set(node):
    """The set of strings if node is a set literal of string constants."""
    if isinstance(node, ast.Set) and node.elts:
        values = set()
        for elt in node.elts:
            if not (isinstance(elt, ast.Constant) and isinstance(elt.value, str)):
                return None
            values.add(elt.value)
        return values
    return None


def _find_function(tree, name, site, filename):
    for node in ast.walk(tree):
        if isinstance(node, ast.FunctionDef) and node.name == name:
            return node
    raise Unresolved(f"{site}: function {name} not found in {filename}")


def _literal_assignments(func):
    """name -> set-of-strings for every `name = {<string literals>}` in func."""
    env = {}
    for node in ast.walk(func):
        if isinstance(node, ast.Assign) and len(node.targets) == 1 \
                and isinstance(node.targets[0], ast.Name):
            values = _literal_str_set(node.value)
            if values is not None:
                env[node.targets[0].id] = values
    return env


def _eval_set_expr(node, env, site, what):
    """Evaluate a set expression made of literals, names and `|` unions."""
    literal = _literal_str_set(node)
    if literal is not None:
        return set(literal)
    if isinstance(node, ast.Name):
        if node.id in env:
            return set(env[node.id])
        raise Unresolved(f"{site}: {what} references {node.id!r}, "
                         "which is not a literal set assignment")
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.BitOr):
        return _eval_set_expr(node.left, env, site, what) \
            | _eval_set_expr(node.right, env, site, what)
    raise Unresolved(f"{site}: {what} is not a literal set expression "
                     f"({node.__class__.__name__}); parity cannot be extracted")


def _is_set_call(node, arg_name):
    return isinstance(node, ast.Call) and isinstance(node.func, ast.Name) \
        and node.func.id == "set" and len(node.args) == 1 \
        and isinstance(node.args[0], ast.Name) and node.args[0].id == arg_name


def extract_validator(path: Path, site: str, func_name: str) -> ValidatorSets:
    if not path.is_file():
        raise Unresolved(f"{site}: {path} not found")
    try:
        tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
    except (OSError, SyntaxError) as e:
        raise Unresolved(f"{site}: cannot parse {path.name}: {e}") from e
    func = _find_function(tree, func_name, site, path.name)
    env = _literal_assignments(func)

    required = allowed = contract = None
    for node in ast.walk(func):
        if not isinstance(node, ast.Compare):
            continue
        operands = [node.left, *node.comparators]
        for i, operand in enumerate(operands):
            if _is_set_call(operand, "config"):
                # Shape: required <= set(config) <= <allowed expr>
                if i == 0 or i == len(operands) - 1:
                    raise Unresolved(
                        f"{site}: set(config) comparison has no required/"
                        "allowed neighbour to extract")
                required = _eval_set_expr(operands[i - 1], env, site,
                                          "required key set")
                allowed = _eval_set_expr(operands[i + 1], env, site,
                                         "allowed key set")
            if _is_set_call(operand, "contract"):
                others = [o for j, o in enumerate(operands) if j != i]
                literals = [s for s in (_literal_str_set(o) for o in others)
                            if s is not None]
                if len(literals) != 1:
                    raise Unresolved(
                        f"{site}: contract comparison does not pin exactly "
                        "one literal key set")
                if contract is not None and contract != literals[0]:
                    raise Unresolved(
                        f"{site}: multiple differing contract key sets in "
                        f"{func_name}; parity cannot be extracted")
                contract = literals[0]
    if required is None or allowed is None:
        raise Unresolved(f"{site}: no `required <= set(config) <= allowed` "
                         f"check found in {func_name}")
    if contract is None:
        raise Unresolved(f"{site}: no `set(contract)` exact-set check found "
                         f"in {func_name}")
    return ValidatorSets(site=site, required=frozenset(required),
                         allowed=frozenset(allowed),
                         contract=frozenset(contract))


def extract_producer(path: Path, site: str, func_name: str) -> ProducerSets:
    if not path.is_file():
        raise Unresolved(f"{site}: {path} not found")
    try:
        tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
    except (OSError, SyntaxError) as e:
        raise Unresolved(f"{site}: cannot parse {path.name}: {e}") from e
    func = _find_function(tree, func_name, site, path.name)

    always = conditional = contract = None
    for node in ast.walk(func):
        if isinstance(node, ast.Assign) and len(node.targets) == 1:
            target = node.targets[0]
            if isinstance(target, ast.Name) and target.id == "document" \
                    and isinstance(node.value, ast.Dict):
                keys, contract_node = set(), None
                for k, v in zip(node.value.keys, node.value.values):
                    if not (isinstance(k, ast.Constant)
                            and isinstance(k.value, str)):
                        raise Unresolved(
                            f"{site}: manifest document has a non-literal key")
                    keys.add(k.value)
                    if k.value == "contract":
                        contract_node = v
                if not isinstance(contract_node, ast.Dict):
                    raise Unresolved(
                        f"{site}: manifest 'contract' is not a dict literal")
                contract_keys = set()
                for k in contract_node.keys:
                    if not (isinstance(k, ast.Constant)
                            and isinstance(k.value, str)):
                        raise Unresolved(
                            f"{site}: contract has a non-literal key")
                    contract_keys.add(k.value)
                always, contract = keys, contract_keys
            elif isinstance(target, ast.Subscript) \
                    and isinstance(target.value, ast.Name) \
                    and target.value.id == "document" \
                    and isinstance(target.slice, ast.Constant) \
                    and isinstance(target.slice.value, str):
                if conditional is None:
                    conditional = set()
                conditional.add(target.slice.value)
    if always is None or contract is None:
        raise Unresolved(f"{site}: no manifest document dict literal found "
                         f"in {func_name}")
    return ProducerSets(site=site, always=frozenset(always),
                        conditional=frozenset(conditional or set()),
                        contract=frozenset(contract))


def find_unregistered_sites(scripts_dir: Path, own_name: str) -> list[str]:
    """Flag other scripts/*.py modules declaring a manifest key set."""
    known = {fname for _, fname, _ in (*VALIDATOR_SITES, PRODUCER_SITE)}
    flagged = []
    for path in sorted(scripts_dir.glob("*.py")):
        if path.name in known or path.name == own_name:
            continue
        try:
            tree = ast.parse(path.read_text(encoding="utf-8"),
                             filename=str(path))
        except (OSError, SyntaxError):
            continue  # not a parity question; other tooling owns syntax
        for node in ast.walk(tree):
            hit = False
            if isinstance(node, ast.Compare):
                operands = [node.left, *node.comparators]
                hit = any(_is_set_call(o, "contract") for o in operands)
            elif isinstance(node, ast.Assign):
                values = _literal_str_set(node.value)
                hit = values is not None \
                    and len(values & SIGNATURE_KEYS) >= 3
            if hit:
                flagged.append(path.name)
                break
    return flagged


# ---------------------------------------------------------------------------
# Parity assertions
# ---------------------------------------------------------------------------

def compare(report: Report) -> None:
    divs = report.divergences
    validators = report.validators

    for i, a in enumerate(validators):
        for b in validators[i + 1:]:
            for key in sorted(a.required ^ b.required):
                holder = a if key in a.required else b
                other = b if holder is a else a
                divs.append(Divergence(
                    "top-level", key,
                    f"{holder.site} requires it, {other.site} does not"))
            for key in sorted(a.allowed ^ b.allowed):
                holder = a if key in a.allowed else b
                other = b if holder is a else a
                divs.append(Divergence(
                    "top-level", key,
                    f"{holder.site} allows it, {other.site} rejects it"))
            for key in sorted(a.contract ^ b.contract):
                holder = a if key in a.contract else b
                other = b if holder is a else a
                divs.append(Divergence(
                    "contract", key,
                    f"in {holder.site}'s contract set, "
                    f"not in {other.site}'s"))

    producer = report.producer
    if producer is None:
        return
    emitted = producer.always | producer.conditional
    for v in validators:
        for key in sorted(v.required - producer.always):
            if key in producer.conditional:
                detail = (f"{v.site} requires it, but the producer emits "
                          "it only conditionally")
            else:
                detail = (f"{v.site} requires it, but the producer never "
                          "emits it")
            divs.append(Divergence("top-level", key, detail))
        for key in sorted(emitted - v.allowed):
            divs.append(Divergence(
                "top-level", key,
                f"producer emits it, {v.site} rejects it"))
        for key in sorted(producer.contract ^ v.contract):
            if key in producer.contract:
                detail = (f"producer emits it in the contract, "
                          f"{v.site} rejects it")
            else:
                detail = (f"{v.site}'s contract set requires it, "
                          "the producer never emits it")
            divs.append(Divergence("contract", key, detail))


def run(scripts_dir: Path, own_name: str) -> Report:
    report = Report()
    for site, fname, func_name in VALIDATOR_SITES:
        try:
            report.validators.append(
                extract_validator(scripts_dir / fname, site, func_name))
        except Unresolved as e:
            report.unresolved.append(str(e))
    site, fname, func_name = PRODUCER_SITE
    try:
        report.producer = extract_producer(scripts_dir / fname, site,
                                           func_name)
    except Unresolved as e:
        report.unresolved.append(str(e))
    for fname in find_unregistered_sites(scripts_dir, own_name):
        report.unresolved.append(
            f"unregistered validator key-set declaration in {fname}: "
            "register the site in check_manifest_schema_parity.py and "
            "bring it into parity")
    if not report.unresolved:
        compare(report)
    return report


def _fmt(keys) -> str:
    return "{" + ", ".join(sorted(keys)) + "}"


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(
        description="Assert every task-manifest schema declaration "
                    "(containment, proxy, producer) agrees on the key sets.")
    parser.add_argument("--scripts-dir", type=Path,
                        default=Path(__file__).resolve().parent,
                        help="directory holding the task-fabric scripts "
                             "(default: this script's directory)")
    args = parser.parse_args(argv)

    report = run(args.scripts_dir, Path(__file__).name)
    for v in report.validators:
        print(f"{v.site}: required={_fmt(v.required)} "
              f"allowed={_fmt(v.allowed)} contract={_fmt(v.contract)}")
    if report.producer is not None:
        p = report.producer
        print(f"{p.site}: always-emitted={_fmt(p.always)} "
              f"conditional={_fmt(p.conditional)} "
              f"contract={_fmt(p.contract)}")

    if report.unresolved:
        for problem in report.unresolved:
            print(f"UNRESOLVED: {problem}")
        print("manifest schema parity NOT verified (extraction failed)")
        return 2
    if report.divergences:
        for d in report.divergences:
            print(f"DIVERGE: {d.scope} key {d.key!r}: {d.detail}")
        print("validator sets DIVERGE — NOT verified")
        return 1
    print("manifest schema parity verified: containment, proxy and "
          "producer agree on the top-level and contract key sets")
    return 0


if __name__ == "__main__":
    sys.exit(main())
