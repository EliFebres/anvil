"""Checks an anvil audit log without anvil, by the same rules as `anvil audit verify`.

Usage: python verify_audit.py <audit.jsonl>
Prints "intact <records>" and exits 0, or "broken at line <n>: <reason>" and exits 1.

The rules are those of crates/anvil-audit/src/audit/chain.rs: the file is UTF-8
with LF line ends, every line ends with a newline, and each line is exactly the
canonical JSON of its record with ',"hash":"<sha256>"' before the closing brace.
"""

import hashlib
import json
import sys

GENESIS = "0" * 64
HASH_KEY = ',"hash":"'
TOP = ["seq", "ts", "session", "agent", "user", "host", "version", "commit",
       "binary_sha256", "policy", "event", "prev"]
U64, I32 = range(0, 2 ** 64), range(-2 ** 31, 2 ** 31)
EVENTS = {
    "session_start": [("mode", "str"), ("providers", "providers"), ("roles", "map"), ("flags", "map")],
    "prompt_submitted": [("content", "content")],
    "model_request": [("role", "str"), ("provider", "str"), ("host", "str"), ("model", "str"),
                      ("input_tokens", "u64"), ("output_tokens", "u64"), ("cached_tokens", "u64"),
                      ("stop_reason", "str"), ("redactions", "u64")],
    "tool_call": [("call_id", "str"), ("tool", "str"), ("arguments", "content")],
    "permission_decision": [("tool", "str"), ("decision", "str"), ("scope", "str"), ("source", "str")],
    "tool_result": [("call_id", "str"), ("tool", "str"), ("outcome", "str"), ("error", "bool"),
                    ("bytes", "u64"), ("sha256", "str"), ("truncated", "bool"), ("exit_code", "opt_i32")],
    "file_change": [("call_id", "str"), ("path", "str"), ("before", "opt_str"), ("after", "str")],
    "plan_approval": [("decision", "str"), ("answered_by", "str")],
    "policy_refusal": [("what", "str"), ("key", "str")],
    "session_end": [("outcome", "str")],
}


class Broken(Exception):
    pass


def no_duplicates(pairs):
    keys = [key for key, _ in pairs]
    if len(keys) != len(set(keys)):
        raise Broken("a key appears twice")
    return dict(pairs)


def is_int(value, allowed):
    return type(value) is int and value in allowed


def check(value, kind):
    if kind == "str":
        ok = type(value) is str
    elif kind == "u64":
        ok = is_int(value, U64)
    elif kind == "bool":
        ok = type(value) is bool
    elif kind == "opt_str":
        ok = value is None or type(value) is str
    elif kind == "opt_i32":
        ok = value is None or is_int(value, I32)
    elif kind == "map":
        ok = (type(value) is dict and list(value) == sorted(value)
              and all(type(item) is str for item in value.values()))
    elif kind == "providers":
        ok = type(value) is list and all(
            type(item) is dict and list(item) == ["name", "host", "model"]
            and all(type(field) is str for field in item.values()) for item in value)
    elif kind == "content":
        keys = list(value) if type(value) is dict else None
        ok = keys in (["bytes", "sha256"], ["bytes", "sha256", "text"]) and is_int(
            value["bytes"], U64) and type(value["sha256"]) is str and type(value.get("text", "")) is str
    else:
        ok = False
    if not ok:
        raise Broken("a field has the wrong type or layout")


def open_line(line):
    cut = line.rfind(HASH_KEY)
    if cut < 0:
        raise Broken("the record has no hash")
    digest = line[cut + len(HASH_KEY):]
    if not digest.endswith('"}'):
        raise Broken("the record's hash is not 64 hex digits at the end")
    digest = digest[:-2]
    if len(digest) != 64 or any(c not in "0123456789abcdefABCDEF" for c in digest):
        raise Broken("the record's hash is not 64 hex digits at the end")
    body = line[:cut] + "}"
    if hashlib.sha256(body.encode("utf-8")).hexdigest() != digest:
        raise Broken("the hash does not match the record: it was edited")
    try:
        record = json.loads(body, object_pairs_hook=no_duplicates)
    except ValueError as error:
        raise Broken(f"the record does not parse: {error}")
    if type(record) is not dict or list(record) != TOP:
        raise Broken("the record's fields are not the canonical ones in order")
    check(record["seq"], "u64")
    for key in TOP[1:10] + ["prev"]:
        check(record[key], "str")
    event = record["event"]
    tag = event.get("type") if type(event) is dict else None
    fields = EVENTS.get(tag) if type(tag) is str else None
    if fields is None or list(event) != ["type"] + [name for name, _ in fields]:
        raise Broken("the event's fields are not the canonical ones in order")
    for name, kind in fields:
        check(event[name], kind)
    if json.dumps(record, separators=(",", ":"), ensure_ascii=False) != body:
        raise Broken("the record is not in canonical form")
    return record, digest


def verify(data):
    segments = data.split(b"\n")
    count, prev, session = 0, GENESIS, None
    for number, raw in enumerate(segments[:-1], 1):
        try:
            line = raw.decode("utf-8")
            record, digest = open_line(line)
        except (Broken, UnicodeDecodeError) as error:
            raise Broken(f"line {number}: {error}")
        if record["seq"] != count or record["prev"] != prev:
            raise Broken(f"line {number}: a record was removed or moved")
        if session is None:
            session = record["session"]
        elif record["session"] != session:
            raise Broken(f"line {number}: the record belongs to another session")
        count, prev = count + 1, digest
    if segments[-1] != b"":
        raise Broken(f"line {len(segments)}: the last line has no newline")
    return count


if __name__ == "__main__":
    with open(sys.argv[1], "rb") as file:
        contents = file.read()
    try:
        print(f"intact {verify(contents)}")
    except Broken as error:
        print(f"broken at {error}")
        sys.exit(1)
