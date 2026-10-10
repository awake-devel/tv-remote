#!/usr/bin/env bash
# Publish docs/*.md to the GitHub wiki (https://github.com/<owner>/<repo>/wiki).
#
# docs/ is the source of truth. The wiki uses different conventions, so each page is
# converted: docs/README.md becomes Home.md, other pages become Title-Case names
# (casting.md -> Casting.md), and relative links lose their ".md" and point at the
# wiki page names. Links to other repo files (e.g. smartthings-app.json) become
# absolute links to the main branch.
#
# Usage: scripts/publish-wiki.sh [--dry-run]
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
dry_run=0
[[ ${1:-} == --dry-run ]] && dry_run=1

remote=$(git -C "$root" remote get-url origin)
remote=${remote%.git}
slug=${remote#*github.com[:/]}
wiki_url="https://github.com/${slug}.wiki.git"
blob_base="https://github.com/${slug}/blob/main/docs"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
git clone --quiet "$wiki_url" "$work/wiki"

# Remove existing pages so deleted docs disappear from the wiki too (keeps .git).
find "$work/wiki" -mindepth 1 -maxdepth 1 ! -name .git -exec rm -rf {} +

python3 -I - "$root/docs" "$work/wiki" "$blob_base" <<'EOF'
import os, re, sys

src, dst, blob_base = sys.argv[1], sys.argv[2], sys.argv[3]

# Words whose capitalisation .capitalize() would get wrong.
SPECIAL = {"faq": "FAQ", "cli": "CLI", "smartthings": "SmartThings", "tvs": "TVs", "tv": "TV"}

def page_name(md):
    stem = md[:-3]
    if stem.lower() == "readme":
        return "Home"
    return "-".join(SPECIAL.get(w.lower(), w.capitalize()) for w in re.split(r"[-_ ]+", stem))

pages = {f: page_name(f) for f in os.listdir(src) if f.endswith(".md")}

link = re.compile(r"\]\((?!https?://|mailto:|#)([^)#\s]+)(#[^)\s]*)?\)")

def fix(m):
    target, anchor = m.group(1), m.group(2) or ""
    base = os.path.basename(target)
    if base in pages:
        return f"]({pages[base]}{anchor})"
    # Any other repo file: link to it on GitHub.
    return f"]({blob_base}/{target}{anchor})"

for f, name in pages.items():
    text = open(os.path.join(src, f), encoding="utf-8").read()
    open(os.path.join(dst, name + ".md"), "w", encoding="utf-8").write(link.sub(fix, text))

# Sidebar: every page, Home first.
order = sorted(pages.values(), key=lambda n: (n != "Home", n))
with open(os.path.join(dst, "_Sidebar.md"), "w", encoding="utf-8") as s:
    s.write("**tv-remote**\n\n" + "\n".join(f"- [{n.replace('-', ' ')}]({n})" for n in order) + "\n")
print("pages:", ", ".join(order))
EOF

cd "$work/wiki"
git add -A
if git diff --cached --quiet; then
    echo "Wiki already up to date."
    exit 0
fi
git commit --quiet -m "Sync from docs/ ($(git -C "$root" rev-parse --short HEAD))"
if (( dry_run )); then
    git show --stat --oneline HEAD | head -30
    echo "(dry run: not pushed)"
else
    git push --quiet origin HEAD
    echo "Published to https://github.com/${slug}/wiki"
fi
