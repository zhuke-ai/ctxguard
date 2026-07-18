"""Pre-populate /tmp/demo-session.jsonl with realistic assistant turns."""
import json
from datetime import datetime, timezone


def turn(i, cache_read):
    return {
        "parentUuid": "demo",
        "isSidechain": False,
        "promptId": f"demo-{i}",
        "agentId": None,
        "type": "assistant",
        "message": {
            "id": f"msg-{i}",
            "type": "message",
            "role": "assistant",
            "model": "claude-opus-4-8[1m]",
            "content": [{"type": "text", "text": f"turn {i}"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 80,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": cache_read,
            },
        },
        "uuid": f"uuid-{i}",
        "timestamp": datetime(2026, 7, 14, 3, i, 0, tzinfo=timezone.utc).isoformat().replace("+00:00", "Z"),
    }


if __name__ == "__main__":
    import sys
    out = sys.argv[1]
    with open(out, "w") as f:
        for i in range(6):
            f.write(json.dumps(turn(i + 1, 100 * (i + 1) ** 2)) + "\n")
    print(f"wrote {i+1} turns to {out}")
