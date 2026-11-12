---
name: Stale Comments Cleaner
description: Clean up comments, paths and docs that have gone stale.
schedule:
  - cron: "0 9 * * *"    # run every day at 9:00 AM UTC
repo: DataDog/saluki
branch: main
pull_request: draft      # open | draft | none  (default: draft)
model: claude-sonnet    # optional LLM override (default: auto)
team: agent-data-plane
notifications:
  slack:
    channel: "#agent-data-plane"
    # DataDog/saluki is public, so Bits Code does not open the PR itself and `pr_created` does not
    # fire. `session_completed` posts the agent's summary and a link to the session.
    on: [session_completed, session_failed]
---
- Find code comments in `bin/` and `lib/` that appear provably untrue and fix them to make them
  true.
- Check `*.md` file contents and update them if the code has drifted from what they say.
- Check file path references in code comments to make sure they are not stale, update them if they
  are.
- Check symbol references in Rust docs and make sure they resolve correctly.
- Delete TODOs that appear to have already been resolved.

When opening your PR, please use `.github/PULL_REQUEST_TEMPLATE.md`

Finish with a short summary of what you changed. If nothing was stale, say so.
