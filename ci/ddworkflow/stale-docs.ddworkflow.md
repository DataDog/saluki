---
name: Stale Comments Cleaner
description: Clean up comments, paths and docs that have gone stale.
schedule:
  - cron: "0 9 * * *"    # run every day at 9:00 AM UTC
repo: DataDog/saluki
branch: main
pull_request: draft      # open | draft | none  (default: draft)
model: claude-sonnet    # optional LLM override (default: auto)
notifications:
  slack:
    channel: "#agent-data-plane"
    on: [pr_created]     # [pr_created, session_started, session_failed]
---
- Find code comments in `bin/` and `lib/` that appear provably untrue and fix them to make them
  true.
- Check `*.md` file contents and update them if the code has drifted from what they say.
- Check file path references in code comments to make sure they are not stale, update them if they
  are.
- Check symbol references in Rust docs and make sure they resolve correctly.
- Delete TODOs that appear to have already been resolved.

When opening your PR, please use `.github/PULL_REQUEST_TEMPLATE.md`
