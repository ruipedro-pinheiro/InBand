---
description: Make this session the inband lead
---

The user makes this session the inband lead.

1. Call the inband tool `claim_lead` with `from` set to your mailbox for this session. The SessionStart hook gave you this mailbox.
2. Follow the `protocol` in the result for the rest of the session.
3. Tell the user in one line that you are the lead. If the result names a `previous` lead, say which agent it was.
