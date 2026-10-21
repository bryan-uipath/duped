#!/usr/bin/env sh
# Score every first-parent commit of a repo and rank the PRs that moved the score.
# usage: coupling-history.sh <repo> <out-dir> [rev-list args, e.g. --since=90.days develop]
set -eu
repo=$1; out=$2; shift 2
mkdir -p "$out"
git -C "$repo" rev-list --first-parent --reverse "$@" > "$out/commits"
n=$(wc -l < "$out/commits" | tr -d ' ')
i=0
while read -r sha; do
  i=$((i + 1))
  [ -s "$out/$sha.json" ] && continue   # resumable
  duped coupling "$repo" --rev "$sha" --json --top 100000 > "$out/$sha.json.tmp" 2> "$out/$sha.err" \
    && mv "$out/$sha.json.tmp" "$out/$sha.json" || echo "fail $sha" >&2
  printf '\r%d/%d' "$i" "$n" >&2
done < "$out/commits"
echo >&2
# One row per commit: sha, score, bypassRate, unusedSurface, subject.
: > "$out/history.tsv"
while read -r sha; do
  [ -s "$out/$sha.json" ] || continue
  printf '%s\t%s\t%s\n' "$sha" \
    "$(jq -r '.totals | [.score, .bypassRate, .unusedSurface, .bypassSymbols, .crossBoundarySymbols] | @tsv' "$out/$sha.json")" \
    "$(git -C "$repo" log -1 --format=%s "$sha")" >> "$out/history.tsv"
done < "$out/commits"
# Deltas between consecutive commits, biggest movers first.
awk -F'\t' 'NR>1 { d=$2-ps; printf "%+.4f\t%+.4f\t%s\t%s\n", d, $3-pr, substr($1,1,10), $7 } { ps=$2; pr=$3 }' \
  "$out/history.tsv" | sort -t$'\t' -k1,1g > "$out/deltas.tsv"
echo "wrote $out/history.tsv and $out/deltas.tsv (score delta, bypassRate delta, sha, subject)"
