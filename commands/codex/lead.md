---
description: Make this session the agent-bridge lead
---

The user makes this session the agent-bridge lead.

1. Call the agent-bridge tool `claim_lead` with `from` set to your canonical Codex mailbox (`codex-<full session uuid>`). The SessionStart hook gave you this mailbox.
2. Follow the `protocol` in the result for the rest of the session.
3. Tell the user in one line that you are the lead. If the result names a `previous` lead, say which agent it was.
