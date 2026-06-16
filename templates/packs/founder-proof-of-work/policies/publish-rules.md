# Publish Rules

Default: private.

Publish only when all are true:

- `review_status: reviewed`
- no secrets, credentials, private customer names, or private revenue details
- source evidence is local and available
- the claim is specific enough to defend

Never publish:

- bearer tokens or credential references
- raw customer conversations without explicit permission
- private workspace names unless already public
- unreviewed agent-generated claims
