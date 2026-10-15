#!/usr/bin/env python3
"""Check that every relative link in the built book points to a file and, if given, an anchor.

Run after `mdbook build book`:  python3 book/check-links.py [book/build]
External links (http, https, mailto) are not fetched; the site is not on the network when CI runs.
"""
import sys
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import unquote, urlsplit

root = Path(sys.argv[1] if len(sys.argv) > 1 else "book/build").resolve()


class Page(HTMLParser):
    def __init__(self):
        super().__init__()
        self.links, self.ids = [], set()

    def handle_starttag(self, tag, attrs):
        a = dict(attrs)
        if "id" in a:
            self.ids.add(a["id"])
        if tag == "a" and a.get("name"):
            self.ids.add(a["name"])
        for key in ("href", "src"):
            if tag in ("a", "img", "link", "script") and a.get(key):
                self.links.append(a[key])


pages = {}
for path in sorted(root.rglob("*.html")):
    page = Page()
    page.feed(path.read_text(encoding="utf-8"))
    pages[path] = page

errors = []
for path, page in pages.items():
    for link in page.links:
        parts = urlsplit(link)
        if parts.scheme or parts.netloc:
            continue  # external
        if parts.path.startswith("/"):
            # Site-absolute: the book lives under /docs/, strip that prefix.
            rel = parts.path.removeprefix("/docs/")
            target = root / unquote(rel)
        elif parts.path:
            target = (path.parent / unquote(parts.path)).resolve()
        else:
            target = path
        if target.is_dir():
            target = target / "index.html"
        if not target.exists():
            errors.append(f"{path.relative_to(root)}: missing file for {link}")
            continue
        if parts.fragment and target.suffix == ".html":
            ids = pages[target].ids if target in pages else set()
            if unquote(parts.fragment) not in ids:
                errors.append(f"{path.relative_to(root)}: missing anchor in {link}")

if errors:
    print("\n".join(sorted(set(errors))))
    print(f"{len(set(errors))} broken link(s)")
    sys.exit(1)
print(f"ok: {sum(len(p.links) for p in pages.values())} links in {len(pages)} pages")
