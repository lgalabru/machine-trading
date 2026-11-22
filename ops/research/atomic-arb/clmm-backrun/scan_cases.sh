#!/bin/sh
# scan slot-8..slot+3 around each case slot
cd "$(dirname "$0")"
for s in $(cat ../data/case_slots.txt); do
  python3 -I blockscan.py ../data/cases.jsonl $((s-8)) $((s+3)) 4
done
