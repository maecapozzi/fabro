#!/usr/bin/env python3
"""Summarize observed Fabro model, tool, lifecycle, and stage events."""

import argparse
import collections
import json
from pathlib import Path


parser = argparse.ArgumentParser(
    description="Count observed Fabro events without inferring missing coverage."
)
parser.add_argument("events")
args = parser.parse_args()

event_path = Path(args.events)
rows = [
    json.loads(line)
    for line in event_path.read_text().splitlines()
    if line.strip()
]
seen = set()
counts = collections.defaultdict(collections.Counter)
visits = collections.defaultdict(set)
tools = collections.defaultdict(collections.Counter)
tool_events = collections.defaultdict(list)
stage_timings = collections.defaultdict(list)
stage_starts = {}
lifecycles = []
run_ids = set()
sequences = []

for row in rows:
    run_id = row.get("run_id")
    sequence = row.get("stream_seq")
    key = (run_id, sequence)
    if sequence is not None and key in seen:
        continue
    seen.add(key)
    if run_id:
        run_ids.add(run_id)
    if sequence is not None:
        sequences.append(sequence)

    item = row.get("item", {})
    platform = item.get("record", {})
    if row.get("kind") == "platform" and platform.get("kind") == "run.lifecycle":
        lifecycles.append(
            {
                "at_ms": row.get("recorded_at"),
                "transition": platform.get("transition"),
                "status": platform.get("status"),
            }
        )

    subject = item.get("subject", {})
    stage = subject.get("node", {}).get("name")
    if not stage:
        continue
    visit_key = (
        stage,
        subject.get("visit"),
        subject.get("firing"),
        item.get("context", {}).get("execution"),
    )
    derived = item.get("derived", {})
    if derived.get("event") == "visit.started":
        stage_starts[visit_key] = row.get("recorded_at")
    if derived.get("event") == "visit.completed":
        metrics = derived.get("outcome", {}).get("metrics", {})
        custom = metrics.get("custom", {})
        stage_timings[stage].append(
            {
                "visit": subject.get("visit"),
                "started_at_ms": stage_starts.get(visit_key),
                "completed_at_ms": row.get("recorded_at"),
                "duration_ms": metrics.get("duration_ms"),
                "inference_ms": custom.get("pebble.inference_ms"),
                "tool_ms": custom.get("pebble.tool_ms"),
                "status": derived.get("outcome", {}).get("status"),
            }
        )

    event = (
        item.get("record", {})
        .get("body", {})
        .get("ev", {})
        .get("custom", {})
        .get("event", {})
        .get("event", {})
    )
    if isinstance(event, str):
        name, value = event, {}
    elif isinstance(event, dict) and len(event) == 1:
        name, value = next(iter(event.items()))
    else:
        continue

    visits[stage].add(subject.get("visit"))
    if name in (
        "LlmRequestStarted",
        "AssistantMessage",
        "RoundInterrupted",
        "ToolCallStarted",
        "ToolCallCompleted",
    ):
        counts[stage][name] += 1
    if name in ("ToolCallStarted", "ToolCallCompleted") and isinstance(value, dict):
        tool_name = value.get("tool_name", "<unknown>")
        phase = "started" if name == "ToolCallStarted" else "completed"
        tool_events[stage].append(
            {
                "at_ms": row.get("recorded_at"),
                "name": tool_name,
                "phase": phase,
                "is_error": value.get("is_error") if phase == "completed" else None,
            }
        )
        if phase == "started":
            tools[stage][tool_name] += 1

unique_sequences = sorted(set(sequences))
sequence_gaps = [
    [left, right]
    for left, right in zip(unique_sequences, unique_sequences[1:])
    if right != left + 1
]

print(
    json.dumps(
        {
            "source": str(event_path.resolve()),
            "run_ids": sorted(run_ids),
            "coverage": {
                "first_observed_sequence": min(sequences) if sequences else None,
                "last_observed_sequence": max(sequences) if sequences else None,
                "unique_observed_records": len(seen),
                "sequence_gaps": sequence_gaps,
                "complete_run_not_assumed": True,
            },
            "lifecycle_transitions": lifecycles,
            "stage_timings": dict(stage_timings),
            "tool_events": dict(tool_events),
            "stages": {
                stage: {
                    "visits_with_agent_events": len(visits[stage]),
                    "observed_events": dict(counts[stage]),
                    "tools": dict(tools[stage]),
                }
                for stage in sorted(counts)
            },
        },
        indent=2,
    )
)
