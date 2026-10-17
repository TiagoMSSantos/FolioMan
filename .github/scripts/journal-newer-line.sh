# shellcheck shell=bash
# (#637) pages.yml's `record` job SOURCES this (`. .github/scripts/journal-newer-line.sh`), never runs it:
# `day` is read by the commit below the call, and the early `exit 0` is the step's own. It was inline
# until the actionlint job in ci.yml needed to run it on five journals (a newer day appends exactly
# one line; the same day, an older day, an empty main file and a missing one are pinned too), because a
# bad edit here reverts the forward record's one-line-a-day journal and nothing red says so. It runs
# from the checkout root under the step's `bash -e` (no pipefail): an empty or missing main journal
# reads as last="", so the run's line is appended.
#
# (#605) APPEND THE RUN'S LAST LINE, NEVER COPY ITS FILE. The run's journal comes from its
# own checkout, which can be older than main: a queued run checks out the SHA it was
# triggered on, and a re-run reuses its original SHA. Copied whole, that file reverts every
# line main gained since. 210c9dc did it to 9379bd8's 2026-10-16 line (two runs queued on
# 32d2ec4); a failed run re-run a day later would have dropped whole days. Only a line dated
# after main's last one is new.
line=$(tail -n 1 run-journal/.screen_snapshots.jsonl)
# The date comes from the line itself, not from `date`, so a run crossing midnight UTC
# cannot label a snapshot with a day it was not taken on.
day=$(jq -r .date <<<"$line")
last=$(tail -n 1 .screen_snapshots.jsonl | jq -r .date)
if [[ ! "$day" > "$last" ]]; then
  echo "journal unchanged: main already ends at $last (this run's last line: $day)"
  exit 0
fi
printf '%s\n' "$line" >> .screen_snapshots.jsonl
