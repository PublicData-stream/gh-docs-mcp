#!/usr/bin/env python3
"""Check local Markdown link destinations in the Git checkout."""

from html import unescape
from os import fsdecode
from pathlib import Path
import re
import subprocess
import sys
from unicodedata import category
from urllib.parse import unquote


FENCE = re.compile(r"^ {0,3}(`{3,}|~{3,})(.*)$")
HEADING = re.compile(r"^ {0,3}#{1,6}[ \t]+(.*?)(?:[ \t]+#+)?[ \t]*$")
UNDERLINE = re.compile(r"^ {0,3}(?:=+|-+)[ \t]*$")
CODE_SPAN = re.compile(r"(?<!`)(`+)(?!`).*?(?<!`)\1(?!`)")
DESTINATION = r"(?:<([^<>\n]*)>|((?:\\.|\([^()\s]*\)|[^\s()\\])+))"
TITLE = r"(?:[ \t]+(?:\"[^\"]*\"|'[^']*'|\([^)]*\)))?"
LABEL = r"((?:\\.|\[[^\[\]]*\]|[^\[\]\\])*)"
INLINE_LINK = re.compile(r"(?<!\\)\[" + LABEL + r"\]\(\s*" + DESTINATION + TITLE + r"\s*\)")
REFERENCE = re.compile(r"^ {0,3}\[(?!\^)[^\[\]]+\]:[ \t]*" + DESTINATION + TITLE + r"[ \t]*$")
EXTERNAL = re.compile(r"^(?:[a-zA-Z][a-zA-Z0-9+.-]*:|/)")


def markdown_lines(path):
    fence = None
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        marker = FENCE.match(line)
        if fence:
            if marker and marker[1].startswith(fence) and not marker[2].strip():
                fence = None
        elif marker:
            fence = marker[1]
            yield number, ""
        elif line.startswith(("    ", "\t")):
            yield number, ""
        else:
            yield number, line


def heading_anchors(lines):
    anchors = set()
    previous = ""
    for _, line in lines:
        heading = HEADING.match(line)
        if heading or (previous and UNDERLINE.fullmatch(line)):
            title = heading[1] if heading else previous
            title = INLINE_LINK.sub(r"\1", title)
            title = unescape(re.sub(r"<[^>]+>", "", title)).strip().lower()
            slug = "".join("-" if char.isspace() else char for char in title
                           if char.isspace() or char in "-_" or category(char)[0] in "LMN")
            anchor = slug
            suffix = 1
            while anchor in anchors:
                anchor = f"{slug}-{suffix}"
                suffix += 1
            anchors.add(anchor)
            previous = ""
        else:
            previous = line.strip()
    return anchors


def destinations(line):
    # Mask example links in inline code while keeping inline code in link labels.
    prose = CODE_SPAN.sub(lambda match: " " * len(match[0]), line)
    if reference := REFERENCE.match(prose):
        yield reference[1] if reference[1] is not None else reference[2]
    for match in INLINE_LINK.finditer(prose):
        yield match[2] if match[2] is not None else match[3]


def main():
    root = Path(fsdecode(subprocess.check_output(["git", "rev-parse", "--show-toplevel"]).rstrip(b"\n")))
    tracked = subprocess.check_output(["git", "ls-files", "-z"], cwd=root)
    files = {root / fsdecode(raw) for raw in tracked.split(b"\0") if raw}
    directories = {parent for path in files for parent in path.parents if parent.is_relative_to(root)}
    targets = files | directories
    documents = {path: list(markdown_lines(path)) for path in sorted(files)
                 if path.suffix.lower() == ".md" and path.is_file() and not path.is_symlink()}
    anchors = {path: heading_anchors(lines) for path, lines in documents.items()}
    failed = False
    for path, lines in documents.items():
        for number, line in lines:
            for destination in destinations(line):
                if EXTERNAL.match(destination):
                    continue
                local_path, _, fragment = destination.partition("#")
                local_path = unquote(local_path.split("?", 1)[0])
                local_path = re.sub(r"\\([^\w\s])", r"\1", local_path)
                target = (path.parent / local_path).resolve() if local_path else path
                if target not in targets or not target.exists():
                    reason = "missing tracked target"
                elif fragment and target in anchors and unquote(fragment) not in anchors[target]:
                    reason = "missing heading anchor"
                else:
                    continue
                print(f"{path.relative_to(root)}:{number}: {reason} {destination!r}", file=sys.stderr)
                failed = True
    return int(failed)


if __name__ == "__main__":
    sys.exit(main())
