#!/usr/bin/env python3
"""Check that a release's two note bodies do not contradict each other.

The plain-text notes reach the Sparkle dialog, appcast.xml and latest.json;
the markdown reaches the GitHub release page. They are written separately
because those audiences need different shapes, which leaves room for the same
change to be described with different figures in each. That is the failure this
guards: a reader comparing the update dialog against the release page should
not find two different numbers for one measurement.

The rule is one-directional. Every number in the .txt must appear in the .md,
not the reverse: the markdown legitimately carries figures the dialog omits,
such as table contents or a longer worked example.

Numbers are compared after folding the glyphs the two files spell differently.
The v1.17.0 pair writes the same values as `1.25x` / `-5.9%` / `18-28` in the
text and `1.25x` / `-5.9%` / `18-28` in the markdown using U+00D7, U+2212 and
U+2013 respectively, so a comparison over raw characters would report a
contradiction that is not there. Thousands separators are stripped for the
same reason (`1,144` against `1144`).
"""
import re
import sys
from pathlib import Path

FOLD = str.maketrans({"−": "-", "–": "-", "—": "-", "×": "x"})

# The sign is part of the value: the whole point of this check is to catch the
# two files disagreeing about a measurement, and "cost falls 5.9%" against
# "cost rises 5.9%" is exactly that disagreement. Dropping the sign would let
# it through, and these notes routinely carry signed percentages.
#
# The lookbehind is what keeps a range from reading as a negative. After
# folding, "18-28 seconds" and "1-4 seconds" would otherwise yield -28 and -4;
# a hyphen only counts as a sign when what precedes it is not part of a word or
# number. A leading "+" is accepted for symmetry but does not occur today.
NUMBER = re.compile(r"(?<![\w.])[-+]?\d+(?:\.\d+)?")

# Identifiers are not measurements. The markdown cites issues and pull requests
# as `[#287](https://github.com/.../pull/287)`, which puts 287 into its number
# set twice over — once from the link text, once from the URL path, where the
# preceding `/` does not stop the lookbehind. A wrong figure in the plain text
# would then be reported as present merely because some PR happens to carry
# that number. On the committed v1.17.0 markdown, seven of its twenty-four
# distinct numbers come only from links (286, 287, 288, 289, 293, 296, 300);
# removing them leaves exactly the seventeen the plain text also states.
#
# Contributor handles need no special case: `@Mai0313` is already excluded,
# because the digits are preceded by a word character.
INLINE_LINK = re.compile(r"\[([^\]]*)\]\([^)]*\)")
BARE_URL = re.compile(r"https?://\S+")
ISSUE_REF = re.compile(r"#\d+")


def numbers(path: Path) -> list[str]:
    text = path.read_text(encoding="utf-8")
    text = INLINE_LINK.sub(r"\1", text)   # keep the link text, drop the target
    text = BARE_URL.sub(" ", text)
    text = ISSUE_REF.sub(" ", text)
    return NUMBER.findall(text.translate(FOLD).replace(",", ""))


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: check_release_notes.py <tag>", file=sys.stderr)
        return 2
    tag = sys.argv[1]
    txt_path = Path("release-notes") / f"{tag}.txt"
    md_path = Path("release-notes") / f"{tag}.md"

    missing = [p for p in (txt_path, md_path) if not p.is_file()]
    if missing:
        for p in missing:
            print(f"error: missing {p}", file=sys.stderr)
        return 1

    # An empty file is not a release with nothing to say; it is a file someone
    # created to get past the existence check. Both would ship: the appcast
    # renderer skips its description when the text is empty, and latest.json
    # would carry empty notes.
    empty = [p for p in (txt_path, md_path) if not p.read_text(encoding="utf-8").strip()]
    if empty:
        for p in empty:
            print(f"error: {p} is empty", file=sys.stderr)
        return 1

    txt = numbers(txt_path)
    md = set(numbers(md_path))
    absent = sorted({n for n in txt if n not in md}, key=float)

    if absent:
        print(f"error: {len(absent)} number(s) in {txt_path} do not appear in {md_path}:",
              file=sys.stderr)
        for n in absent:
            for line in txt_path.read_text(encoding="utf-8").splitlines():
                if n in line.translate(FOLD).replace(",", ""):
                    print(f"  {n}  <- {line.strip()[:100]}", file=sys.stderr)
                    break
            else:
                print(f"  {n}", file=sys.stderr)
        print("The Sparkle dialog would state a figure the release page does not.",
              file=sys.stderr)
        return 1

    print(f"{txt_path}: {len(txt)} numbers ({len(set(txt))} distinct), all present in {md_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
