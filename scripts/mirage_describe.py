"""Describe one image with the locally provisioned Cactus Mirage model.

Used as a sidecar by igris-core::vision: prints a short plain-text summary to
stdout so LFM3/Qwen can reason about the image through the normal chat path.
The raw Mirage result is a large dict (masks, boxes) that would blow the
8k-token context, so only high-signal fields are kept and bounded.
"""
import json
import os
import sys
import warnings

warnings.filterwarnings("ignore")


def summarize(result: dict) -> str:
    lines = []
    scene = result.get("scene") or {}
    if scene:
        lines.append(
            "Scene: {}, {}, {}.".format(
                scene.get("setting", "unknown"),
                scene.get("background", "unknown"),
                scene.get("style", "unknown"),
            )
        )
    objects = result.get("objects") or []
    if objects:
        names = []
        for obj in objects[:12]:
            name = obj.get("name", "object")
            pos = (obj.get("position") or "").strip()
            names.append("{}{}".format(name, " ({})".format(pos) if pos else ""))
        lines.append("Objects: {}.".format(", ".join(names)))
    regions = result.get("regions") or []
    if regions:
        lines.append(
            "Regions: {}.".format(
                ", ".join(str(r.get("name", "region")) for r in regions[:6])
            )
        )
    texts = result.get("text") or []
    if texts:
        strings = []
        for t in texts[:20]:
            s = (t.get("string") or "").strip()
            if s and s not in strings:
                strings.append(s)
        lines.append("Text in image: {}.".format(" | ".join(strings)))
    relations = result.get("relations") or []
    if relations:
        lines.append("{} spatial relation(s) detected.".format(len(relations)))
    reasoning = (result.get("reasoning") or "").strip()
    if reasoning:
        lines.append("Summary: {}".format(reasoning[:600]))
    if not lines:
        lines.append("No structured detail was extracted from this image.")
    return "\n".join(lines)


def locate(result: dict, query: str):
    """Find the best detected object/text matching QUERY and return its centre."""
    words = [w for w in query.lower().split() if len(w) > 1]
    candidates = []
    for t in result.get("text") or []:
        s = t.get("string") or ""
        if any(w in s.lower() for w in words):
            candidates.append((float(t.get("score", 0)), "text: " + s, t.get("bbox")))
    for o in result.get("objects") or []:
        n = o.get("name") or ""
        if any(w in n.lower() for w in words):
            candidates.append((float(o.get("score", 0)), "object: " + n, o.get("bbox")))
    if not candidates:
        return None
    candidates.sort(key=lambda c: c[0], reverse=True)
    score, name, bbox = candidates[0]
    if not bbox or len(bbox) < 4:
        return None
    x1, y1, x2, y2 = (float(v) for v in bbox[:4])
    return {
        "x": (x1 + x2) / 2.0,
        "y": (y1 + y2) / 2.0,
        "name": name,
        "score": score,
        "bbox": [x1, y1, x2, y2],
    }


def main() -> int:
    locate_mode = len(sys.argv) > 1 and sys.argv[1] == "locate"
    if locate_mode:
        if len(sys.argv) < 5:
            print("usage: mirage_describe.py locate IMAGE WEIGHTS QUERY", file=sys.stderr)
            return 2
        image, weights, query = sys.argv[2], sys.argv[3], sys.argv[4]
    elif len(sys.argv) < 3:
        print("usage: mirage_describe.py IMAGE WEIGHTS", file=sys.stderr)
        return 2
    else:
        image, weights, query = sys.argv[1], sys.argv[2], None
    if not os.path.isfile(image):
        print("image not found: " + image, file=sys.stderr)
        return 2
    if not os.path.isfile(weights):
        print("weights not found: " + weights, file=sys.stderr)
        return 2
    try:
        import cactus
    except Exception as exc:
        print("cactus-compute is not installed: " + str(exc), file=sys.stderr)
        return 3
    try:
        model = cactus.Mirage(weights=weights)
        result = model.describe(image)
        if locate_mode:
            found = locate(result, query) if isinstance(result, dict) else None
            print(json.dumps(found or {}, ensure_ascii=True))
        elif isinstance(result, str):
            print(result)
        else:
            print(summarize(result))
        return 0
    except Exception as exc:
        print("mirage inference failed: " + str(exc), file=sys.stderr)
        return 4


if __name__ == "__main__":
    raise SystemExit(main())
