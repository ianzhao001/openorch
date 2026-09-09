#!/usr/bin/env python3
"""Classify a captured DSH session history without contacting its daemon."""

import argparse
import json
import math
import sys


# A stream with an event in the last minute is actively advancing. Longer quiet
# periods are called stalled, never dead, unless the caller supplies independent
# evidence that the daemon is not alive.
GENERATING_MAX_EVENT_AGE_SECS = 60.0


class HistoryError(ValueError):
    """Raised when the supplied session history has an unsupported shape."""


def _finite_number(value):
    try:
        parsed = float(value)
    except (TypeError, ValueError) as error:
        raise argparse.ArgumentTypeError("must be a finite number") from error
    if not math.isfinite(parsed):
        raise argparse.ArgumentTypeError("must be a finite number")
    return parsed


def _read_history(path):
    if path == "-":
        return json.load(sys.stdin)
    with open(path, "r", encoding="utf-8") as stream:
        return json.load(stream)


def _history_entries(history):
    if isinstance(history, list):
        return history
    if not isinstance(history, dict):
        raise HistoryError("session history must be an object or an event array")

    events = history.get("events")
    if isinstance(events, list):
        return events

    value = history.get("value")
    if isinstance(value, dict) and isinstance(value.get("events"), list):
        return value["events"]

    raise HistoryError("session history must contain an events array")


def _unwrap_event(entry, index):
    if not isinstance(entry, dict):
        raise HistoryError("event at index %d must be an object" % index)
    if "event" not in entry:
        return entry
    event = entry["event"]
    if not isinstance(event, dict):
        raise HistoryError("nested event at index %d must be an object" % index)
    return event


def _events(history):
    return [
        _unwrap_event(entry, index)
        for index, entry in enumerate(_history_entries(history))
    ]


def _event_time_ms(event):
    value = event.get("time")
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    value = float(value)
    return value if math.isfinite(value) else None


def _metadata_candidates(event):
    data = event.get("data")
    if not isinstance(data, dict):
        return []

    candidates = []
    header = data.get("header")
    if isinstance(header, dict):
        config = header.get("config")
        if isinstance(config, dict):
            candidates.append(config)
        candidates.append(header)

    context = data.get("context")
    if isinstance(context, dict):
        candidates.append(context)

    config = data.get("config")
    if isinstance(config, dict):
        candidates.append(config)
    candidates.append(data)
    return candidates


def _extract_metadata(events):
    result = {
        "provider": None,
        "model": None,
        "effort": None,
        "contextWindow": None,
    }
    field_names = {
        "provider": ("provider",),
        "model": ("model",),
        "effort": ("reasoningEffort", "effort"),
        "contextWindow": ("contextWindow",),
    }

    for event in events:
        if event.get("type") not in ("request/header", "request/context"):
            continue
        for candidate in _metadata_candidates(event):
            for output_name, input_names in field_names.items():
                for input_name in input_names:
                    if input_name in candidate:
                        result[output_name] = candidate[input_name]
                        break
    return result


def _json_seconds(value):
    if value is None:
        return None
    rounded = round(value, 3)
    return int(rounded) if rounded.is_integer() else rounded


def _classify(history, now_ms, daemon_alive):
    events = _events(history)
    kinds = [event.get("type") for event in events]
    timestamps = [
        timestamp
        for timestamp in (_event_time_ms(event) for event in events)
        if timestamp is not None
    ]
    last_time_ms = max(timestamps) if timestamps else None
    age_secs = (
        max(0.0, (now_ms - last_time_ms) / 1000.0)
        if last_time_ms is not None
        else None
    )

    turn_ended = "turn/end" in kinds
    if turn_ended:
        state = "converged"
    elif daemon_alive == "no":
        state = "dead"
    elif age_secs is not None and age_secs < GENERATING_MAX_EVENT_AGE_SECS:
        state = "generating"
    else:
        state = "stalled"

    output = {
        "state": state,
        "lastEventAgeSecs": _json_seconds(age_secs),
        "eventCount": len(events),
        "turnStarted": "turn/start" in kinds,
        "turnEnded": turn_ended,
        "toolCalls": kinds.count("tool/call"),
        "toolResults": kinds.count("tool/result"),
    }
    output.update(_extract_metadata(events))
    return output


def _parser():
    parser = argparse.ArgumentParser(
        description="Classify a captured DSH session.history JSON document."
    )
    parser.add_argument(
        "--history",
        default="-",
        metavar="PATH",
        help="history JSON file, or -/omitted for stdin",
    )
    parser.add_argument(
        "--now-ms",
        required=True,
        type=_finite_number,
        help="explicit current Unix timestamp in milliseconds",
    )
    parser.add_argument(
        "--daemon-alive",
        required=True,
        choices=("yes", "no", "unknown"),
        help="independent daemon-liveness evidence",
    )
    return parser


def main():
    parser = _parser()
    args = parser.parse_args()
    try:
        history = _read_history(args.history)
        output = _classify(history, args.now_ms, args.daemon_alive)
        encoded = json.dumps(
            output, ensure_ascii=False, separators=(",", ":"), allow_nan=False
        )
    except (HistoryError, OSError, json.JSONDecodeError, ValueError) as error:
        parser.error(str(error))
    print(encoded)


if __name__ == "__main__":
    main()
